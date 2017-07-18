// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use ClientContext;
use audioipc::{ClientMessage, Connection, ServerMessage, messages};
use cubeb_backend::Stream;
use cubeb_core::{ErrorCode, Result, ffi};
use std::ffi::CString;
use std::os::raw::c_void;
use std::os::unix::io::FromRawFd;
use std::ptr;
use std::thread;

pub struct ClientStream<'ctx> {
    // This must be a reference to Context for cubeb, cubeb accesses stream methods via stream->context->ops
    context: &'ctx ClientContext,
    token: usize,
    join_handle: Option<thread::JoinHandle<()>>
}

fn stream_thread(
    mut conn: Connection,
    data_cb: ffi::cubeb_data_callback,
    state_cb: ffi::cubeb_state_callback,
    user_ptr: usize,
) {
    loop {
        let r = match conn.receive::<ClientMessage>() {
            Ok(r) => r,
            Err(e) => {
                debug!("stream_thread: Failed to receive message: {:?}", e);
                continue;
            },
        };

        match r {
            ClientMessage::StreamDestroyed => {
                info!("stream_thread: Shutdown callback thread.");
                return;
            },
            ClientMessage::StreamDataCallback(v, nframes, frame_size) => {
                info!(
                    "stream_thread: Data Callback: {:?} nframes={} frame_size={}",
                    v,
                    nframes,
                    frame_size
                );
                // TODO: This is proof-of-concept. Make it better.
                let mut tmp: Vec<u8> = Vec::with_capacity(nframes as usize * frame_size);
                unsafe {
                    tmp.set_len(nframes as usize * frame_size);
                }
                debug!("tmp buffer: len: {}, bytes: {:?}", tmp.len(), &tmp[..16]);
                let input_ptr: *const u8 = if v.len() > 0 { v.as_ptr() } else { ptr::null() };
                let output_ptr: *mut u8 = if tmp.len() > 0 {
                    tmp.as_mut_ptr()
                } else {
                    ptr::null_mut()
                };
                let nframes = data_cb(
                    ptr::null_mut(),
                    user_ptr as *mut c_void,
                    input_ptr as *const _,
                    output_ptr as *mut _,
                    nframes as _
                );
                tmp.truncate(nframes as usize * frame_size);
                conn.send(ServerMessage::StreamDataCallback(tmp)).unwrap();
            },
            ClientMessage::StreamStateCallback(state) => {
                info!("stream_thread: State Callback: {:?}", state);
                state_cb(ptr::null_mut(), user_ptr as *mut _, state);
            },
            m => {
                info!("Unexpected ClientMessage: {:?}", m);
            },
        }
    }
}

impl<'ctx> ClientStream<'ctx> {
    fn init(
        ctx: &'ctx ClientContext,
        init_params: messages::StreamInitParams,
        data_callback: ffi::cubeb_data_callback,
        state_callback: ffi::cubeb_state_callback,
        user_ptr: *mut c_void,
    ) -> Result<*mut ffi::cubeb_stream> {

        ctx.conn()
            .send(ServerMessage::StreamInit(init_params))
            .unwrap();

        let r = match ctx.conn().receive_with_fd::<ClientMessage>() {
            Ok(r) => r,
            Err(_) => return Err(ErrorCode::Error.into()),
        };

        let (token, conn) = match r {
            (ClientMessage::StreamCreated(tok), Some(fd)) => (tok, unsafe { Connection::from_raw_fd(fd) }),
            (ClientMessage::StreamCreated(_), None) => {
                debug!("Missing fd!");
                return Err(ErrorCode::Error.into());
            },
            (m, _) => {
                debug!("Unexpected message: {:?}", m);
                return Err(ErrorCode::Error.into());
            },
        };

        let user_data = user_ptr as usize;
        let join_handle = thread::spawn(move || {
            stream_thread(conn, data_callback, state_callback, user_data)
        });

        Ok(Box::into_raw(Box::new(ClientStream {
            context: ctx,
            token: token,
            join_handle: Some(join_handle)
        })) as _)
    }
}

impl<'ctx> Drop for ClientStream<'ctx> {
    fn drop(&mut self) {
        let _: Result<()> = send_recv!(self.context.conn(), StreamDestroy(self.token) => StreamDestroyed);
        self.join_handle.take().unwrap().join().unwrap();
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
