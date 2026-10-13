// Copyright © 2026 Mozilla Foundation
// SPDX-License-Identifier: ISC

//! Runs the real ancillary codec and its unit tests without cubeb's native build.
//!
//! Run the complete suite using Linux's ABI, including its ancillary padding:
//! cargo +nightly miri test --manifest-path audioipc/tests/scm_rights_miri/Cargo.toml \
//!     --target x86_64-unknown-linux-gnu

#[macro_use]
extern crate log;

#[path = "../../../src/sys/unix/cmsg.rs"]
pub mod cmsg;
