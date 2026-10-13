// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use bytes::{BufMut, BytesMut};
use libc::cmsghdr;
use std::convert::TryInto;
use std::os::unix::io::RawFd;
use std::{io, mem};

#[cfg(test)]
#[path = "cmsg_tests.rs"]
mod tests;

/// Encode descriptor values into an SCM_RIGHTS control record.
pub fn encode_handles(cmsg: &mut BytesMut, handles: &[RawFd]) {
    let payload_len = mem::size_of_val(handles);

    let cmsg_space = space(payload_len);
    cmsg.reserve(cmsg_space);

    unsafe {
        let cmsghdr_ptr = cmsg.chunk_mut().as_mut_ptr();
        std::ptr::write_bytes(cmsghdr_ptr, 0, cmsg_space);
        let header = cmsghdr_ptr.cast::<cmsghdr>();
        #[allow(clippy::useless_conversion)]
        std::ptr::addr_of_mut!((*header).cmsg_len)
            .write_unaligned(len(payload_len).try_into().unwrap());
        std::ptr::addr_of_mut!((*header).cmsg_level).write_unaligned(libc::SOL_SOCKET);
        std::ptr::addr_of_mut!((*header).cmsg_type).write_unaligned(libc::SCM_RIGHTS);
        let cmsg_data_ptr = libc::CMSG_DATA(header);
        std::ptr::copy_nonoverlapping(handles.as_ptr().cast::<u8>(), cmsg_data_ptr, payload_len);
        cmsg.advance_mut(cmsg_space);
    }
}

/// Visit descriptor values in the control records in `buf`.
///
/// The callback can run before a later record produces an error. For control data
/// returned by `recvmsg`, it must take ownership of each descriptor immediately
/// and arrange to close it even if this function returns `Err`. This decoder does
/// not close descriptors itself.
pub fn decode_handles(mut buf: &[u8], mut receive: impl FnMut(RawFd)) -> io::Result<()> {
    let cmsghdr_len = len(0);

    while !buf.is_empty() {
        if buf.len() < cmsghdr_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short control header",
            ));
        }
        let cmsg = unsafe { buf.as_ptr().cast::<cmsghdr>().read_unaligned() };
        #[allow(clippy::unnecessary_cast)]
        let cmsg_len = cmsg.cmsg_len as usize;
        if cmsg_len < cmsghdr_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid control length",
            ));
        }

        match (cmsg.cmsg_level, cmsg.cmsg_type) {
            (libc::SOL_SOCKET, libc::SCM_RIGHTS) => {
                // Darwin can retain the original cmsg_len after truncation.
                let slice = &buf[cmsghdr_len..cmsg_len.min(buf.len())];
                // Keep the older API rather than raising compiler requirements.
                #[allow(clippy::chunks_exact_to_as_chunks)]
                let mut descriptors = slice.chunks_exact(mem::size_of::<RawFd>());
                for fd in &mut descriptors {
                    receive(RawFd::from_ne_bytes(fd.try_into().unwrap()));
                }
                if !descriptors.remainder().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "partial descriptor",
                    ));
                }
            }
            (level, kind) => {
                trace!("Skipping cmsg level, {level}, type={kind}...");
            }
        }

        if cmsg_len > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated control record",
            ));
        }
        let next = space(cmsg_len - cmsghdr_len);
        if next >= buf.len() {
            break;
        }
        buf = &buf[next..];
    }

    Ok(())
}

fn len(len: usize) -> usize {
    unsafe { libc::CMSG_LEN(len.try_into().unwrap()) as usize }
}

pub fn space(len: usize) -> usize {
    unsafe { libc::CMSG_SPACE(len.try_into().unwrap()) as usize }
}
