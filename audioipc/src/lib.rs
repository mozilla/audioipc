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
extern crate serde;
extern crate bincode;

extern crate nix;
extern crate mio;
extern crate mio_uds;
extern crate slab;

extern crate cubeb_core;

mod connection;
pub mod errors;
pub mod messages;

pub use connection::*;
pub use messages::{ClientMessage, ServerMessage};
use std::env::temp_dir;
use std::path::PathBuf;

pub fn get_uds_path() -> PathBuf {
    let mut path = temp_dir();
    path.push("cubeb-sock");
    path
}
