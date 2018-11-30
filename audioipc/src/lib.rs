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
extern crate winapi;

mod async;
#[cfg(not(target_os = "windows"))]
mod cmsg;
pub mod codec;
pub mod core;
#[allow(deprecated)]
pub mod errors;
#[cfg(not(target_os = "windows"))]
pub mod fd_passing;
pub mod frame;
pub mod messages;
#[cfg(not(target_os = "windows"))]
mod msg;
pub mod rpc;
pub mod shm;

pub use messages::{ClientMessage, ServerMessage};
use std::env::temp_dir;
use std::path::PathBuf;

// This must match the definition of
// ipc::FileDescriptor::PlatformHandleType in Gecko.
#[cfg(target_os = "windows")]
pub type PlatformHandleType = std::os::windows::raw::HANDLE;
#[cfg(not(target_os = "windows"))]
pub type PlatformHandleType = libc::c_int;

// This stands in for RawFd/RawHandle.
#[derive(Copy, Clone, Debug)]
pub struct PlatformHandle(PlatformHandleType);

// Custom serialization to treat HANDLEs as i64.
// We're slightly lazy and treat file descriptors as i64 rather than i32.
impl serde::Serialize for PlatformHandle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i64(self.0 as i64)
    }
}

struct PlatformHandleVisitor;
impl<'de> serde::de::Visitor<'de> for PlatformHandleVisitor {
    type Value = PlatformHandle;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("an integer between -2^63 and 2^63")
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(PlatformHandle::new(value as PlatformHandleType))
    }
}

impl<'de> serde::Deserialize<'de> for PlatformHandle {
    fn deserialize<D>(deserializer: D) -> Result<PlatformHandle, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_i64(PlatformHandleVisitor)
    }
}

#[cfg(not(target_os = "windows"))]
fn valid_handle(handle: PlatformHandleType) -> bool {
    handle >= 0
}

#[cfg(target_os = "windows")]
fn valid_handle(handle: PlatformHandleType) -> bool {
    const INVALID_HANDLE_VALUE: PlatformHandleType = -1isize as PlatformHandleType;
    handle != INVALID_HANDLE_VALUE
}

impl PlatformHandle {
    pub fn new(raw: PlatformHandleType) -> PlatformHandle {
        PlatformHandle(raw)
    }

    pub fn try_new(raw: PlatformHandleType) -> Option<PlatformHandle> {
        if !valid_handle(raw) {
            return None;
        }
        Some(PlatformHandle::new(raw))
    }

    pub fn as_raw(&self) -> PlatformHandleType {
        self.0
    }

    #[cfg(not(windows))]
    pub fn close(self) {
        unsafe { libc::close(self.0) };
    }

    #[cfg(windows)]
    pub fn close(self) {
        unsafe { winapi::um::handleapi::CloseHandle(self.0) };
    }
}

pub fn get_shm_path(dir: &str) -> PathBuf {
    let pid = unsafe { libc::getpid() };
    let mut temp = temp_dir();
    temp.push(&format!("cubeb-shm-{}-{}", pid, dir));
    temp
}

#[cfg(not(windows))]
pub mod messagestream_unix;
#[cfg(not(windows))]
pub use messagestream_unix::*;

#[cfg(windows)]
pub mod messagestream_win;
#[cfg(windows)]
pub use messagestream_win::*;
