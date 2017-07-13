#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;

extern crate audioipc;
extern crate ctrlc;
extern crate env_logger;
#[macro_use]
extern crate log;
extern crate server;

use std::process::exit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

mod errors {
    error_chain! {
        links {
            AudioIPC(::audioipc::errors::Error, ::audioipc::errors::ErrorKind);
            Server(::server::errors::Error, ::server::errors::ErrorKind);
        }
    }
}

use errors::*;

// Run server with 'RUST_LOG=run,audioipc cargo run'
// Run clients with 'RUST_LOG=run,audioipc cargo run -- -c'
fn run() -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));

    let r = running.clone();
    if let Err(_) = ctrlc::set_handler(move || { r.store(false, Ordering::SeqCst); }) {
        bail!("could not set ctrlc handler");
    }

    server::run(running)?;

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
