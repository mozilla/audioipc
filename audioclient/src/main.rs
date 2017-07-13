#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;

extern crate audioipc;
extern crate client;
extern crate cubeb;
extern crate cubeb_core;
extern crate env_logger;
#[macro_use]
extern crate log;

use cubeb_core::binding::Binding;
use cubeb_core::ffi;
use std::ffi::CString;
use std::process::exit;
use std::ptr;

mod errors {
    error_chain! {
        links {
            AudioIPC(::audioipc::errors::Error, ::audioipc::errors::ErrorKind);
        }
    }
}

use errors::*;

fn run() -> Result<()> {

    macro_rules! query(
        ($e: expr) => (match $e {
            Ok(v) => v,
            Err(e) => { return Err(e).chain_err(|| "cubeb api error") }
        })
        );

    // Bootstrap connection to server by calling direction into client
    // init function to get a raw cubeb pointer.
    let context_name = CString::new("AudioIPC").unwrap();
    let mut c: *mut ffi::cubeb = ptr::null_mut();
    if unsafe { client::cubeb_remote_init(&mut c, context_name.as_ptr()) } < 0 {
        return Err("Failed to connect to remote cubeb server.".into());
    }
    let ctx = unsafe { cubeb::Context::from_raw(c) };


    let format = cubeb::SampleFormat::S16NE;
    let rate = query!(ctx.preferred_sample_rate());
    let channels = query!(ctx.max_channel_count());
    let layout = query!(ctx.preferred_channel_layout());

    let params = cubeb::StreamParamsBuilder::new()
        .format(format)
        .rate(rate)
        .channels(channels)
        .layout(layout)
        .take();

    let latency = query!(ctx.min_latency(&params));

    println!("Cubeb context {}:", ctx.backend_id());
    println!("Max Channels: {}", channels);
    println!("Min Latency: {}", latency);
    println!("Preferred Rate: {}", rate);
    println!("Preferred Layout: {:?}", layout);

    Ok(())
}

fn main() {
    env_logger::init().unwrap();

    println!("Cubeb AudioClient...");

    if let Err(ref e) = run() {
        error!("error: {}", e);

        for e in e.iter().skip(1) {
            info!("caused by: {}", e);
        }

        if let Some(backtrace) = e.backtrace() {
            info!("backtrace: {:?}", backtrace);
        }

        exit(1);
    }
}
