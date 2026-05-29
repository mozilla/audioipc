// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

use crate::stream;
use crate::{assert_not_in_callback, run_in_callback};
use crate::{ClientStream, AUDIOIPC_INIT_PARAMS};
use audioipc::ipccore::EventLoopHandle;
use audioipc::{ipccore, rpccore, sys, PlatformHandle};
use audioipc::{
    messages, messages::DeviceCollectionReq, messages::DeviceCollectionResp, ClientMessage,
    ServerMessage,
};
use cubeb_backend::{
    capi_new, ffi, ContextOps, DeviceId, DeviceType, Error, InputProcessingParams, Ops, Result,
    Stream, StreamParams, StreamParamsRef,
};
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::{fmt, ptr};

struct CubebClient;

impl rpccore::Client for CubebClient {
    type ServerMessage = ServerMessage;
    type ClientMessage = ClientMessage;
}

pub const CLIENT_OPS: Ops = capi_new!(ClientContext, ClientStream);

// ClientContext's layout *must* match cubeb.c's `struct cubeb` for the
// common fields.
#[repr(C)]
pub struct ClientContext {
    _ops: *const Ops,
    rpc: rpccore::Proxy<ServerMessage, ClientMessage>,
    rpc_thread: ipccore::EventLoopThread,
    callback_thread: ipccore::EventLoopThread,
    backend_id: CString,
    device_collection_rpc: bool,
    device_collection_callbacks: Arc<Mutex<DeviceCollectionCallbacks>>,
    // Number of ClientStreams on this context that are currently started.
    // Used to gate callback-thread promotion: the thread is promoted on the
    // 0->1 transition and demoted on 1->0.
    pub(crate) active_streams: Arc<AtomicUsize>,
}

impl ClientContext {
    #[doc(hidden)]
    pub fn rpc_handle(&self) -> &EventLoopHandle {
        self.rpc_thread.handle()
    }

    #[doc(hidden)]
    pub fn rpc(&self) -> rpccore::Proxy<ServerMessage, ClientMessage> {
        self.rpc.clone()
    }

    #[doc(hidden)]
    pub fn callback_handle(&self) -> &EventLoopHandle {
        self.callback_thread.handle()
    }
}

fn register_thread(callback: Option<extern "C" fn(*const ::std::os::raw::c_char)>) {
    if let Some(func) = callback {
        let thr = thread::current();
        let name = CString::new(thr.name().unwrap()).unwrap();
        func(name.as_ptr());
    }
}

fn unregister_thread(callback: Option<extern "C" fn()>) {
    if let Some(func) = callback {
        func();
    }
}

#[derive(Default)]
struct DeviceCollectionCallbacks {
    input_cb: ffi::cubeb_device_collection_changed_callback,
    input_user_ptr: usize,
    output_cb: ffi::cubeb_device_collection_changed_callback,
    output_user_ptr: usize,
}

struct DeviceCollectionServer {
    callbacks: Arc<Mutex<DeviceCollectionCallbacks>>,
}

impl rpccore::Server for DeviceCollectionServer {
    type ServerMessage = DeviceCollectionReq;
    type ClientMessage = DeviceCollectionResp;

    fn process(&mut self, req: Self::ServerMessage) -> Self::ClientMessage {
        match req {
            DeviceCollectionReq::DeviceChange(device_type) => {
                trace!("ctx_thread: DeviceChange Callback: device_type={device_type}");

                let devtype = cubeb_backend::DeviceType::from_bits_truncate(device_type);

                // Hold the lock across callback invocation to ensure
                // unregistration cannot complete while a callback is in
                // progress, preventing use-after-free of user_ptr.
                let cbs = self.callbacks.lock().unwrap();

                run_in_callback(|| {
                    if devtype.contains(cubeb_backend::DeviceType::INPUT) {
                        if let Some(cb) = cbs.input_cb {
                            unsafe { cb(ptr::null_mut(), cbs.input_user_ptr as *mut c_void) }
                        }
                    }
                    if devtype.contains(cubeb_backend::DeviceType::OUTPUT) {
                        if let Some(cb) = cbs.output_cb {
                            unsafe { cb(ptr::null_mut(), cbs.output_user_ptr as *mut c_void) }
                        }
                    }
                });

                DeviceCollectionResp::DeviceChange
            }
        }
    }
}

impl ContextOps for ClientContext {
    fn init(_context_name: Option<&CStr>) -> Result<Box<Self>> {
        assert_not_in_callback();

        let params = AUDIOIPC_INIT_PARAMS.with(|p| p.replace(None).unwrap());
        let thread_create_callback = params.thread_create_callback;
        let thread_destroy_callback = params.thread_destroy_callback;

        let server_connection =
            unsafe { sys::Pipe::from_raw_handle(PlatformHandle::new(params.server_connection)) };

        let rpc_thread = ipccore::EventLoopThread::new(
            "AudioIPC Client RPC".to_string(),
            None,
            move || register_thread(thread_create_callback),
            move || unregister_thread(thread_destroy_callback),
        )
        .map_err(|_| Error::Error)?;
        let rpc = rpc_thread
            .handle()
            .bind_client::<CubebClient>(server_connection)
            .map_err(|_| Error::Error)?;

        // Don't let errors bubble from here.  Later calls against this context
        // will return errors the caller expects to handle.
        let _ = send_recv!(rpc, ClientConnect => ClientConnected);

        let backend_id = send_recv!(rpc, ContextGetBackendId => ContextBackendId())
            .unwrap_or_else(|_| "(remote error)".to_string());
        let backend_id = CString::new(backend_id).expect("backend_id query failed");

        // TODO: remove params.pool_size from init params.
        // The callback thread starts at normal priority; it is promoted on demand
        // when the first stream on this context is started, and demoted when the
        // last stream stops.
        let callback_thread = ipccore::EventLoopThread::new(
            "AudioIPC Client Callback".to_string(),
            Some(params.stack_size),
            move || register_thread(thread_create_callback),
            move || {
                // Best-effort: if still promoted at shutdown, demote first.
                crate::thread_priority::demote();
                unregister_thread(thread_destroy_callback);
            },
        )
        .map_err(|_| Error::Error)?;

        let ctx = Box::new(ClientContext {
            _ops: &CLIENT_OPS as *const _,
            rpc,
            rpc_thread,
            callback_thread,
            backend_id,
            device_collection_rpc: false,
            device_collection_callbacks: Arc::new(Mutex::new(Default::default())),
            active_streams: Arc::new(AtomicUsize::new(0)),
        });
        Ok(ctx)
    }

    fn backend_id(&mut self) -> &CStr {
        assert_not_in_callback();
        self.backend_id.as_c_str()
    }

    fn max_channel_count(&mut self) -> Result<u32> {
        assert_not_in_callback();
        send_recv!(self.rpc(), ContextGetMaxChannelCount => ContextMaxChannelCount())
    }

    fn min_latency(&mut self, params: StreamParams) -> Result<u32> {
        assert_not_in_callback();
        let params = messages::StreamParams::from(params.as_ref());
        send_recv!(self.rpc(), ContextGetMinLatency(params) => ContextMinLatency())
    }

    fn preferred_sample_rate(&mut self) -> Result<u32> {
        assert_not_in_callback();
        send_recv!(self.rpc(), ContextGetPreferredSampleRate => ContextPreferredSampleRate())
    }

    fn supported_input_processing_params(&mut self) -> Result<InputProcessingParams> {
        assert_not_in_callback();
        send_recv!(self.rpc(),
                   ContextGetSupportedInputProcessingParams =>
                   ContextSupportedInputProcessingParams())
        .map(InputProcessingParams::from_bits_truncate)
    }

    fn enumerate_devices(
        &mut self,
        devtype: DeviceType,
    ) -> Result<Box<[cubeb_backend::DeviceInfo]>> {
        assert_not_in_callback();
        let v: Vec<cubeb_backend::DeviceInfo> = send_recv!(
            self.rpc(), ContextGetDeviceEnumeration(devtype.bits()) => ContextEnumeratedDevices())?
        .into_iter()
        .map(|i| cubeb_backend::DeviceInfo::from(ffi::cubeb_device_info::from(i)))
        .collect();
        Ok(v.into_boxed_slice())
    }

    fn device_collection_destroy(
        &mut self,
        collection: Box<[cubeb_backend::DeviceInfo]>,
    ) -> Result<()> {
        assert_not_in_callback();
        for dev in collection {
            let dev = ffi::cubeb_device_info::from(dev);
            if !dev.device_id.is_null() {
                let _ = unsafe { CString::from_raw(dev.device_id as *mut _) };
            }
            if !dev.group_id.is_null() {
                let _ = unsafe { CString::from_raw(dev.group_id as *mut _) };
            }
            if !dev.vendor_name.is_null() {
                let _ = unsafe { CString::from_raw(dev.vendor_name as *mut _) };
            }
            if !dev.friendly_name.is_null() {
                let _ = unsafe { CString::from_raw(dev.friendly_name as *mut _) };
            }
        }
        Ok(())
    }

    fn stream_init(
        &mut self,
        stream_name: Option<&CStr>,
        input_device: DeviceId,
        input_stream_params: Option<&StreamParamsRef>,
        output_device: DeviceId,
        output_stream_params: Option<&StreamParamsRef>,
        latency_frames: u32,
        // These params aren't sent to the server
        data_callback: ffi::cubeb_data_callback,
        state_callback: ffi::cubeb_state_callback,
        user_ptr: *mut c_void,
    ) -> Result<Stream> {
        assert_not_in_callback();

        let stream_name = stream_name.map(|name| name.to_bytes_with_nul().to_vec());

        let input_stream_params = input_stream_params.map(messages::StreamParams::from);
        let output_stream_params = output_stream_params.map(messages::StreamParams::from);

        let init_params = messages::StreamInitParams {
            stream_name,
            input_device: input_device as usize,
            input_stream_params,
            output_device: output_device as usize,
            output_stream_params,
            latency_frames,
        };
        stream::init(self, init_params, data_callback, state_callback, user_ptr)
    }

    fn register_device_collection_changed(
        &mut self,
        devtype: DeviceType,
        collection_changed_callback: ffi::cubeb_device_collection_changed_callback,
        user_ptr: *mut c_void,
    ) -> Result<()> {
        assert_not_in_callback();

        if !self.device_collection_rpc {
            let mut fd = send_recv!(self.rpc(),
                                 ContextSetupDeviceCollectionCallback =>
                                 ContextSetupDeviceCollectionCallback())?;

            let stream = unsafe { sys::Pipe::from_raw_handle(fd.platform_handle.take_handle()) };

            let server = DeviceCollectionServer {
                callbacks: self.device_collection_callbacks.clone(),
            };

            self.rpc_handle()
                .bind_server(server, stream)
                .map_err(|_| Error::Error)?;
            self.device_collection_rpc = true;
        }

        {
            let mut cbs = self.device_collection_callbacks.lock().unwrap();
            if devtype.contains(cubeb_backend::DeviceType::INPUT) {
                cbs.input_cb = collection_changed_callback;
                cbs.input_user_ptr = user_ptr as usize;
            }
            if devtype.contains(cubeb_backend::DeviceType::OUTPUT) {
                cbs.output_cb = collection_changed_callback;
                cbs.output_user_ptr = user_ptr as usize;
            }
        }

        let enable = collection_changed_callback.is_some();
        send_recv!(self.rpc(),
                   ContextRegisterDeviceCollectionChanged(devtype.bits(), enable) =>
                   ContextRegisteredDeviceCollectionChanged)
    }
}

impl Drop for ClientContext {
    fn drop(&mut self) {
        debug!("ClientContext dropped...");
        let _ = send_recv!(self.rpc(), ClientDisconnect => ClientDisconnected);
    }
}

impl fmt::Debug for ClientContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientContext")
            .field("_ops", &self._ops)
            .field("rpc", &self.rpc)
            .field("core", &self.rpc_thread)
            .field("cpu_pool", &"...")
            .finish()
    }
}
