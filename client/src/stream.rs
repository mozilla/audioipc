// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use cubeb_backend::Stream;
use cubeb_core::Result;
use cubeb_core::ffi;
use std::ptr;

pub struct ClientStream {}

impl Stream for ClientStream {
    fn start(&mut self) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
    fn position(&self) -> Result<u64> {
        Ok(0)
    }
    fn latency(&self) -> Result<u32> {
        Ok(0)
    }
    fn set_volume(&mut self, _volume: f32) -> Result<()> {
        Ok(())
    }
    fn set_panning(&mut self, _panning: f32) -> Result<()> {
        Ok(())
    }
    fn current_device(&mut self) -> Result<*const ffi::cubeb_device> {
        Ok(ptr::null())
    }
    fn device_destroy(&mut self, _device: *const ffi::cubeb_device) -> Result<()> {
        Ok(())
    }
    fn register_device_changed_callback(
        &mut self,
        _device_changed_callback: ffi::cubeb_device_changed_callback,
    ) -> Result<()> {
        Ok(())
    }
}
