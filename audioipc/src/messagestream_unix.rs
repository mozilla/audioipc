// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use std::os::unix::io::{IntoRawFd, FromRawFd, AsRawFd, RawFd};
use std::os::unix::net;

#[derive(Debug)]
pub struct MessageStream(net::UnixStream);
pub struct AsyncMessageStream(tokio_uds::UnixStream);

impl MessageStream {
    fn new(stream: net::UnixStream) -> MessageStream {
        MessageStream(stream)
    }

    pub unsafe fn from_raw_fd(raw: super::PlatformHandleType) -> MessageStream {
        MessageStream::new(net::UnixStream::from_raw_fd(raw))
    }
}

impl AsyncMessageStream {
    fn new(stream: tokio_uds::UnixStream) -> AsyncMessageStream {
        AsyncMessageStream(stream)
    }

    pub fn poll_read(&self) -> futures::Async<()> {
        self.0.poll_read()
    }

    pub fn poll_write(&self) -> futures::Async<()> {
        self.0.poll_write()
    }

    pub fn need_read(&self) {
        self.0.need_read()
    }

    pub fn need_write(&self) {
        self.0.need_write()
    }
}

impl std::io::Read for AsyncMessageStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl std::io::Write for AsyncMessageStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl tokio_io::AsyncRead for AsyncMessageStream {
    fn read_buf<B: bytes::BufMut>(&mut self, buf: &mut B) -> futures::Poll<usize, std::io::Error> {
        <&tokio_uds::UnixStream>::read_buf(&mut &self.0, buf)
    }
}

impl tokio_io::AsyncWrite for AsyncMessageStream {
    fn shutdown(&mut self) -> futures::Poll<(), std::io::Error> {
        <&tokio_uds::UnixStream>::shutdown(&mut &self.0)
    }

    fn write_buf<B: bytes::Buf>(&mut self, buf: &mut B) -> futures::Poll<usize, std::io::Error> {
        <&tokio_uds::UnixStream>::write_buf(&mut &self.0, buf)
    }
}

impl AsRawFd for AsyncMessageStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

pub fn anonymous_ipc_pair() -> std::result::Result<(MessageStream, MessageStream), std::io::Error> {
    let pair = net::UnixStream::pair()?;
    Ok((MessageStream::new(pair.0), MessageStream::new(pair.1)))
}

pub fn std_ipc_to_tokio_ipc(std: MessageStream, handle: &tokio_core::reactor::Handle) -> std::result::Result<AsyncMessageStream, std::io::Error> {
    Ok(AsyncMessageStream::new(tokio_uds::UnixStream::from_stream(std.0, handle)?))
}

pub fn to_raw_handle(sock: MessageStream) -> super::PlatformHandleType {
    sock.0.into_raw_fd()
}
