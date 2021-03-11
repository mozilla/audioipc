// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use crate::async_msg::{AsyncRecvMsg, AsyncSendMsg};
use crate::cmsg;
use crate::codec::Codec;
use crate::messages::AssocRawPlatformHandle;
use bytes::{Bytes, BytesMut, IntoBuf};
use futures::{
    task::{self, Poll},
    Sink, Stream,
};
use std::collections::VecDeque;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::{fmt, io, mem};

const INITIAL_CAPACITY: usize = 1024;
const BACKPRESSURE_THRESHOLD: usize = 4 * INITIAL_CAPACITY;
const FDS_CAPACITY: usize = 16;

struct IncomingFds {
    cmsg: BytesMut,
    recv_fds: Option<cmsg::ControlMsgIter>,
}

impl IncomingFds {
    pub fn new(c: usize) -> Self {
        let capacity = c * cmsg::space(mem::size_of::<[RawFd; 3]>());
        IncomingFds {
            cmsg: BytesMut::with_capacity(capacity),
            recv_fds: None,
        }
    }

    pub fn take_fds(&mut self) -> Option<[RawFd; 3]> {
        loop {
            let fds = self
                .recv_fds
                .as_mut()
                .and_then(|recv_fds| recv_fds.next())
                .map(|fds| clone_into_array(&fds));

            if fds.is_some() {
                return fds;
            }

            if self.cmsg.is_empty() {
                return None;
            }

            self.recv_fds = Some(cmsg::iterator(self.cmsg.take().freeze()));
        }
    }

    pub fn cmsg(&mut self) -> &mut BytesMut {
        self.cmsg.reserve(cmsg::space(mem::size_of::<[RawFd; 3]>()));
        &mut self.cmsg
    }
}

#[derive(Debug)]
struct Frame {
    msgs: Bytes,
    fds: Option<Bytes>,
}

/// A unified `Stream` and `Sink` interface over an I/O object, using
/// the `Codec` trait to encode and decode the payload.
pub struct FramedWithPlatformHandles<A, C> {
    io: A,
    codec: C,
    // Stream
    read_buf: BytesMut,
    incoming_fds: IncomingFds,
    is_readable: bool,
    eof: bool,
    // Sink
    frames: VecDeque<Frame>,
    write_buf: BytesMut,
    outgoing_fds: BytesMut,
}

impl<A, C> FramedWithPlatformHandles<A, C>
where
    A: AsyncSendMsg,
{
    fn do_flush(&mut self) -> Result<(), io::Error> {
        self.do_write()?;
        self.io.flush()?;
    }

    // If there is a buffered frame, try to write it to `A`
    fn do_write(&mut self) -> Result<(), io::Error> {
        trace!("do_write...");
        // Create a frame from any pending message in `write_buf`.
        if !self.write_buf.is_empty() {
            self.set_frame(None);
        }

        trace!("pending frames: {:?}", self.frames);

        let mut processed = 0;

        loop {
            let n = match self.frames.front() {
                Some(frame) => {
                    trace!("sending msg {:?}, fds {:?}", frame.msgs, frame.fds);
                    let mut msgs = frame.msgs.clone().into_buf();
                    let fds = match frame.fds {
                        Some(ref fds) => fds.clone(),
                        None => Bytes::new(),
                    }
                    .into_buf();
                    self.io.send_msg_buf(&mut msgs, &fds)?;
                }
                _ => {
                    // No pending frames.
                    return Ok(());
                }
            };

            match self.frames.pop_front() {
                Some(mut frame) => {
                    processed += 1;

                    // Close any fds that have been sent. The fds are
                    // encoded in cmsg format inside frame.fds. Use
                    // the cmsg iterator to access msg and extract
                    // RawFds.
                    frame.fds.take().and_then(|cmsg| {
                        for fds in cmsg::iterator(cmsg) {
                            close_fds(&*fds)
                        }
                        Some(())
                    });

                    if n != frame.msgs.len() {
                        // If only part of the message was sent then
                        // re-queue the remaining message at the head
                        // of the queue. (Don't need to resend the fds
                        // since they've been sent with the first
                        // part.)
                        drop(frame.msgs.split_to(n));
                        self.frames.push_front(frame);
                        break;
                    }
                }
                _ => panic!(),
            }
        }
        trace!("process {} frames", processed);
        trace!("pending frames: {:?}", self.frames);

        Ok(())
    }

    fn set_frame(&mut self, fds: Option<Bytes>) {
        if self.write_buf.is_empty() {
            assert!(fds.is_none());
            trace!("set_frame: No pending messages...");
            return;
        }

        let msgs = self.write_buf.take().freeze();
        trace!("set_frame: msgs={:?} fds={:?}", msgs, fds);

        self.frames.push_back(Frame { msgs, fds });
    }
}

impl<A, C> Stream for FramedWithPlatformHandles<A, C>
where
    A: AsyncRecvMsg,
    C: Codec,
    C::Out: AssocRawPlatformHandle,
{
    type Item = C::Out;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Option<Self::Item>> {
        loop {
            // Repeatedly call `decode` or `decode_eof` as long as it is
            // "readable". Readable is defined as not having returned `None`. If
            // the upstream has returned EOF, and the decoder is no longer
            // readable, it can be assumed that the decoder will never become
            // readable again, at which point the stream is terminated.
            if self.is_readable {
                if self.eof {
                    let mut item = self.codec.decode_eof(&mut self.read_buf)?;
                    item.take_platform_handles(|| self.incoming_fds.take_fds());
                    return Ok(Some(item).into());
                }

                trace!("attempting to decode a frame");

                if let Some(mut item) = self.codec.decode(&mut self.read_buf)? {
                    trace!("frame decoded from buffer");
                    item.take_platform_handles(|| self.incoming_fds.take_fds());
                    return Ok(Some(item).into());
                }

                self.is_readable = false;
            }

            assert!(!self.eof);

            // Otherwise, try to read more data and try again. Make sure we've
            // got room for at least one byte to read to ensure that we don't
            // get a spurious 0 that looks like EOF
            let (n, _) = self
                .io
                .recv_msg_buf(&mut self.read_buf, self.incoming_fds.cmsg())?;

            if n == 0 {
                self.eof = true;
            }

            self.is_readable = true;
        }
    }
}

impl<A, C> Sink<C::In> for FramedWithPlatformHandles<A, C>
where
    A: AsyncSendMsg,
    C: Codec,
    C::In: AssocRawPlatformHandle + fmt::Debug,
{
    type Error = io::Error;

    fn start_send(self: Pin<&mut Self>, item: C::In) -> Result<(), Self::Error> {
        trace!("start_send: item={:?}", item);

        // If the buffer is already over BACKPRESSURE_THRESHOLD,
        // then attempt to flush it. If after flush it's *still*
        // over BACKPRESSURE_THRESHOLD, then reject the send.
        if self.write_buf.len() > BACKPRESSURE_THRESHOLD {
            self.do_flush()?;
            if self.write_buf.len() > BACKPRESSURE_THRESHOLD {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Too much backpressure",
                ));
            }
        }

        // Need to take fd ownership here for `set_frame` to keep fds alive until `do_write`,
        // otherwise fds are closed too early (when `item` is dropped).
        let fds = item.platform_handles();
        self.codec.encode(item, &mut self.write_buf)?;

        let fds = fds.and_then(|fds| {
            cmsg::builder(&mut self.outgoing_fds)
                .rights(&fds.0[..])
                .finish()
                .ok()
        });

        trace!("item fds: {:?}", fds);

        if fds.is_some() {
            // Enforce splitting sends on messages that contain file
            // descriptors.
            self.set_frame(fds);
        }

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Result<(), Self::Error>> {
        trace!("flushing framed transport");

        self.do_flush()?;

        trace!("framed transport flushed");
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Result<(), Self::Error>> {
        self.do_flush()?;
        self.io.shutdown()
    }
}

pub fn framed_with_platformhandles<A, C>(io: A, codec: C) -> FramedWithPlatformHandles<A, C> {
    FramedWithPlatformHandles {
        io,
        codec,
        read_buf: BytesMut::with_capacity(INITIAL_CAPACITY),
        incoming_fds: IncomingFds::new(FDS_CAPACITY),
        is_readable: false,
        eof: false,
        frames: VecDeque::new(),
        write_buf: BytesMut::with_capacity(INITIAL_CAPACITY),
        outgoing_fds: BytesMut::with_capacity(
            FDS_CAPACITY * cmsg::space(mem::size_of::<[RawFd; 3]>()),
        ),
    }
}

fn clone_into_array<A, T>(slice: &[T]) -> A
where
    A: Sized + Default + AsMut<[T]>,
    T: Clone,
{
    let mut a = Default::default();
    <A as AsMut<[T]>>::as_mut(&mut a).clone_from_slice(slice);
    a
}

fn close_fds(fds: &[RawFd]) {
    for fd in fds {
        unsafe {
            super::close_platformhandle(*fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::BufMut;

    extern "C" {
        fn cmsghdr_bytes(size: *mut libc::size_t) -> *const u8;
    }

    fn cmsg_bytes() -> &'static [u8] {
        let mut size = 0;
        unsafe {
            let ptr = cmsghdr_bytes(&mut size);
            std::slice::from_raw_parts(ptr, size)
        }
    }

    #[test]
    fn single_cmsg() {
        let mut incoming = super::IncomingFds::new(16);

        incoming.cmsg().put_slice(cmsg_bytes());
        assert!(incoming.take_fds().is_some());
        assert!(incoming.take_fds().is_none());
    }

    #[test]
    fn multiple_cmsg_1() {
        let mut incoming = super::IncomingFds::new(16);

        incoming.cmsg().put_slice(cmsg_bytes());
        assert!(incoming.take_fds().is_some());
        incoming.cmsg().put_slice(cmsg_bytes());
        assert!(incoming.take_fds().is_some());
        assert!(incoming.take_fds().is_none());
    }

    #[test]
    fn multiple_cmsg_2() {
        let mut incoming = super::IncomingFds::new(16);
        println!("cmsg_bytes() {}", cmsg_bytes().len());

        incoming.cmsg().put_slice(cmsg_bytes());
        incoming.cmsg().put_slice(cmsg_bytes());
        assert!(incoming.take_fds().is_some());
        incoming.cmsg().put_slice(cmsg_bytes());
        assert!(incoming.take_fds().is_some());
        assert!(incoming.take_fds().is_some());
        assert!(incoming.take_fds().is_none());
    }
}
