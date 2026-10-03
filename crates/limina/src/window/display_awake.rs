// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The IOKit half of keeping the host display awake: performs what [`super::wake_policy`] decides.
//!
//! Holds one `PreventUserIdleDisplaySleep` power assertion while the policy says so, the same one
//! a Mac video player takes; `pmset -g assertions` lists it under this process with the reason
//! below. It also carries the one input that does not come from the worker: the guest's idle
//! inhibitors, which the control plane receives on its own thread.

use std::ffi::c_void;
use std::sync::Mutex;

use objc2_core_foundation::CFString;

use super::wake_policy::Reason;

type IOPMAssertionID = u32;
type IOReturn = i32;
const IOPM_ASSERTION_LEVEL_ON: u32 = 255;
const IO_RETURN_SUCCESS: IOReturn = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: *const c_void,
        level: u32,
        name: *const c_void,
        id: *mut IOPMAssertionID,
    ) -> IOReturn;
    fn IOPMAssertionRelease(id: IOPMAssertionID) -> IOReturn;
}

/// The guest's idle inhibitors as the control plane last saw them: `None` while no connected
/// helper reports them. A `Mutex` rather than main-thread state because the control plane runs on
/// its own thread; the window tick reads it.
static GUEST_INHIBITED: Mutex<Option<bool>> = Mutex::new(None);

/// What the control plane last recorded.
pub(crate) fn guest_inhibitors() -> Option<bool> {
    *GUEST_INHIBITED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The VM's display-sleep assertion, when it holds one.
#[derive(Default)]
pub(crate) struct DisplayAwake {
    id: Option<IOPMAssertionID>,
}

impl DisplayAwake {
    /// Take the assertion, naming why. A no-op while already held.
    pub(crate) fn hold(&mut self, title: &str, reason: Option<Reason>) {
        if self.id.is_some() {
            return;
        }
        let why = match reason {
            Some(Reason::GuestInhibitor) => "an application in the guest is inhibiting idle",
            Some(Reason::Playback) | None => "the guest is playing video",
        };
        let kind = CFString::from_str("PreventUserIdleDisplaySleep");
        let name = CFString::from_str(&format!("{title}: {why}"));
        let mut id: IOPMAssertionID = 0;
        // SAFETY: both strings are live CFStrings for the duration of the call, and `id` is a
        // valid out-pointer; IOKit copies what it keeps.
        let ret = unsafe {
            IOPMAssertionCreateWithName(
                (&*kind as *const CFString).cast(),
                IOPM_ASSERTION_LEVEL_ON,
                (&*name as *const CFString).cast(),
                &mut id,
            )
        };
        if ret == IO_RETURN_SUCCESS {
            log::info!("display: keeping the host display awake ({why})");
            self.id = Some(id);
        } else {
            log::warn!("display: could not keep the host display awake (IOReturn {ret:#x})");
        }
    }

    /// Release the assertion. A no-op when none is held.
    pub(crate) fn release(&mut self) {
        let Some(id) = self.id.take() else { return };
        // SAFETY: `id` came from a successful create and is released exactly once.
        let ret = unsafe { IOPMAssertionRelease(id) };
        if ret == IO_RETURN_SUCCESS {
            log::info!("display: the host display may sleep again");
        } else {
            log::warn!("display: releasing the display-sleep assertion failed (IOReturn {ret:#x})");
        }
    }
}

impl Drop for DisplayAwake {
    fn drop(&mut self) {
        self.release();
    }
}
