// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use ClientStream;
use audioipc::{self, ClientMessage, Connection, ServerMessage, messages};
use cubeb_backend::{Context, Ops};
use cubeb_core::{DeviceId, DeviceType, Error, Result, StreamParams, ffi};
use cubeb_core::binding::Binding;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::os::unix::net::UnixStream;
use std::ptr;

pub struct ClientContext {
    _ops: *const Ops,
    connection: Connection
}

macro_rules! t(
    ($e:expr) => (
        match $e {
            Ok(e) => e,
            Err(_) => return Err(Error::default())
        }
    ));

macro_rules! send_recv {
    ($self_:ident, $smsg:ident => $rmsg:ident) => {{
        $self_.connection
            .send(ServerMessage::$smsg)
            .unwrap();
        send_recv!(__recv $self_, $rmsg)
    }};
    ($self_:ident, $smsg:ident => $rmsg:ident()) => {{
        $self_.connection
            .send(ServerMessage::$smsg)
            .unwrap();
        send_recv!(__recv $self_, $rmsg ())
    }};
    ($self_:ident, $smsg:ident($p:tt) => $rmsg:ident()) => {{
        $self_.connection
            .send(ServerMessage::$smsg($p))
            .unwrap();
        send_recv!(__recv $self_, $rmsg ())
    }};
    ($self_:ident, $smsg:ident($p:tt) => $rmsg:ident) => {{
        $self_.connection
            .send(ServerMessage::$smsg($p))
            .unwrap();
        send_recv!(__recv $self_, $rmsg)
    }};
    (__recv $self_:ident, $rmsg:ident ()) =>
        (if let ClientMessage::$rmsg(v) = $self_.connection.receive().unwrap() {
            Ok(v)
        } else {
            panic!("wrong message received");
        });
    (__recv $self_:ident, $rmsg:ident) =>
        (if let ClientMessage::$rmsg = $self_.connection.receive().unwrap() {
            ()
        } else {
            panic!("wrong message received");
        })
}

pub const CLIENT_OPS: Ops = capi_new!(ClientContext, ClientStream);

impl Context for ClientContext {
    fn init(_context_name: Option<&CStr>) -> Result<*mut ffi::cubeb> {
        // TODO: encapsulate connect, etc inside audioipc.
        let stream = t!(UnixStream::connect(audioipc::get_uds_path()));
        let ctx = Box::new(ClientContext {
            _ops: &CLIENT_OPS as *const _,
            connection: Connection::new(stream)
        });
        Ok(Box::into_raw(ctx) as *mut _)
    }

    fn backend_id(&mut self) -> &'static CStr {
        unsafe { CStr::from_ptr(b"remote\0".as_ptr() as *const _) }
    }

    fn max_channel_count(&mut self) -> Result<u32> {
        send_recv!(self, ContextGetMaxChannelCount => ContextMaxChannelCount())
    }

    fn min_latency(&mut self, params: &StreamParams) -> Result<u32> {
        let params = messages::StreamParams::from(unsafe { &*params.raw() });
        send_recv!(self, ContextGetMinLatency(params) => ContextMinLatency())
    }

    fn preferred_sample_rate(&mut self) -> Result<u32> {
        send_recv!(self, ContextGetPreferredSampleRate => ContextPreferredSampleRate())
    }

    fn preferred_channel_layout(&mut self) -> Result<ffi::cubeb_channel_layout> {
        send_recv!(self, ContextGetPreferredChannelLayout => ContextPreferredChannelLayout())
    }

    fn enumerate_devices(&mut self, _devtype: DeviceType) -> Result<ffi::cubeb_device_collection> {
        Ok(ffi::cubeb_device_collection {
            device: ptr::null(),
            count: 0
        })
    }

    fn device_collection_destroy(&mut self, _collection: *mut ffi::cubeb_device_collection) {}

    fn stream_init(
        &mut self,
        _stream_name: Option<&CStr>,
        _input_device: DeviceId,
        _input_stream_params: Option<&ffi::cubeb_stream_params>,
        _output_device: DeviceId,
        _output_stream_params: Option<&ffi::cubeb_stream_params>,
        _latency_frame: u32,
        _data_callback: ffi::cubeb_data_callback,
        _state_callback: ffi::cubeb_state_callback,
        _user_ptr: *mut c_void,
    ) -> Result<*mut ffi::cubeb_stream> {
        Ok(ptr::null_mut())
    }

    fn register_device_collection_changed(
        &mut self,
        _dev_type: DeviceType,
        _collection_changed_callback: ffi::cubeb_device_collection_changed_callback,
        _user_ptr: *mut c_void,
    ) -> Result<()> {
        Ok(())
    }
}

impl Drop for ClientContext {
    fn drop(&mut self) {
        info!("ClientContext drop...");
        send_recv!(self, ClientDisconnect => ClientDisconnected);
    }
}
