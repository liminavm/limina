// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! HID-level `CGEventTap` that takes three-finger trackpad gestures from macOS while the guest
//! owns them (the Input menu's "Three-Finger Gestures in the VM").
//!
//! macOS claims three-finger swipes for Spaces and Mission Control under its default setting,
//! and once it has, AppKit stops attaching touches to the app's gesture events — so the guest
//! touchpad never sees the swipe (`docs/design/trackpad-gestures.md`). Returning NULL for the
//! gesture event types at the **HID** level (a session tap is too late) leaves the Dock
//! nothing to act on, while pointer, cooked scroll, clicks and haptics pass untouched
//! (`spikes/mt-raw-capture/RESULTS.md` §hidtap). Which events to take is the trackpad
//! policy's call ([`super::trackpad::TrackpadSeq::swallows_gestures`]): a guest-owned
//! sequence whose peak is three, for all of it — four fingers stay macOS's.
//!
//! **Installed, this tap is the touch source.** An event it takes never reaches the local
//! `NSEvent` monitor, so the policy must be fed here, before the decision; the monitor then
//! ignores gesture events altogether rather than feeding the passed ones twice. It needs
//! Accessibility, like the capture tap; without it the switch stays on but does nothing, and
//! the first time the user turns it on from the menu the system prompt is raised.

use std::cell::Cell;
use std::os::raw::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicPtr, Ordering};

use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSView};
use objc2_core_graphics::CGEvent;
use objc2_foundation::NSPoint;

use super::input::InputState;

type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CGEventTapCallBack =
    extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CFMachPortCreateRunLoopSource(
        alloc: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetMain() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CGEventGetLocation(event: CGEventRef) -> NSPoint;
    static kCFRunLoopCommonModes: CFStringRef;
}

/// `kCGHIDEventTap`: where the window server takes events in from the hardware, ahead of the
/// Dock's gesture recognition.
const HID_EVENT_TAP: u32 = 0;
/// The gesture-family CGEvent types (rotate 18, begin 19, end 20, gesture 29, magnify 30,
/// swipe 31, smart magnify 32). Only 29 carries touches.
const GESTURE_TYPES: [u32; 7] = [18, 19, 20, 29, 30, 31, 32];
const GESTURE: u32 = 29;
const DISABLED_TIMEOUT: u32 = 0xFFFF_FFFE;
const DISABLED_USERINPUT: u32 = 0xFFFF_FFFF;

static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Heap context for the C callback (leaked for the app's lifetime; main run loop only).
struct TapCtx {
    input: Rc<InputState>,
    /// The primary guest view, for resolving where the pointer is.
    view: Retained<NSView>,
    /// Whether the last gesture event was taken, so the trace logs the edges only.
    swallowing: Cell<bool>,
}

thread_local! {
    /// The context of a tap not created yet — [`prepare`] leaves it here, and [`ensure`] builds
    /// the tap from it once the switch wants one and Accessibility allows it.
    static PENDING_CTX: Cell<*mut TapCtx> = const { Cell::new(std::ptr::null_mut()) };
}

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user: *mut c_void,
) -> CGEventRef {
    if etype == DISABLED_TIMEOUT || etype == DISABLED_USERINPUT {
        let port = TAP_PORT.load(Ordering::Acquire);
        if !port.is_null() {
            unsafe { CGEventTapEnable(port, true) };
        }
        log::warn!(
            "trackpad: the system disabled the gesture tap ({}) — re-enabled; gestures in the gap reached macOS",
            if etype == DISABLED_TIMEOUT {
                "we were too slow answering it"
            } else {
                "user input"
            }
        );
        return event;
    }
    // SAFETY: `user` is the leaked `TapCtx` from `ensure`; the callback runs only on the main
    // run loop while it is alive (the app's lifetime).
    let ctx = unsafe { &*(user as *const TapCtx) };
    let swallow = if etype == GESTURE {
        // SAFETY: a live CGEventRef for the duration of the callback.
        let cg: &CGEvent = unsafe { &*(event as *const CGEvent) };
        match NSEvent::eventWithCGEvent(cg) {
            Some(ns) => {
                let loc = unsafe { CGEventGetLocation(event) };
                ctx.input.on_tap_gesture(&ns, loc, &ctx.view)
            }
            None => ctx.input.swallows_gestures(),
        }
    } else {
        ctx.input.swallows_gestures()
    };
    if ctx.swallowing.replace(swallow) != swallow && super::input::wire_trace() {
        eprintln!(
            "[GESTAP] t={} {} macOS's gestures",
            super::input::wire_now_us(),
            if swallow { "taking" } else { "passing" }
        );
    }
    if swallow { std::ptr::null_mut() } else { event }
}

/// Whether the gesture tap is live — and so the touch source (see the module docs).
pub(crate) fn installed() -> bool {
    !TAP_PORT.load(Ordering::Relaxed).is_null()
}

/// Get the tap ready without creating it: [`ensure`] does that when the switch is on.
pub(crate) fn prepare(input: Rc<InputState>, view: Retained<NSView>) {
    let ctx = Box::into_raw(Box::new(TapCtx {
        input,
        view,
        swallowing: Cell::new(false),
    }));
    PENDING_CTX.with(|p| p.set(ctx));
}

/// Create the tap if it is not live yet. Returns whether it is live after the call; `false`
/// means Accessibility is missing (or [`prepare`] never ran). Main thread only. Once live it
/// stays: the switch turned off only changes what the policy owns.
pub(crate) fn ensure() -> bool {
    if installed() {
        return true;
    }
    let ctx = PENDING_CTX.with(|p| p.replace(std::ptr::null_mut()));
    if ctx.is_null() {
        return false;
    }
    let mask = GESTURE_TYPES.iter().fold(0u64, |m, t| m | (1u64 << t));
    // place = kCGHeadInsertEventTap (0), options = kCGEventTapOptionDefault (0): consuming.
    let port = unsafe { CGEventTapCreate(HID_EVENT_TAP, 0, 0, mask, tap_callback, ctx.cast()) };
    if port.is_null() {
        PENDING_CTX.with(|p| p.set(ctx));
        log::warn!(
            "trackpad: gesture tap unavailable — grant Accessibility (System Settings → Privacy & \
             Security → Accessibility); three-finger swipes stay macOS's until then"
        );
        return false;
    }
    unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
        CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
    }
    TAP_PORT.store(port, Ordering::Release);
    log::info!("trackpad: gesture tap installed (HID-level; takes guest three-finger gestures)");
    true
}
