// Copyright © 2026 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

//! Per-thread real-time priority management for the server's callback thread.
//!
//! Promote/demote operations must be performed on the target thread on every
//! supported platform (macOS/Windows use per-thread state that isn't safely
//! modifiable from other threads, and Linux's self-demote asserts
//! `pthread_self()` matches the saved handle).  Callers arrange for these
//! functions to run on the callback thread via `ipccore::EventLoopHandle::run_task`.

use audio_thread_priority::{
    demote_current_thread_from_real_time, promote_current_thread_to_real_time, RtPriorityHandle,
};
use std::cell::RefCell;

thread_local! {
    // The RT priority handle for this thread, if currently promoted.
    static RT_HANDLE: RefCell<Option<RtPriorityHandle>> = const { RefCell::new(None) };
}

/// Promote the current thread to real-time audio priority.  Idempotent: a
/// second call on an already-promoted thread is a no-op.
pub(crate) fn promote() {
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
            Err(e) => {
                warn!("failed to promote callback thread to real-time: {e:?}");
            }
        }
    });
}

/// Demote the current thread from real-time audio priority.  Idempotent: a
/// call on an un-promoted thread is a no-op.
pub(crate) fn demote() {
    RT_HANDLE.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            if let Err(e) = demote_current_thread_from_real_time(handle) {
                warn!("failed to demote callback thread from real-time: {e:?}");
            } else {
                debug!("callback thread demoted from real-time");
            }
        }
    });
}
