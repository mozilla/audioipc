// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use super::tokio_uds_stream as tokio_uds;
use mio::Ready;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Debug)]
pub struct MessageStream(net::UnixStream);
pub struct AsyncMessageStream(tokio_uds::UnixStream);

impl MessageStream {
    fn new(stream: net::UnixStream) -> MessageStream {
        MessageStream(stream)
    }

    pub fn anonymous_ipc_pair() -> std::result::Result<(MessageStream, MessageStream), io::Error> {
        let pair = net::UnixStream::pair()?;
        Ok((MessageStream::new(pair.0), MessageStream::new(pair.1)))
    }

    pub unsafe fn from_raw_fd(raw: super::PlatformHandleType) -> MessageStream {
        MessageStream::new(net::UnixStream::from_raw_fd(raw))
    }

    pub fn into_tokio_ipc(self) -> Result<Self, io::Error> {
        Ok(AsyncMessageStream::new(tokio_uds::UnixStream::from_std(
            self.0,
        )?))
    }
}

impl IntoRawFd for MessageStream {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl AsyncMessageStream {
    fn new(stream: tokio_uds::UnixStream) -> AsyncMessageStream {
        AsyncMessageStream(stream)
    }

    pub fn poll_read_ready(&self, ready: Ready) -> Poll<Result<Ready, io::Error>> {
        self.0.poll_read_ready(ready)
    }

    pub fn clear_read_ready(&self, ready: Ready) -> Result<(), io::Error> {
        self.0.clear_read_ready(ready)
    }

    pub fn poll_write_ready(&self) -> Poll<Result<Ready, io::Error>> {
        self.0.poll_write_ready()
    }

    pub fn clear_write_ready(&self) -> Result<(), io::Error> {
        self.0.clear_write_ready()
    }
}

impl io::Read for AsyncMessageStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl io::Write for AsyncMessageStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl AsyncRead for AsyncMessageStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut ReadBuf,
    ) -> Poll<Result<(), io::Error>> {
        <&tokio_uds::UnixStream>::poll_read(&mut &self.0, cx, buf)
    }
}

impl AsyncWrite for AsyncMessageStream {
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        <&tokio_uds::UnixStream>::poll_shutdown(&mut &*self, cx)
    }

    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        <&tokio_uds::UnixStream>::poll_write(&mut &*self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        <&tokio_uds::UnixStream>::poll_flush(&mut &*self, cx)
    }
}

impl AsRawFd for AsyncMessageStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}
