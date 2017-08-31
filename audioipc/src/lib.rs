// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details
#![allow(dead_code)] // TODO: Remove.

#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;

#[macro_use]
extern crate log;

#[macro_use]
extern crate serde_derive;

extern crate bincode;
extern crate bytes;
extern crate cubeb_core;
extern crate libc;
extern crate memmap;
extern crate mio;
extern crate serde;

pub mod async;
mod connection;
pub mod errors;
pub mod messages;
mod msg;
pub mod shm;

pub use connection::*;
pub use messages::{ClientMessage, ServerMessage};
use std::env::temp_dir;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

// Extend sys::os::unix::net::UnixStream to support sending and receiving a single file desc.
// We can extend UnixStream by using traits, eliminating the need to introduce a new wrapped
// UnixStream type.
pub trait RecvFd {
    fn recv_fd(&mut self, bytes: &mut [u8]) -> io::Result<(usize, Option<RawFd>)>;
}

pub trait SendFd {
    fn send_fd(&mut self, bytes: &[u8], fd: Option<RawFd>) -> io::Result<(usize)>;
}

impl RecvFd for UnixStream {
    fn recv_fd(&mut self, buf_to_recv: &mut [u8]) -> io::Result<(usize, Option<RawFd>)> {
        msg::recvmsg(self.as_raw_fd(), buf_to_recv)
    }
}

impl SendFd for UnixStream {
    fn send_fd(&mut self, buf_to_send: &[u8], fd_to_send: Option<RawFd>) -> io::Result<usize> {
        msg::sendmsg(self.as_raw_fd(), buf_to_send, fd_to_send)
    }
}

////////////////////////////////////////////////////////////////////////////////

fn get_temp_path(name: &str) -> PathBuf {
    let mut path = temp_dir();
    path.push(name);
    path
}

pub fn get_uds_path() -> PathBuf {
    get_temp_path("cubeb-sock")
}

pub fn get_shm_path(dir: &str) -> PathBuf {
    get_temp_path(&format!("cubeb-shm-{}", dir))
}
