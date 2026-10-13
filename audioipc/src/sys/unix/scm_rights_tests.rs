// Copyright © 2026 Mozilla Foundation
// SPDX-License-Identifier: ISC

use super::*;
use crate::messages::RemoteHandle;
use std::fs::File;
use std::io;
use std::mem;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};

fn pair() -> (Pipe, Pipe) {
    let (pipe, peer) = make_pipe_pair().unwrap();
    (pipe, unsafe { Pipe::from_raw_handle(peer) })
}

fn outbound() -> ConnectionBuffer {
    let mut buf = ConnectionBuffer::with_capacity(32);
    buf.buf.extend_from_slice(b"x");
    let fd = File::open("/dev/null").unwrap().into_raw_fd();
    buf.push_handle(unsafe { RemoteHandle::new(fd, 0) })
        .unwrap();
    buf
}

fn fail_send(pipe: &mut Pipe, buf: &mut ConnectionBuffer) {
    msg::SEND_ERROR.with(|error| error.set(Some(libc::EAGAIN)));
    let result = pipe.send_msg(buf);
    // Clear even if a future implementation returns before the syscall hook.
    msg::SEND_ERROR.with(|error| error.set(None));
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

// Separate processes make descriptor counts independent of concurrent tests and
// contain descriptor ownership errors if the implementation regresses.
fn isolated() -> bool {
    let thread = std::thread::current();
    let name = thread.name().unwrap();
    if std::env::var("AUDIOIPC_SCM_TEST_CHILD").as_deref() == Ok(name) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env("AUDIOIPC_SCM_TEST_CHILD", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    false
}

// Independent wire fixtures: do not use the production encoder/decoder to
// validate their own output. usize storage supplies alignment and is initialized.
fn send_rights(socket: RawFd, fds: &[RawFd]) {
    let size = cmsg::space(mem::size_of_val(fds));
    let mut control = vec![0usize; size.div_ceil(mem::size_of::<usize>())];
    let byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_ptr() as *mut _,
        iov_len: byte.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = size as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(mem::size_of_val(fds) as _) as _;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(header),
            mem::size_of_val(fds),
        );
        assert_eq!(
            libc::sendmsg(socket, &msg, 0),
            1,
            "{}",
            io::Error::last_os_error()
        );
    }
}

fn receive_rights(socket: RawFd) -> Vec<OwnedFd> {
    let mut control = [0usize; 64];
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = mem::size_of_val(&control) as _;
    let mut fds = Vec::new();
    unsafe {
        assert_eq!(
            libc::recvmsg(socket, &mut msg, 0),
            1,
            "{}",
            io::Error::last_os_error()
        );
        assert_eq!(msg.msg_flags & libc::MSG_CTRUNC, 0);
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            assert_eq!((*header).cmsg_level, libc::SOL_SOCKET);
            assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
            let count = ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                / mem::size_of::<RawFd>();
            for i in 0..count {
                let fd = libc::CMSG_DATA(header)
                    .cast::<RawFd>()
                    .add(i)
                    .read_unaligned();
                fds.push(OwnedFd::from_raw_fd(fd));
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    fds
}

#[test]
fn scm_rights_retry_transfers_exactly_one_descriptor() {
    if !isolated() {
        return;
    }
    let (mut pipe, peer) = pair();
    let mut out = outbound();
    fail_send(&mut pipe, &mut out);
    assert_eq!(out.handles.len(), 1);
    assert_eq!(pipe.send_msg(&mut out).unwrap(), 1);
    assert!(out.handles.is_empty());
    assert_eq!(receive_rights(peer.io.as_raw_fd()).len(), 1);
}

#[test]
fn scm_rights_receive_after_failed_send_does_not_claim_outbound_fd() {
    if !isolated() {
        return;
    }
    let (mut pipe, mut peer) = pair();
    let mut out = outbound();
    let outbound_fd = out.handles.front().unwrap().handle;
    fail_send(&mut pipe, &mut out);
    let mut data = ConnectionBuffer::with_capacity(1);
    data.buf.extend_from_slice(b"y");
    peer.send_msg(&mut data).unwrap();
    let mut input = ConnectionBuffer::with_capacity(32);
    assert_eq!(pipe.recv_msg(&mut input).unwrap(), 1);
    let count = input.handles.len();
    // Disarm accidental duplicate owners before assertions unwind the test.
    while let Some(handle) = input.pop_handle() {
        if handle.handle == outbound_fd {
            unsafe {
                handle.take();
            }
        }
    }
    assert_eq!(
        count, 0,
        "ordinary data must not create an inbound descriptor"
    );
    assert_ne!(unsafe { libc::fcntl(outbound_fd, libc::F_GETFD) }, -1);
    assert_eq!(out.handles.len(), 1);
}

fn descriptor_count() -> usize {
    #[cfg(target_os = "linux")]
    let path = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    let path = "/dev/fd";
    std::fs::read_dir(path).unwrap().count()
}

#[test]
fn scm_rights_oversized_transfer_returns_error_without_leaking() {
    if !isolated() {
        return;
    }
    assert_oversized_transfer_does_not_leak(HANDLE_QUEUE_LIMIT + 1);
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", target_pointer_width = "64")
))]
#[test]
fn scm_rights_kernel_maximum_transfer_returns_error_without_leaking() {
    if !isolated() {
        return;
    }
    // Linux: SCM_MAX_FD. 64-bit Darwin: sockargs() expands each FD to an
    // eight-byte pointer within a 2048-byte mbuf, including a 12-byte header.
    // Keep this independent of the production buffer capacity so reducing that
    // capacity below a supported kernel's transfer limit breaks the test.
    #[cfg(target_os = "linux")]
    let max_handles = 253;
    #[cfg(target_os = "macos")]
    let max_handles = 254;
    if !ensure_descriptor_capacity(max_handles) {
        return;
    }
    assert_oversized_transfer_does_not_leak(max_handles);
}

// A complete transfer needs room for the received duplicates as well as the
// child's existing descriptors, the socket pair, and the source file.
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", target_pointer_width = "64")
))]
fn ensure_descriptor_capacity(handle_count: usize) -> bool {
    let required = (descriptor_count() + handle_count + 3) as libc::rlim_t;
    unsafe {
        let mut limit: libc::rlimit = mem::zeroed();
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        if limit.rlim_cur >= required {
            return true;
        }
        if limit.rlim_max >= required {
            limit.rlim_cur = required;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                return true;
            }
        }
    }
    eprintln!("skipping kernel-maximum transfer: need {required} descriptor slots");
    false
}

fn assert_oversized_transfer_does_not_leak(handle_count: usize) {
    let before = descriptor_count();
    for _ in 0..4 {
        let (mut pipe, peer) = pair();
        let file = File::open("/dev/null").unwrap();
        send_rights(peer.io.as_raw_fd(), &vec![file.as_raw_fd(); handle_count]);
        let mut input = ConnectionBuffer::with_capacity(32);
        let error = pipe
            .recv_msg(&mut input)
            .expect_err("oversized rights transfer must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // The complete transfer must reach application-level limit enforcement,
        // rather than failing because the ancillary buffer was too small.
        assert_eq!(error.to_string(), "too many queued handles");
    }
    assert_eq!(
        descriptor_count(),
        before,
        "oversized transfers leaked descriptors"
    );
}

#[test]
fn scm_rights_queue_overflow_returns_error_without_leaking() {
    if !isolated() {
        return;
    }
    let before = descriptor_count();
    {
        let (mut pipe, peer) = pair();
        let file = File::open("/dev/null").unwrap();
        let mut input = ConnectionBuffer::with_capacity(32);
        send_rights(peer.io.as_raw_fd(), &[file.as_raw_fd(); HANDLE_QUEUE_LIMIT]);
        assert_eq!(pipe.recv_msg(&mut input).unwrap(), 1);
        assert_eq!(input.handles.len(), HANDLE_QUEUE_LIMIT);
        send_rights(peer.io.as_raw_fd(), &[file.as_raw_fd()]);
        let error = pipe
            .recv_msg(&mut input)
            .expect_err("excess rights must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
    assert_eq!(
        descriptor_count(),
        before,
        "queue overflow leaked descriptors"
    );
}

#[test]
fn scm_rights_received_descriptor_is_cloexec() {
    if !isolated() {
        return;
    }
    let (mut pipe, peer) = pair();
    let file = File::open("/dev/null").unwrap();
    send_rights(peer.io.as_raw_fd(), &[file.as_raw_fd()]);
    let mut input = ConnectionBuffer::with_capacity(32);
    assert_eq!(pipe.recv_msg(&mut input).unwrap(), 1);
    let handle = input.pop_handle().unwrap();
    let flags = unsafe { libc::fcntl(handle.handle, libc::F_GETFD) };
    assert_ne!(flags, -1);
    assert_ne!(
        flags & libc::FD_CLOEXEC,
        0,
        "received descriptor is inheritable"
    );
}
