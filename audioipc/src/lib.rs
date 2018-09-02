// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details
#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;

#[macro_use]
extern crate log;

#[macro_use]
extern crate serde_derive;

extern crate bincode;
extern crate bytes;
extern crate cubeb;
#[macro_use]
extern crate futures;
extern crate iovec;
extern crate libc;
extern crate memmap;
#[macro_use]
extern crate scoped_tls;
extern crate serde;
extern crate tokio_core;
#[macro_use]
extern crate tokio_io;
extern crate tokio_uds;

mod async;
mod cmsg;
pub mod codec;
pub mod errors;
pub mod fd_passing;
pub mod frame;
pub mod rpc;
pub mod core;
pub mod messages;
mod msg;
pub mod shm;

use iovec::IoVec;
pub use messages::{ClientMessage, ServerMessage};
use std::env::temp_dir;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

// This must match the definition of
// ipc::FileDescriptor::PlatformHandleType in Gecko.
#[cfg(target_os = "windows")]
pub type PlatformHandleType = *mut std::os::raw::c_void;
#[cfg(not(target_os = "windows"))]
pub type PlatformHandleType = libc::c_int;

// Extend sys::os::unix::net::UnixStream to support sending and receiving a single file desc.
// We can extend UnixStream by using traits, eliminating the need to introduce a new wrapped
// UnixStream type.
trait RecvMsg {
    fn recv_msg(
        &mut self,
        iov: &mut [&mut IoVec],
        cmsg: &mut [u8],
    ) -> io::Result<(usize, usize, i32)>;
}

trait SendMsg {
    fn send_msg(&mut self, iov: &[&IoVec], cmsg: &[u8]) -> io::Result<usize>;
}

impl<T: AsRawFd> RecvMsg for T {
    fn recv_msg(
        &mut self,
        iov: &mut [&mut IoVec],
        cmsg: &mut [u8],
    ) -> io::Result<(usize, usize, i32)> {
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        msg::recv_msg_with_flags(self.as_raw_fd(), iov, cmsg, flags)
    }
}

impl<T: AsRawFd> SendMsg for T {
    fn send_msg(&mut self, iov: &[&IoVec], cmsg: &[u8]) -> io::Result<usize> {
        msg::send_msg_with_flags(self.as_raw_fd(), iov, cmsg, 0)
    }
}

pub fn get_shm_path(dir: &str) -> PathBuf {
    let pid = unsafe { libc::getpid() };
    let mut temp = temp_dir();
    temp.push(&format!("cubeb-shm-{}-{}", pid, dir));
    temp
}
