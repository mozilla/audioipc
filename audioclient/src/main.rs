#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;

extern crate audioipc;
extern crate audioipc_client as client;
extern crate cubeb;
extern crate cubeb_core;
extern crate env_logger;
#[macro_use]
extern crate log;

use cubeb::SampleType;
use cubeb_core::binding::Binding;
use cubeb_core::ffi;
use std::f32::consts::PI;
use std::ffi::CString;
use std::process::exit;
use std::ptr;
use std::thread;
use std::time::Duration;

mod errors {
    error_chain! {
        links {
            AudioIPC(::audioipc::errors::Error, ::audioipc::errors::ErrorKind);
        }
    }
}

use errors::*;

const SAMPLE_RATE: u32 = 48000;
const STREAM_FORMAT: cubeb::SampleFormat = cubeb::SampleFormat::S16LE;

// store the phase of the generated waveform
struct Tone {
    position: isize
}

impl cubeb::StreamCallback for Tone {
    type Frame = cubeb::MonoFrame<i16>;

    fn data_callback(&mut self, _: &[cubeb::MonoFrame<i16>], output: &mut [cubeb::MonoFrame<i16>]) -> isize {

        // generate our test tone on the fly
        for f in output.iter_mut() {
            // North American dial tone
            let t1 = (2.0 * PI * 350.0 * self.position as f32 / SAMPLE_RATE as f32).sin();
            let t2 = (2.0 * PI * 440.0 * self.position as f32 / SAMPLE_RATE as f32).sin();

            f.m = i16::from_float(0.5 * (t1 + t2));

            self.position += 1;
        }

        output.len() as isize
    }

    fn state_callback(&mut self, state: cubeb::State) {
        println!("stream {:?}", state);
    }
}

fn print_device_info(info: &cubeb::DeviceInfo) {

    let devtype = if info.device_type().contains(cubeb::DEVICE_TYPE_INPUT) {
        "input"
    } else if info.device_type().contains(cubeb::DEVICE_TYPE_OUTPUT) {
        "output"
    } else {
        "unknown?"
    };

    let devstate = match info.state() {
        cubeb::DeviceState::Disabled => "disabled",
        cubeb::DeviceState::Unplugged => "unplugged",
        cubeb::DeviceState::Enabled => "enabled",
    };

    let devdeffmt = match info.default_format() {
        cubeb::DEVICE_FMT_S16LE => "S16LE",
        cubeb::DEVICE_FMT_S16BE => "S16BE",
        cubeb::DEVICE_FMT_F32LE => "F32LE",
        cubeb::DEVICE_FMT_F32BE => "F32BE",
        _ => "unknown?",
    };

    let mut devfmts = "".to_string();
    if info.format().contains(cubeb::DEVICE_FMT_S16LE) {
        devfmts = format!("{} S16LE", devfmts);
    }
    if info.format().contains(cubeb::DEVICE_FMT_S16BE) {
        devfmts = format!("{} S16BE", devfmts);
    }
    if info.format().contains(cubeb::DEVICE_FMT_F32LE) {
        devfmts = format!("{} F32LE", devfmts);
    }
    if info.format().contains(cubeb::DEVICE_FMT_F32BE) {
        devfmts = format!("{} F32BE", devfmts);
    }

    if let Some(device_id) = info.device_id() {
        let preferred = if info.preferred().is_empty() {
            ""
        } else {
            " (PREFERRED)"
        };
        println!("dev: \"{}\"{}", device_id, preferred);
    }
    if let Some(friendly_name) = info.friendly_name() {
        println!("\tName:    \"{}\"", friendly_name);
    }
    if let Some(group_id) = info.group_id() {
        println!("\tGroup:   \"{}\"", group_id);
    }
    if let Some(vendor_name) = info.vendor_name() {
        println!("\tVendor:  \"{}\"", vendor_name);
    }
    println!("\tType:    {}", devtype);
    println!("\tState:   {}", devstate);
    println!("\tCh:      {}", info.max_channels());
    println!(
        "\tFormat:  {} (0x{:x}) (default: {})",
        &devfmts[1..],
        info.format(),
        devdeffmt
    );
    println!(
        "\tRate:    {} - {} (default: {})",
        info.min_rate(),
        info.max_rate(),
        info.default_rate()
    );
    println!(
        "\tLatency: lo {} frames, hi {} frames",
        info.latency_lo(),
        info.latency_hi()
    );
}

fn enumerate_devices(ctx: &cubeb::Context) -> Result<()> {
    let devices = match ctx.enumerate_devices(cubeb::DEVICE_TYPE_INPUT) {
        Ok(devices) => devices,
        Err(e) if e.code() == cubeb::ErrorCode::NotSupported => {
            println!("Device enumeration not support for this backend.");
            return Ok(());
        },
        Err(e) => {
            return Err(e).chain_err(|| "Error enumerating devices");
        },
    };

    println!("Found {} input devices", devices.len());
    for d in devices.iter() {
        print_device_info(d);
    }

    println!(
        "Enumerating output devices for backend {}",
        ctx.backend_id()
    );

    let devices = match ctx.enumerate_devices(cubeb::DEVICE_TYPE_OUTPUT) {
        Ok(devices) => devices,
        Err(e) => {
            return Err(e).chain_err(|| "Error enumerating devices");
        },
    };

    println!("Found {} output devices", devices.len());
    for d in devices.iter() {
        print_device_info(d);
    }

    Ok(())
}

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
    if unsafe { client::audioipc_client_init(&mut c, context_name.as_ptr()) } < 0 {
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

    try!(enumerate_devices(&ctx));

    let params = cubeb::StreamParamsBuilder::new()
        .format(STREAM_FORMAT)
        .rate(SAMPLE_RATE)
        .channels(1)
        .layout(cubeb::ChannelLayout::Mono)
        .take();

    let stream_init_opts = cubeb::StreamInitOptionsBuilder::new()
        .stream_name("Cubeb tone (mono)")
        .output_stream_param(&params)
        .latency(4096)
        .take();

    let stream = query!(ctx.stream_init(
        &stream_init_opts,
        Tone {
            position: 0
        }
    ));

    query!(stream.start());
    thread::sleep(Duration::from_millis(500));
    query!(stream.stop());

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
