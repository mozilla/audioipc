// Copyright © 2021 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use std::{
    collections::VecDeque,
    io::{self, Result},
};

use bytes::BytesMut;
use mio::{event::Source, Interest, Registry, Token};

#[cfg(unix)]
mod unix;
use crate::messages::RemoteHandle;

#[cfg(unix)]
pub use self::unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

impl Source for Pipe {
    fn register(&mut self, registry: &Registry, token: Token, interests: Interest) -> Result<()> {
        self.io.register(registry, token, interests)
    }

    fn reregister(&mut self, registry: &Registry, token: Token, interests: Interest) -> Result<()> {
        self.io.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> Result<()> {
        self.io.deregister(registry)
    }
}

const HANDLE_QUEUE_LIMIT: usize = 16;

#[derive(Debug)]
pub struct ConnectionBuffer {
    pub buf: BytesMut,
    handles: VecDeque<RemoteHandle>,
    #[cfg(unix)]
    read_limit: Option<usize>,
}

impl ConnectionBuffer {
    pub fn with_capacity(cap: usize) -> Self {
        ConnectionBuffer {
            buf: BytesMut::with_capacity(cap),
            handles: VecDeque::with_capacity(HANDLE_QUEUE_LIMIT),
            #[cfg(unix)]
            read_limit: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Queue an owned handle, or close it if the queue is full.
    ///
    /// Ownership is consumed on both success and error.
    pub fn push_handle(&mut self, handle: RemoteHandle) -> Result<()> {
        self.ensure_handle_capacity()?;
        self.handles.push_back(handle);
        Ok(())
    }

    pub(crate) fn ensure_handle_capacity(&self) -> Result<()> {
        if self.handles.len() >= HANDLE_QUEUE_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many queued handles",
            ));
        }
        Ok(())
    }

    pub fn pop_handle(&mut self) -> Option<RemoteHandle> {
        self.handles.pop_front()
    }

    #[cfg(unix)]
    pub(crate) fn set_read_limit(&mut self, limit: Option<usize>) {
        if let Some(limit) = limit {
            debug_assert!(limit > 0);
        }
        self.read_limit = limit;
    }

    #[cfg(unix)]
    pub(crate) fn handle_count(&self) -> usize {
        self.handles.len()
    }
}

pub trait RecvMsg {
    // Receive data from the associated connection.  `recv_msg` expects the capacity of
    // the `ConnectionBuffer` members have been adjusted appropriately by the caller.
    fn recv_msg(&mut self, buf: &mut ConnectionBuffer) -> Result<usize>;
}

pub trait SendMsg {
    // Send data on the associated connection.  `send_msg` consumes and adjusts the length of the
    // `ConnectionBuffer` members based on the size of the successful send operation.
    fn send_msg(&mut self, buf: &mut ConnectionBuffer) -> Result<usize>;
}
