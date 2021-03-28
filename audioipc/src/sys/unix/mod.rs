// Copyright © 2021 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use std::io::Result;
use std::os::unix::prelude::{AsRawFd, RawFd};

use bytes::{Buf, BufMut, BytesMut, IntoBuf};
use iovec::IoVec;
use mio_07::net::UnixStream;

use crate::{close_platform_handle, cmsg, msg, PlatformHandle};

use super::{RecvMsg, SendMsg};

pub struct Pipe(pub UnixStream);

// Create a connected "pipe" pair.  The `Pipe` is the server end,
// the `PlatformHandle` is the client end to be remoted.
pub fn make_pipe_pair() -> Result<(Pipe, PlatformHandle), std::io::Error> {
    let (server, client) = UnixStream::pair()?;
    Ok((Pipe(server), PlatformHandle::from(client)))
}

impl Pipe {
    pub unsafe fn from_raw_handle(raw: crate::PlatformHandleType) -> Pipe {
        Pipe(UnixStream::from_raw_fd(raw))
    }
}

impl RecvMsg for Pipe {
    // Receive data (and fds) from the associated connection.  `recv_msg` expects the capacity of
    // the `ConnectionBuffer` members has been adjusted appropriate by the caller.
    fn recv_msg(&mut self, buf: &mut ConnectionBuffer) -> Result<usize> {
        assert!(buf.buf.remaining_mut() > 0);
        assert!(buf.cmsg.remaining_mut() > 0);
        let r = unsafe {
            let mut iovec = [<&mut IoVec>::from(buf.buf.bytes_mut())];
            #[cfg(target_os = "linux")]
            let flags = libc::MSG_CMSG_CLOEXEC;
            #[cfg(not(target_os = "linux"))]
            let flags = 0;
            msg::recv_msg_with_flags(self.0.as_raw_fd(), &mut iovec, buf.cmsg.bytes_mut(), flags)
        };
        match r {
            Ok((n, cmsg_n, flags)) => unsafe {
                assert_eq!(flags, 0);
                buf.buf.advance_mut(n);
                buf.cmsg.advance_mut(cmsg_n);
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
        assert!(buf.buf.len() > 0);
        let r = {
            let iovec = [<&IoVec>::from(&buf.buf[..buf.buf.len()])];
            msg::send_msg_with_flags(self.0.as_raw_fd(), &iovec, &buf.cmsg[..buf.cmsg.len()], 0)
        };
        match r {
            Ok(n) => {
                buf.buf.advance(n);
                // Close sent fds.
                let b = buf.cmsg.clone().freeze();
                for fd in cmsg::iterator(b) {
                    assert_eq!(fd.len(), 1);
                    unsafe {
                        close_platform_handle(fd[0]);
                    }
                }
                buf.cmsg.clear();
                Ok(n)
            }
            Err(e) => Err(e),
        }
    }
}

// Platform-specific wrapper around `BytesMut`.
// `cmsg` is a secondary buffer used for sending/receiving
// fds via `sendmsg`/`recvmsg` on a Unix Domain Socket.
#[derive(Debug)]
pub struct ConnectionBuffer {
    pub buf: BytesMut,
    pub cmsg: BytesMut,
}

impl ConnectionBuffer {
    pub fn with_capacity(cap: usize) -> Self {
        ConnectionBuffer {
            buf: BytesMut::with_capacity(cap),
            cmsg: BytesMut::with_capacity(cmsg::space(std::mem::size_of::<RawFd>())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.cmsg.is_empty()
    }
}

// Close any unprocessed fds in cmsg buffer.
impl Drop for ConnectionBuffer {
    fn drop(&mut self) {
        if !self.cmsg.is_empty() {
            debug!(
                "ConnectionBuffer dropped with {} bytes in cmsg",
                self.cmsg.len()
            );
            let b = self.cmsg.clone().freeze();
            for fd in cmsg::iterator(b) {
                assert_eq!(fd.len(), 1);
                unsafe {
                    close_platform_handle(fd[0]);
                }
            }
        }
    }
}
