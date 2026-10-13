// Copyright © 2026 Mozilla Foundation
// SPDX-License-Identifier: ISC

use super::*;

// Miri also runs these fixtures to diagnose invalid accesses if the codec regresses.
#[test]
fn scm_rights_encode_reserves_a_short_tail() {
    let payload_len = mem::size_of::<RawFd>();
    let record_size = space(payload_len);
    // Leave too little room for the payload itself, not just trailing padding:
    // on Linux, CMSG_SPACE is larger than CMSG_LEN for a single descriptor.
    let available = len(payload_len) - 1;
    let mut buf = BytesMut::zeroed(available);
    buf.clear();
    assert_eq!(buf.capacity(), available);
    encode_handles(&mut buf, &[123]);
    assert_eq!(buf.len(), record_size);
    assert_eq!(&decode(&buf)[..], &[123]);
}

// Linux's 64-bit ABI has four bytes of trailing padding for one descriptor.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[test]
fn scm_rights_zeroes_trailing_padding() {
    let payload_len = mem::size_of::<RawFd>();
    let mut buf = BytesMut::from(&vec![0xa5; space(payload_len)][..]);
    buf.clear();
    encode_handles(&mut buf, &[123]);
    assert!(space(payload_len) > len(payload_len));
    assert!(buf[len(payload_len)..].iter().all(|&byte| byte == 0));
}

// Miri detects uninitialized bytes in fresh storage; a native value check cannot.
#[cfg(all(miri, target_os = "linux", target_pointer_width = "64"))]
#[test]
fn scm_rights_initializes_fresh_padding() {
    let payload_len = mem::size_of::<RawFd>();
    let mut buf = BytesMut::with_capacity(space(payload_len));
    encode_handles(&mut buf, &[123]);
    assert!(space(payload_len) > len(payload_len));
    // Reading every byte makes Miri check initialization, not just its value.
    for byte in buf.iter() {
        std::hint::black_box(*byte);
    }
}

// These fixtures exercise the byte codec only. The synthetic descriptor values
// are never passed to a syscall or placed in an owning handle wrapper.
fn record(fd: RawFd) -> BytesMut {
    let mut buf = BytesMut::zeroed(space(mem::size_of::<RawFd>()));
    buf.clear();
    encode_handles(&mut buf, &[fd]);
    buf
}

#[test]
// Keep the arithmetic compatible with older Rust toolchains.
#[allow(clippy::manual_is_multiple_of)]
fn scm_rights_decodes_unaligned_storage() {
    use bytes::Buf;

    let encoded = record(123);
    let mut buf = BytesMut::zeroed(encoded.len() + mem::align_of::<cmsghdr>());
    let offset = (0..mem::align_of::<cmsghdr>())
        .find(|&offset| (buf.as_ptr() as usize + offset) % mem::align_of::<cmsghdr>() != 0)
        .unwrap();
    buf[offset..offset + encoded.len()].copy_from_slice(&encoded);
    buf.advance(offset);
    buf.truncate(encoded.len());
    assert_eq!(&decode(&buf)[..], &[123]);
}

#[test]
fn scm_rights_decodes_all_control_records() {
    let mut buf = record(123);
    buf.extend_from_slice(&record(456));
    assert_eq!(&decode(&buf)[..], &[123, 456]);
}

#[test]
fn scm_rights_skips_unrelated_record_before_rights() {
    let mut buf = record(123);
    // An unrelated, well-formed control record must not hide subsequent rights.
    unsafe {
        let header = buf.as_mut_ptr().cast::<cmsghdr>();
        std::ptr::addr_of_mut!((*header).cmsg_type).write_unaligned(-1);
    }
    buf.extend_from_slice(&record(456));
    assert_eq!(&decode(&buf)[..], &[456]);
}

fn decode(buf: &[u8]) -> Vec<RawFd> {
    let mut fds = Vec::new();
    decode_handles(buf, |fd| fds.push(fd)).unwrap();
    fds
}

#[test]
fn scm_rights_truncated_record_reports_delivered_descriptors() {
    let mut buf = record(123);
    unsafe {
        let header = buf.as_mut_ptr().cast::<cmsghdr>();
        std::ptr::addr_of_mut!((*header).cmsg_len)
            .write_unaligned(len(3 * mem::size_of::<RawFd>()) as _);
    }
    buf.truncate(len(mem::size_of::<RawFd>()));
    let mut fds = Vec::new();
    let error = decode_handles(&buf, |fd| fds.push(fd)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fds, [123]);
}

#[test]
fn scm_rights_rejects_invalid_control_lengths() {
    for size in [0, len(0) - 1] {
        let mut buf = record(123);
        unsafe {
            let header = buf.as_mut_ptr().cast::<cmsghdr>();
            std::ptr::addr_of_mut!((*header).cmsg_len).write_unaligned(size as _);
        }
        let error =
            decode_handles(&buf, |_| panic!("invalid record delivered a descriptor")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
