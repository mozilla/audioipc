// Copyright © 2021 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use std::io::{self, Result};
use std::os::unix::prelude::{AsRawFd, FromRawFd, RawFd};

use bytes::{Buf, BufMut, BytesMut};
use iovec::IoVec;
use mio::net::UnixStream;

use crate::PlatformHandle;

use super::{ConnectionBuffer, RecvMsg, SendMsg, HANDLE_QUEUE_LIMIT};

pub mod cmsg;
mod msg;

struct MessageFlags {
    receive: libc::c_int,
    send: libc::c_int,
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos"
))]
const MESSAGE_FLAGS: MessageFlags = MessageFlags {
    receive: libc::MSG_CMSG_CLOEXEC,
    send: libc::MSG_NOSIGNAL,
};
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos"
)))]
// Sending on these targets may need SO_NOSIGPIPE instead of a flag.
const MESSAGE_FLAGS: MessageFlags = MessageFlags {
    receive: 0,
    send: 0,
};

// Receive complete rights transfers before enforcing HANDLE_QUEUE_LIMIT. Darwin
// can install FDs omitted by ancillary truncation, leaving userspace unable to
// close them. 512 is a conservative capacity, not Darwin's exact send limit:
// XNU's UIPC_MAX_CMSG_FD is 512, while sockargs() imposes a tighter mbuf-size
// limit (254 FDs on current 64-bit macOS). Linux's SCM_MAX_FD is 253.
// See XNU bsd/kern/{uipc_usrreq,uipc_syscalls}.c and Linux include/net/scm.h.
// KJ uses the same 512-FD workaround:
// https://github.com/capnproto/capnproto/blob/master/c%2B%2B/src/kj/async-io-unix.c%2B%2B
// These are implementation bounds, not a portable Unix API guarantee; revisit
// them when supporting other kernels or when their ancillary handling changes.
#[cfg(any(target_os = "linux", target_os = "android"))]
const RECEIVE_HANDLE_CAPACITY: usize = 253;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const RECEIVE_HANDLE_CAPACITY: usize = 512;

#[cfg(all(test, not(miri)))]
#[path = "scm_rights_tests.rs"]
mod scm_rights_tests;

pub struct Pipe {
    pub(crate) io: UnixStream,
    cmsg: BytesMut,
}

impl Pipe {
    #[cfg(test)]
    pub(crate) fn partial_send_for_test(&mut self, len: usize) {
        msg::SEND_LIMIT.with(|limit| limit.set(Some(len)));
    }

    fn new(io: UnixStream) -> Self {
        Pipe {
            io,
            cmsg: BytesMut::with_capacity(cmsg::space(
                std::mem::size_of::<RawFd>() * RECEIVE_HANDLE_CAPACITY,
            )),
        }
    }
}

// Create a connected "pipe" pair.  The `Pipe` is the server end,
// the `PlatformHandle` is the client end to be remoted.
pub fn make_pipe_pair() -> Result<(Pipe, PlatformHandle)> {
    let (server, client) = UnixStream::pair()?;
    Ok((Pipe::new(server), PlatformHandle::from(client)))
}

impl Pipe {
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn from_raw_handle(handle: crate::PlatformHandle) -> Pipe {
        Pipe::new(UnixStream::from_raw_fd(handle.into_raw()))
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.io.shutdown(std::net::Shutdown::Both)
    }
}

impl RecvMsg for Pipe {
    // Receive data (and fds) from the associated connection.  `recv_msg` expects the capacity of
    // the `ConnectionBuffer` members has been adjusted appropriate by the caller.
    fn recv_msg(&mut self, buf: &mut ConnectionBuffer) -> Result<usize> {
        assert!(buf.buf.remaining_mut() > 0);
        let r = unsafe {
            let chunk = buf.buf.chunk_mut();
            let len = buf.read_limit.unwrap_or(chunk.len()).min(chunk.len());
            let slice = std::slice::from_raw_parts_mut(chunk.as_mut_ptr(), len);
            let mut iovec = [<&mut IoVec>::from(slice)];
            msg::recv_msg_with_flags(
                self.io.as_raw_fd(),
                &mut iovec,
                self.cmsg.chunk_mut(),
                MESSAGE_FLAGS.receive,
            )
        };
        match r {
            Ok((n, cmsg_n, msg_flags)) => unsafe {
                trace!("recv_msg_with_flags flags={msg_flags}");
                buf.buf.advance_mut(n);
                self.cmsg.advance_mut(cmsg_n);
                let mut handles = Vec::new();
                let result = cmsg::decode_handles(&self.cmsg, |h| {
                    handles.push(super::RemoteHandle::new(h, 0));
                });
                self.cmsg.clear();
                result?;
                if msg_flags & libc::MSG_CTRUNC != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated ancillary data",
                    ));
                }
                // Targets without atomic receive flags still need fcntl. This
                // fallback cannot prevent a concurrent fork/exec inheritance race.
                if MESSAGE_FLAGS.receive == 0 {
                    for h in &handles {
                        let flags = libc::fcntl(h.handle, libc::F_GETFD);
                        if flags == -1
                            || libc::fcntl(h.handle, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1
                        {
                            return Err(io::Error::last_os_error());
                        }
                    }
                }
                for h in handles {
                    buf.push_handle(h)?;
                }
                Ok(n)
            },
            Err(e) => Err(e),
        }
    }
}

impl SendMsg for Pipe {
    // Send data (and fds) on the associated connection.  `send_msg` adjusts the length of the
    // `ConnectionBuffer` members based on the size of the successful send operation.
    fn send_msg(&mut self, buf: &mut ConnectionBuffer) -> Result<usize> {
        assert!(!buf.buf.is_empty());
        if !buf.handles.is_empty() {
            let mut handles = [-1i32; HANDLE_QUEUE_LIMIT];
            for (i, h) in buf.handles.iter().enumerate() {
                handles[i] = h.handle;
            }
            cmsg::encode_handles(&mut self.cmsg, &handles[..buf.handles.len()]);
        }
        let r = {
            let iovec = [<&IoVec>::from(&buf.buf[..buf.buf.len()])];
            msg::send_msg_with_flags(self.io.as_raw_fd(), &iovec, &self.cmsg, MESSAGE_FLAGS.send)
        };
        self.cmsg.clear();
        match r {
            Ok(n) => {
                buf.buf.advance(n);
                if n > 0 {
                    buf.handles.clear();
                }
                Ok(n)
            }
            Err(e) => Err(e),
        }
    }
}
