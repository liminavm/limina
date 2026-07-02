// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Session-level `CGEventTap` for **reliable** pointer capture (mouselook).
//!
//! The local `NSEvent` monitor (`input.rs`) can only intercept events destined for our own
//! window — it cannot stop a click or motion from reaching *another* app when the macOS cursor
//! drifts off the VM. And `CGAssociateMouseAndMouseCursorPosition(false)` does not freeze the
//! cursor on macOS 26, so the cursor *does* drift. The result: clicks "escape" to host windows.
//!
//! A session-level event tap fixes this properly: while captured it **consumes** every mouse
//! event (returns NULL, so nothing reaches any other app) and forwards the motion/buttons/scroll
//! to the guest's relative-mouse device. It needs **Accessibility** permission (System Settings →
//! Privacy & Security → Accessibility); if that's not granted `CGEventTapCreate` returns NULL and
//! we fall back to the (leaky) local-monitor warp path. The tap is also the mechanism the future
//! system-combo capture (Cmd-Tab/Cmd-Space) will reuse.

use std::cell::Cell;
use std::os::fd::RawFd;
use std::os::raw::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Arc;

use objc2_foundation::{NSPoint, NSRect};

use limina_input::constants::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY, EV_REL, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y,
};
use limina_input::keymap::{macos_keycode_to_linux_remapped, modifier_is_down, KeyRemap};
use limina_input::InputEvent;

use super::input::{
    apply_capture_cursor, match_host_shortcut, send_event, HostCursor, HostShortcut,
};
use super::WorkerConn;

type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CGEventTapCallBack =
    extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CFMachPortCreateRunLoopSource(
        alloc: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetMain() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CGWarpMouseCursorPosition(point: NSPoint) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> NSRect;
    static kCFRunLoopCommonModes: CFStringRef;
}

// CGEventType values (CGEventTypes.h).
const LMB_DOWN: u32 = 1;
const LMB_UP: u32 = 2;
const RMB_DOWN: u32 = 3;
const RMB_UP: u32 = 4;
const MOUSE_MOVED: u32 = 5;
const LMB_DRAG: u32 = 6;
const RMB_DRAG: u32 = 7;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const SCROLL: u32 = 22;
const OMB_DOWN: u32 = 25;
const OMB_UP: u32 = 26;
const OMB_DRAG: u32 = 27;
const DISABLED_TIMEOUT: u32 = 0xFFFF_FFFE;
const DISABLED_USERINPUT: u32 = 0xFFFF_FFFF;

// CGEventField values (CGEventTypes.h).
const FIELD_BUTTON_NUMBER: u32 = 3;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;
const FIELD_KEYBOARD_AUTOREPEAT: u32 = 8;
const FIELD_KEYBOARD_KEYCODE: u32 = 9;
const FIELD_SCROLL_AXIS1: u32 = 11; // vertical, in lines
const FIELD_SCROLL_AXIS2: u32 = 12; // horizontal, in lines

/// The tap port, so the callback can re-enable itself after the system disables it (there is one
/// window/tap). Set once in [`install`]; read in the callback.
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Heap context handed to the C callback (leaked for the app's lifetime). Only ever touched on
/// the main run loop, so the `Cell` accumulators are safe.
struct TapCtx {
    captured: Arc<AtomicBool>,
    conn: Arc<WorkerConn>,
    /// Guest-cursor adoption, for the host-cursor side of a capture toggle (hide/show/re-assert).
    host_cursor: Rc<HostCursor>,
    /// Keyboard remap policy (e.g. `--swap-cmd-opt`) — same as the local monitor uses, so keys
    /// captured here translate identically to keys handled in absolute mode.
    remap: KeyRemap,
    /// Motion sensitivity: the macOS deltas the tap sees are already pointer-accelerated, and the
    /// guest's libinput accelerates *again*, so 1:1 feels far too fast. We scale by this factor
    /// (env `LIMINA_CAPTURE_SENS`, default 0.65) and carry the truncated remainder in `accum_*`
    /// so slow movements aren't quantized away. The enhanced tier additionally sets the guest
    /// pointer to a *flat* (non-accelerating) profile so the response curve stays linear.
    sens: f64,
    accum_x: Cell<f64>,
    accum_y: Cell<f64>,
}

/// Centre of the main display in global (points) coordinates — where we keep the hidden cursor
/// parked so it can't trip a hot corner or screen-edge action while captured.
fn main_display_center() -> NSPoint {
    let b = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    NSPoint::new(
        b.origin.x + b.size.width / 2.0,
        b.origin.y + b.size.height / 2.0,
    )
}

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user: *mut c_void,
) -> CGEventRef {
    // The system disables the tap on timeout / certain user input — re-enable and pass through.
    if etype == DISABLED_TIMEOUT || etype == DISABLED_USERINPUT {
        let port = TAP_PORT.load(Ordering::Acquire);
        if !port.is_null() {
            unsafe { CGEventTapEnable(port, true) };
        }
        return event;
    }
    // SAFETY: `user` is the leaked `TapCtx` from `install`; the callback only runs on the main
    // run loop while that allocation is alive (the app's lifetime).
    let ctx = unsafe { &*(user as *const TapCtx) };
    let geti = |field: u32| unsafe { CGEventGetIntegerValueField(event, field) };

    // Capture toggle (Cmd-Ctrl-G) is recognized HERE, in any state, and acted on directly — never
    // delegated to the local monitor. Reason: while captured we consume the Cmd/Ctrl flagsChanged,
    // so the window server loses the modifier state and the passed-through G keyDown reaches the
    // local monitor flag-stripped (its match_host_shortcut sees a bare G → no release → stuck).
    // CGEventGetFlags here is reliable (it's read off the event, the standard tap approach).
    if etype == KEY_DOWN {
        let keycode = geti(FIELD_KEYBOARD_KEYCODE) as u16;
        let flags = unsafe { CGEventGetFlags(event) };
        if matches!(
            match_host_shortcut(keycode, flags),
            Some(HostShortcut::ToggleCapture)
        ) {
            let now = !ctx.captured.load(Ordering::Acquire);
            ctx.captured.store(now, Ordering::Release);
            apply_capture_cursor(now, &ctx.host_cursor);
            return std::ptr::null_mut(); // consume — the toggle never reaches macOS or the guest
        }
    }

    if !ctx.captured.load(Ordering::Acquire) {
        return event; // not captured → let it through; the local monitor drives absolute mode
    }

    // Keyboard: while captured, system combos (Cmd-Tab, Cmd-Space, Ctrl-arrows, fn keys, …) go to
    // the GUEST, not macOS — translate to evdev and consume. Capture-release (Cmd-Ctrl-G) was
    // already handled above. The other host shortcut (Cmd-Ctrl-F fullscreen) is passed through
    // best-effort so the local monitor still toggles it. Modifiers stay balanced across the capture
    // boundary because the matching key-up always reaches the guest via whichever path (tap or
    // monitor) is active at release time.
    if matches!(etype, KEY_DOWN | KEY_UP | FLAGS_CHANGED) {
        // Snapshot the current worker's endpoints; holding the Arc keeps the fds open (and
        // their numbers un-reusable) for the sends below even if a relaunch retires this
        // worker mid-callback.
        let io = ctx.conn.io();
        let kbd: RawFd = io.kbd_fd();
        let keycode = geti(FIELD_KEYBOARD_KEYCODE) as u16;
        let flags = unsafe { CGEventGetFlags(event) };
        let send_kbd = |ev: InputEvent| send_event(kbd, ev);
        match etype {
            KEY_DOWN => {
                // Let our host shortcuts reach the local monitor (it toggles fullscreen/capture).
                if match_host_shortcut(keycode, flags).is_some() {
                    return event;
                }
                // Skip autorepeat: the guest compositor generates its own key repeat from one press.
                if geti(FIELD_KEYBOARD_AUTOREPEAT) == 0 {
                    if let Some(code) = macos_keycode_to_linux_remapped(keycode, &ctx.remap) {
                        send_kbd(InputEvent::new(EV_KEY, code, 1));
                        send_kbd(InputEvent::syn());
                    }
                }
            }
            KEY_UP => {
                if let Some(code) = macos_keycode_to_linux_remapped(keycode, &ctx.remap) {
                    send_kbd(InputEvent::new(EV_KEY, code, 0));
                    send_kbd(InputEvent::syn());
                }
            }
            _ => {
                // flagsChanged carries no up/down — read the modifier's resulting state.
                if let Some(down) = modifier_is_down(keycode, flags) {
                    if let Some(code) = macos_keycode_to_linux_remapped(keycode, &ctx.remap) {
                        send_kbd(InputEvent::new(EV_KEY, code, i32::from(down)));
                        send_kbd(InputEvent::syn());
                    }
                }
            }
        }
        return std::ptr::null_mut(); // consume — the combo went to the guest
    }

    // Same snapshot rule as the keyboard path above: the Arc keeps the fd open across the sends.
    let io = ctx.conn.io();
    let fd: RawFd = io.rel_ptr_fd();
    let send = |ev: InputEvent| send_event(fd, ev);
    match etype {
        MOUSE_MOVED | LMB_DRAG | RMB_DRAG | OMB_DRAG => {
            // Scale by sensitivity, carrying the sub-pixel remainder so slow motion still moves.
            let fx = ctx.accum_x.get() + geti(FIELD_DELTA_X) as f64 * ctx.sens;
            let fy = ctx.accum_y.get() + geti(FIELD_DELTA_Y) as f64 * ctx.sens;
            let dx = fx.trunc() as i32;
            let dy = fy.trunc() as i32;
            ctx.accum_x.set(fx - dx as f64);
            ctx.accum_y.set(fy - dy as f64);
            if dx != 0 {
                send(InputEvent::new(EV_REL, REL_X, dx));
            }
            if dy != 0 {
                send(InputEvent::new(EV_REL, REL_Y, dy));
            }
            if dx != 0 || dy != 0 {
                send(InputEvent::syn());
            }
            // Park the hidden cursor at centre so it can't reach a hot corner / screen edge.
            // NOTE: re-pinning every event fights any OTHER agent that also moves the macOS cursor
            // — notably a remote-desktop client driving this Mac — which reads as jitter under RD.
            // It works well locally. A cleaner containment scheme (how do VNC/RDP servers solve the
            // same capture problem?) is parked in docs/hardening-backlog.md.
            unsafe { CGWarpMouseCursorPosition(main_display_center()) };
        }
        LMB_DOWN => {
            send(InputEvent::new(EV_KEY, BTN_LEFT, 1));
            send(InputEvent::syn());
        }
        LMB_UP => {
            send(InputEvent::new(EV_KEY, BTN_LEFT, 0));
            send(InputEvent::syn());
        }
        RMB_DOWN => {
            send(InputEvent::new(EV_KEY, BTN_RIGHT, 1));
            send(InputEvent::syn());
        }
        RMB_UP => {
            send(InputEvent::new(EV_KEY, BTN_RIGHT, 0));
            send(InputEvent::syn());
        }
        OMB_DOWN | OMB_UP => {
            // Middle button only (buttonNumber 2); ignore higher buttons for now.
            if geti(FIELD_BUTTON_NUMBER) == 2 {
                let v = i32::from(etype == OMB_DOWN);
                send(InputEvent::new(EV_KEY, BTN_MIDDLE, v));
                send(InputEvent::syn());
            }
        }
        SCROLL => {
            let v = geti(FIELD_SCROLL_AXIS1) as i32;
            let h = geti(FIELD_SCROLL_AXIS2) as i32;
            if v != 0 {
                send(InputEvent::new(EV_REL, REL_WHEEL, v));
            }
            if h != 0 {
                send(InputEvent::new(EV_REL, REL_HWHEEL, h));
            }
            if v != 0 || h != 0 {
                send(InputEvent::syn());
            }
        }
        _ => {}
    }
    std::ptr::null_mut() // consume — nothing escapes to host windows
}

/// Install the capture tap on the main run loop. Returns `true` if the tap was created; `false`
/// if Accessibility permission is missing (capture then falls back to the local-monitor warp
/// path — leaky, but it still does *something*). Call once, on the main thread, before the app
/// run loop starts.
pub(crate) fn install(
    conn: Arc<WorkerConn>,
    captured: Arc<AtomicBool>,
    remap: KeyRemap,
    host_cursor: Rc<HostCursor>,
) -> bool {
    // Read sensitivity ONCE here (never on the hot callback path). Clamp to a sane band.
    let sens = std::env::var("LIMINA_CAPTURE_SENS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| *s > 0.0 && *s <= 4.0)
        .unwrap_or(0.65);
    let ctx = Box::into_raw(Box::new(TapCtx {
        captured,
        conn,
        host_cursor,
        remap,
        sens,
        accum_x: Cell::new(0.0),
        accum_y: Cell::new(0.0),
    }));
    let mask: u64 = (1 << LMB_DOWN)
        | (1 << LMB_UP)
        | (1 << RMB_DOWN)
        | (1 << RMB_UP)
        | (1 << MOUSE_MOVED)
        | (1 << LMB_DRAG)
        | (1 << RMB_DRAG)
        | (1 << KEY_DOWN)
        | (1 << KEY_UP)
        | (1 << FLAGS_CHANGED)
        | (1 << SCROLL)
        | (1 << OMB_DOWN)
        | (1 << OMB_UP)
        | (1 << OMB_DRAG);
    // tap=kCGSessionEventTap(1), place=kCGHeadInsertEventTap(0), options=kCGEventTapOptionDefault(0,
    // i.e. active/consuming).
    let port = unsafe { CGEventTapCreate(1, 0, 0, mask, tap_callback, ctx as *mut c_void) };
    if port.is_null() {
        log::warn!(
            "pointer capture: CGEventTap unavailable — grant Accessibility permission (System \
             Settings → Privacy & Security → Accessibility) for reliable capture; falling back \
             to the leaky warp path"
        );
        // SAFETY: the tap won't use `ctx`; reclaim it.
        unsafe { drop(Box::from_raw(ctx)) };
        return false;
    }
    TAP_PORT.store(port, Ordering::Release);
    unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
        CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
    }
    log::info!("pointer capture: CGEventTap installed (session-level, consuming; sens={sens})");
    true
}
