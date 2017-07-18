// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use ClientContext;
use audioipc::{ClientMessage, ServerMessage, messages};
use cubeb_backend::Stream;
use cubeb_core::Result;
use cubeb_core::ffi;
use std::ffi::CString;
use std::os::raw::c_void;

pub struct ClientStream<'ctx> {
    // This must be a reference to Context for cubeb, cubeb accesses stream methods via stream->context->ops
    context: &'ctx ClientContext,
    token: usize,
    //
    data_callback: ffi::cubeb_data_callback,
    state_callback: ffi::cubeb_state_callback,
    user_ptr: *mut c_void
}

impl<'ctx> ClientStream<'ctx> {
    fn init(
        ctx: &'ctx ClientContext,
        init_params: messages::StreamInitParams,
        data_callback: ffi::cubeb_data_callback,
        state_callback: ffi::cubeb_state_callback,
        user_ptr: *mut c_void,
    ) -> Result<*mut ffi::cubeb_stream> {

        let token = match send_recv!(ctx.conn(), StreamInit(init_params) => StreamCreated()) {
            Ok(t) => t,
            Err(e) => return Err(e),
        };

        Ok(Box::into_raw(Box::new(ClientStream {
            context: ctx,
            token: token,
            data_callback: data_callback,
            state_callback: state_callback,
            user_ptr: user_ptr
        })) as _)
    }
}

impl<'ctx> Drop for ClientStream<'ctx> {
    fn drop(&mut self) {
        let _: Result<()> = send_recv!(self.context.conn(), StreamDestroy(self.token) => StreamDestroyed);
    }
}

impl<'ctx> Stream for ClientStream<'ctx> {
    fn start(&self) -> Result<()> {
        send_recv!(self.context.conn(), StreamStart(self.token) => StreamStarted)
    }

    fn stop(&self) -> Result<()> {
        send_recv!(self.context.conn(), StreamStop(self.token) => StreamStopped)
    }

    fn position(&self) -> Result<u64> {
        send_recv!(self.context.conn(), StreamGetPosition(self.token) => StreamPosition())
    }

    fn latency(&self) -> Result<u32> {
        send_recv!(self.context.conn(), StreamGetLatency(self.token) => StreamLatency())
    }

    fn set_volume(&self, volume: f32) -> Result<()> {
        send_recv!(self.context.conn(), StreamSetVolume(self.token, volume) => StreamVolumeSet)
    }

    fn set_panning(&self, panning: f32) -> Result<()> {
        send_recv!(self.context.conn(), StreamSetPanning(self.token, panning) => StreamPanningSet)
    }

    fn current_device(&self) -> Result<*const ffi::cubeb_device> {
        match send_recv!(self.context.conn(), StreamGetCurrentDevice(self.token) => StreamCurrentDevice()) {
            Ok(d) => Ok(Box::into_raw(Box::new(d.into()))),
            Err(e) => Err(e),
        }
    }

    fn device_destroy(&self, device: *const ffi::cubeb_device) -> Result<()> {
        // It's all unsafe...
        if !device.is_null() {
            unsafe {
                if !(*device).output_name.is_null() {
                    let _ = CString::from_raw((*device).output_name as *mut _);
                }
                if !(*device).input_name.is_null() {
                    let _ = CString::from_raw((*device).input_name as *mut _);
                }
                let _: Box<ffi::cubeb_device> = Box::from_raw(device as *mut _);
            }
        }
        Ok(())
    }

    // TODO: How do we call this back? On what thread?
    fn register_device_changed_callback(
        &self,
        _device_changed_callback: ffi::cubeb_device_changed_callback,
    ) -> Result<()> {
        Ok(())
    }
}

pub fn init(
    ctx: &ClientContext,
    init_params: messages::StreamInitParams,
    data_callback: ffi::cubeb_data_callback,
    state_callback: ffi::cubeb_state_callback,
    user_ptr: *mut c_void,
) -> Result<*mut ffi::cubeb_stream> {
    ClientStream::init(ctx, init_params, data_callback, state_callback, user_ptr)
}
