// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.

#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;

extern crate audioipc;
extern crate audioipc_client;
extern crate audioipc_server as server;
extern crate cubeb;
extern crate env_logger;
extern crate futures;
extern crate libc;
#[macro_use]
extern crate log;

use std::process::exit;

mod client;

mod errors {
    error_chain! {
        links {
            AudioIPC(::audioipc::errors::Error, ::audioipc::errors::ErrorKind);
            Server(::server::errors::Error, ::server::errors::ErrorKind);
        }
    }
}

use errors::*;

// Run with 'RUST_LOG=run,audioipc cargo run -p ipctest'
fn run() -> Result<()> {
    let handle = server::audioipc_server_start();
    let fd = server::audioipc_server_new_client(handle);

    match unsafe { libc::fork() } {
        -1 => bail!("fork() failed"),
        0 => {
            return client::client_test(fd);
        }
        n => unsafe {
            let mut status: libc::c_int = 0;
            libc::waitpid(n, &mut status, 0);
            if libc::WIFSIGNALED(status) {
                let signum = libc::WTERMSIG(status);
                if libc::WCOREDUMP(status) {
                    bail!(
                        "Child process {} exited with sig {}. Core dumped.",
                        n,
                        signum
                    );
                } else {
                    bail!("Child process {} exited with sig {}.", n, signum);
                }
            }
        },
    };

    server::audioipc_server_stop(handle);

    Ok(())
}

fn main() {
    env_logger::init().unwrap();

    println!("Cubeb AudioServer...");

    if let Err(ref e) = run() {
        error!("error: {}", e);

        for e in e.iter().skip(1) {
            info!("caused by: {}", e);
        }

        // Requires RUST_BACKTRACE=1 in the environment.
        if let Some(backtrace) = e.backtrace() {
            info!("backtrace: {:?}", backtrace);
        }

        exit(1);
    }
}
