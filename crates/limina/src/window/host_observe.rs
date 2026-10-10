// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Sampling the host state on the main thread: the AppKit half of [`crate::host_state`].
//!
//! Every window tick reads what AppKit and the system say about this VM's windows and process
//! (cheap property reads and one `thread_info`), and every [`SLOW`] the facts that cost a call
//! into the window server (displays, the session dictionary). A changed state is logged at info
//! and written into a running frame capture.
//!
//! Under App Nap the tick itself runs late (the main thread is the one throttled), so a change
//! is sampled up to one late tick after it happened; its record carries the sample's time.

use std::time::{Duration, Instant};

use limina_framecap::{HostState, Thermal, WindowState};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplication, NSWindow, NSWindowOcclusionState};
use objc2_foundation::{
    NSDictionary, NSNumber, NSProcessInfo, NSProcessInfoThermalState, NSString,
};

/// How often the window-server facts are re-read.
const SLOW: Duration = Duration::from_secs(1);

/// The effective priority at or below which the main thread is in the background band: where
/// App Nap puts every thread of the app (measured: 31 → 4), as the Game Mode clamp does.
const BACKGROUND_BAND: i32 = 4;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayIsAsleep(display: u32) -> u32;
    fn CGSessionCopyCurrentDictionary() -> *mut AnyObject;
}

/// The window-server facts, as last read.
#[derive(Clone, Copy, Default)]
struct Slow {
    displays: u32,
    displays_asleep: u32,
    screen_locked: Option<bool>,
    on_console: Option<bool>,
}

#[derive(Default)]
pub(crate) struct Sampler {
    slow: Slow,
    slow_at: Option<Instant>,
}

impl Sampler {
    /// Sample now and record it; a change goes into a running frame capture.
    pub(crate) fn tick(&mut self, primary: &NSWindow, primary_slot: usize, app: &NSApplication) {
        crate::no_throttle::reconcile();
        let state = self.sample(primary, primary_slot, app);
        let t = crate::host_state::now_ns();
        if let Some(changed) = crate::host_state::observe(state, t) {
            log::info!(
                "host state: visible={} active={} hidden={} throttled={} (main thread priority {}) \
                 windows={:?} displays asleep {}/{} locked={:?} thermal={} low_power={} \
                 no_throttle={}",
                changed.visible(),
                changed.app_active,
                changed.app_hidden,
                changed.throttled,
                changed.main_thread_priority,
                changed.windows,
                changed.displays_asleep,
                changed.displays,
                changed.screen_locked,
                changed.thermal_state.as_str(),
                changed.low_power_mode,
                changed.no_throttle,
            );
            super::frame_capture::on_host_state(changed);
        }
    }

    fn sample(
        &mut self,
        primary: &NSWindow,
        primary_slot: usize,
        app: &NSApplication,
    ) -> HostState {
        if self.slow_at.is_none_or(|at| at.elapsed() >= SLOW) {
            self.slow = read_slow();
            self.slow_at = Some(Instant::now());
        }
        let window = |slot: usize, w: &NSWindow| WindowState {
            slot,
            visible: w.occlusionState().contains(NSWindowOcclusionState::Visible),
            minimized: w.isMiniaturized(),
        };
        let mut slots = super::windows::hosted_slots();
        slots.sort_unstable();
        let mut windows: Vec<WindowState> = slots
            .into_iter()
            .filter_map(|s| super::windows::window_of_slot(s).map(|(w, _)| window(s, &w)))
            .collect();
        if !windows.iter().any(|w| w.slot == primary_slot) {
            windows.insert(0, window(primary_slot, primary));
        }
        let info = NSProcessInfo::processInfo();
        let priority = main_thread_priority();
        HostState {
            windows,
            app_active: app.isActive(),
            app_hidden: app.isHidden(),
            throttled: priority.is_some_and(|p| p <= BACKGROUND_BAND),
            main_thread_priority: priority.unwrap_or(-1),
            displays: self.slow.displays,
            displays_asleep: self.slow.displays_asleep,
            screen_locked: self.slow.screen_locked,
            on_console: self.slow.on_console,
            thermal_state: thermal(info.thermalState()),
            low_power_mode: info.isLowPowerModeEnabled(),
            no_throttle: crate::no_throttle::held(),
        }
    }
}

fn thermal(t: NSProcessInfoThermalState) -> Thermal {
    match t {
        NSProcessInfoThermalState::Nominal => Thermal::Nominal,
        NSProcessInfoThermalState::Fair => Thermal::Fair,
        NSProcessInfoThermalState::Serious => Thermal::Serious,
        NSProcessInfoThermalState::Critical => Thermal::Critical,
        _ => Thermal::Unknown,
    }
}

/// The calling thread's effective scheduling priority (`pth_curpri`); called on the main thread.
fn main_thread_priority() -> Option<i32> {
    // SAFETY: a zeroed out-struct of the size the count names; `pthread_mach_thread_np` returns
    // the calling thread's port without adding a reference, so there is nothing to deallocate.
    unsafe {
        let mut info: libc::thread_extended_info = std::mem::zeroed();
        let mut count = libc::THREAD_EXTENDED_INFO_COUNT;
        let port = libc::pthread_mach_thread_np(libc::pthread_self());
        let kr = libc::thread_info(
            port,
            libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
            (&mut info as *mut libc::thread_extended_info).cast(),
            &mut count,
        );
        (kr == libc::KERN_SUCCESS).then_some(info.pth_curpri)
    }
}

fn read_slow() -> Slow {
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    // SAFETY: a buffer of 16 ids and its length; `n` receives how many were written.
    let rc = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n) };
    let online = if rc == 0 { &ids[..n as usize] } else { &[][..] };
    // SAFETY: plain queries on display ids the system just listed.
    let asleep = online
        .iter()
        .filter(|&&d| unsafe { CGDisplayIsAsleep(d) } != 0)
        .count() as u32;
    let (screen_locked, on_console) = session();
    Slow {
        displays: online.len() as u32,
        displays_asleep: asleep,
        screen_locked,
        on_console,
    }
}

/// The login session's lock and console state, from its session dictionary. The lock key is
/// present only while the screen is locked, so its absence in a readable dictionary is "no".
fn session() -> (Option<bool>, Option<bool>) {
    // SAFETY: a Copy-rule CFDictionary (owned, +1), toll-free bridged to NSDictionary; the
    // Retained takes that reference and releases it.
    let dict: Option<Retained<NSDictionary<NSString, AnyObject>>> = unsafe {
        Retained::from_raw(
            CGSessionCopyCurrentDictionary().cast::<NSDictionary<NSString, AnyObject>>(),
        )
    };
    let Some(dict) = dict else {
        return (None, None);
    };
    let flag = |k: &str| {
        dict.objectForKey(&NSString::from_str(k))
            .and_then(|v| v.downcast::<NSNumber>().ok())
            .map(|n| n.boolValue())
    };
    (
        Some(flag("CGSSessionScreenIsLocked").unwrap_or(false)),
        flag("kCGSSessionOnConsoleKey"),
    )
}
