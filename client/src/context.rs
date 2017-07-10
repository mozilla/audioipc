// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use ClientStream;
use cubeb_backend::{Context, Ops};
use cubeb_core::{ChannelLayout, DeviceId, DeviceType, Result, StreamParams, ffi};
use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr;

pub struct ClientContext {
    _ops: *const Ops
}

pub const CLIENT_OPS: Ops = capi_new!(ClientContext, ClientStream);

impl Context for ClientContext {
    fn init(_context_name: Option<&CStr>) -> Result<*mut ffi::cubeb> {
        let ctx = Box::new(ClientContext {
            _ops: &CLIENT_OPS as *const _
        });
        Ok(Box::into_raw(ctx) as *mut _)
    }

    fn backend_id(&self) -> &'static CStr {
        unsafe { CStr::from_ptr(b"remote\0".as_ptr() as *const _) }
    }
    fn max_channel_count(&self) -> Result<u32> {
        Ok(0u32)
    }
    fn min_latency(&self, _params: &StreamParams) -> Result<u32> {
        Ok(0u32)
    }
    fn preferred_sample_rate(&self) -> Result<u32> {
        Ok(0u32)
    }
    fn preferred_channel_layout(&self) -> Result<ChannelLayout> {
        Ok(ChannelLayout::Undefined)
    }
    fn enumerate_devices(&self, _devtype: DeviceType) -> Result<ffi::cubeb_device_collection> {
        Ok(ffi::cubeb_device_collection {
            device: ptr::null(),
            count: 0
        })
    }
    fn device_collection_destroy(&self, _collection: *mut ffi::cubeb_device_collection) {}
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
