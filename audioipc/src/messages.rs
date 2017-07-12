// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use cubeb_core::ffi;
use std::ffi::CStr;
use std::os::raw::c_char;

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamParams {
    pub format: u32,
    pub rate: u16,
    pub channels: u8,
    pub layout: i32
}

impl<'a> From<&'a ffi::cubeb_stream_params> for StreamParams {
    fn from(params: &ffi::cubeb_stream_params) -> Self {
        assert!(params.channels <= u8::max_value() as u32);

        StreamParams {
            format: params.format,
            rate: params.rate as u16,
            channels: params.channels as u8,
            layout: params.layout
        }
    }
}

impl<'a> From<&'a StreamParams> for ffi::cubeb_stream_params {
    fn from(params: &StreamParams) -> Self {
        ffi::cubeb_stream_params {
            format: params.format,
            rate: params.rate as u32,
            channels: params.channels as u32,
            layout: params.layout
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamInitParams {
    pub context: usize,
    pub stream_name: Option<Vec<u8>>,
    pub input_device: usize,
    pub input_stream_params: Option<StreamParams>,
    pub output_device: usize,
    pub output_stream_params: Option<StreamParams>,
    pub latency_frames: u32,
    pub user_ptr: usize
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Device {
    pub output_name: Option<Vec<u8>>,
    pub input_name: Option<Vec<u8>>
}

fn dup_str(s: *const c_char) -> Option<Vec<u8>> {
    if s.is_null() {
        None
    } else {
        let vec: Vec<u8> = unsafe { CStr::from_ptr(s) }.to_bytes_with_nul().to_vec();
        Some(vec)
    }
}

impl From<ffi::cubeb_device> for Device {
    fn from(device: ffi::cubeb_device) -> Self {
        let output_name = dup_str(device.output_name);
        let input_name = dup_str(device.input_name);

        Device {
            output_name: output_name,
            input_name: input_name
        }
    }
}

// Client -> Server messages.
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    ClientConnect,
    ClientDisconnect,

    ContextGetBackendId,
    ContextGetMaxChannelCount,
    ContextGetMinLatency(StreamParams),
    ContextGetPreferredSampleRate,
    ContextGetPreferredChannelLayout,

    StreamInit(StreamInitParams),
    StreamDestroy(usize),

    StreamStart(usize),
    StreamStop(usize),
    StreamGetPosition(usize),
    StreamGetLatency(usize),
    StreamSetVolume(usize, f32),
    StreamSetPanning(usize, f32),
    StreamGetCurrentDevice(usize)
}

// Server -> Client messages.
// TODO: Streams need id.
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    ClientConnected,
    ClientDisconnected,

    ContextBackendId(),
    ContextMaxChannelCount(u32),
    ContextMinLatency(u32),
    ContextPreferredSampleRate(u32),
    ContextPreferredChannelLayout(ffi::cubeb_channel_layout),

    StreamCreated, /*(RawFd)*/
    StreamDestroyed,

    StreamStarted,
    StreamStopped,
    StreamPosition(u64),
    StreamLatency(u32),
    StreamVolumeSet(),
    StreamPanningSet(),
    StreamCurrentDevice(Device),

    ContextError(ffi::cubeb_error_code),
    StreamError, /*(Error)*/
    ClientError /*(Error)*/
}
