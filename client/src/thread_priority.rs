// Copyright © 2026 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

//! Per-thread real-time priority management for the client's callback thread.
//!
//! On Linux the promote path goes through the server via RPC (since sandboxed
//! content processes cannot call rtkit directly); demote is performed locally
//! via `pthread_setschedparam`, which does not require elevated privilege.
//! On macOS/Windows/Linux-no-dbus both promote and demote are performed
//! directly on the calling thread.
//!
//! All functions here are intended to run on the callback thread itself,
//! dispatched via `ipccore::EventLoopHandle::run_task`.

#[cfg(not(target_os = "linux"))]
use audio_thread_priority::{
    demote_current_thread_from_real_time, promote_current_thread_to_real_time, RtPriorityHandle,
};
#[cfg(target_os = "linux")]
use audio_thread_priority::{
    demote_thread_from_real_time, get_current_thread_info, RtPriorityThreadInfo,
};

use audioipc::rpccore::Proxy;
use audioipc::{ClientMessage, ServerMessage};
use std::cell::RefCell;

#[cfg(target_os = "linux")]
thread_local! {
    // Thread info captured at promote time.  Kept so that `demote`
    // can demote locally without another round-trip to the server.
    static THREAD_INFO: RefCell<Option<RtPriorityThreadInfo>> = const { RefCell::new(None) };
}

#[cfg(not(target_os = "linux"))]
thread_local! {
    static RT_HANDLE: RefCell<Option<RtPriorityHandle>> = const { RefCell::new(None) };
}

#[cfg(target_os = "linux")]
pub(crate) fn promote(rpc: &Proxy<ServerMessage, ClientMessage>) {
    THREAD_INFO.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return;
        }
        match get_current_thread_info() {
            Ok(info) => {
                let bytes = info.serialize();
                if rpc
                    .call(ServerMessage::PromoteThreadToRealTime(bytes))
                    .is_ok()
                {
                    *slot = Some(info);
                    debug!("callback thread promoted to real-time via server");
                } else {
                    warn!("callback thread promotion RPC failed");
                }
            }
            Err(e) => warn!("get_current_thread_info failed: {e:?}"),
        }
    });
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn promote(_rpc: &Proxy<ServerMessage, ClientMessage>) {
    RT_HANDLE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return;
        }
        match promote_current_thread_to_real_time(0, 48000) {
            Ok(handle) => {
                *slot = Some(handle);
                debug!("callback thread promoted to real-time");
            }
            Err(e) => warn!("failed to promote callback thread: {e:?}"),
        }
    });
}

#[cfg(target_os = "linux")]
pub(crate) fn demote() {
    THREAD_INFO.with(|slot| {
        if let Some(info) = slot.borrow_mut().take() {
            // Demotion to SCHED_OTHER is always permitted; no RPC needed.
            if let Err(e) = demote_thread_from_real_time(info) {
                warn!("failed to demote callback thread: {e:?}");
            } else {
                debug!("callback thread demoted from real-time");
            }
        }
    });
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn demote() {
    RT_HANDLE.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            if let Err(e) = demote_current_thread_from_real_time(handle) {
                warn!("failed to demote callback thread: {e:?}");
            } else {
                debug!("callback thread demoted from real-time");
            }
        }
    });
}
