use audioipc_client;
use cubeb::{self, ffi, Sample};
use std::f32::consts::PI;
use std::ffi::CString;
use std::os::raw::c_int;
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

type Frame = cubeb::MonoFrame<i16>;

fn print_device_info(info: &cubeb::DeviceInfo) {
    let devtype = if info.device_type().contains(cubeb::DeviceType::INPUT) {
        "input"
    } else if info.device_type().contains(cubeb::DeviceType::OUTPUT) {
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
        cubeb::DeviceFormat::S16LE => "S16LE",
        cubeb::DeviceFormat::S16BE => "S16BE",
        cubeb::DeviceFormat::F32LE => "F32LE",
        cubeb::DeviceFormat::F32BE => "F32BE",
        _ => "unknown?",
    };

    let mut devfmts = "".to_string();
    if info.format().contains(cubeb::DeviceFormat::S16LE) {
        devfmts = format!("{} S16LE", devfmts);
    }
    if info.format().contains(cubeb::DeviceFormat::S16BE) {
        devfmts = format!("{} S16BE", devfmts);
    }
    if info.format().contains(cubeb::DeviceFormat::F32LE) {
        devfmts = format!("{} F32LE", devfmts);
    }
    if info.format().contains(cubeb::DeviceFormat::F32BE) {
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
    let devices = match ctx.enumerate_devices(cubeb::DeviceType::INPUT) {
        Ok(devices) => devices,
        Err(e) if e.code() == cubeb::ErrorCode::NotSupported => {
            println!("Device enumeration not support for this backend.");
            return Ok(());
        }
        Err(e) => {
            return Err(e).chain_err(|| "Error enumerating devices");
        }
    };

    println!("Found {} input devices", devices.len());
    for d in devices.iter() {
        print_device_info(d);
    }

    println!(
        "Enumerating output devices for backend {}",
        ctx.backend_id()
    );

    let devices = match ctx.enumerate_devices(cubeb::DeviceType::OUTPUT) {
        Ok(devices) => devices,
        Err(e) => {
            return Err(e).chain_err(|| "Error enumerating devices");
        }
    };

    println!("Found {} output devices", devices.len());
    for d in devices.iter() {
        print_device_info(d);
    }

    Ok(())
}

pub fn client_test(fd: c_int) -> Result<()> {
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
    let init_params = audioipc_client::AudioIpcInitParams {
        server_connection: fd,
        pool_size: 1,
        stack_size: 64 * 1024,
        thread_create_callback: None,
    };
    if unsafe { audioipc_client::audioipc_client_init(&mut c, context_name.as_ptr(), &init_params) }
        < 0
    {
        return Err("Failed to connect to remote cubeb server.".into());
    }
    let ctx = unsafe { cubeb::Context::from_ptr(c) };

    let format = cubeb::SampleFormat::S16NE;
    let rate = query!(ctx.preferred_sample_rate());
    let channels = query!(ctx.max_channel_count());

    let params = cubeb::StreamParamsBuilder::new()
        .format(format)
        .rate(rate)
        .channels(channels)
        .layout(cubeb::ChannelLayout::UNDEFINED)
        .take();

    let latency = query!(ctx.min_latency(&params));

    println!("Cubeb context {}:", ctx.backend_id());
    println!("Max Channels: {}", channels);
    println!("Min Latency: {}", latency);
    println!("Preferred Rate: {}", rate);

    try!(enumerate_devices(&ctx));

    let params = cubeb::StreamParamsBuilder::new()
        .format(STREAM_FORMAT)
        .rate(SAMPLE_RATE)
        .channels(1)
        .layout(cubeb::ChannelLayout::MONO)
        .take();

    let mut position = 0u32;

    let mut builder = cubeb::StreamBuilder::<Frame>::new();
    builder
        .name("Cubeb tone (mono)")
        .default_output(&params)
        .latency(4096)
        .data_callback(move |_, output| {
            // generate our test tone on the fly
            for f in output.iter_mut() {
                // North American dial tone
                let t1 = (2.0 * PI * 350.0 * position as f32 / SAMPLE_RATE as f32).sin();
                let t2 = (2.0 * PI * 440.0 * position as f32 / SAMPLE_RATE as f32).sin();

                f.m = i16::from_float(0.5 * (t1 + t2));

                position += 1;
            }

            output.len() as isize
        })
        .state_callback(|state| println!("stream {:?}", state));

    let stream = query!(builder.init(&ctx));

    query!(stream.set_volume(1.0));
    query!(stream.start());
    thread::sleep(Duration::from_millis(500));
    query!(stream.stop());

    Ok(())
}
