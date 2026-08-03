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
//! event (returns NULL, so nothing reaches any other app), integrates the motion deltas into a
//! virtual cursor position, and drives the guest's **absolute tablet** with it — the same device
//! and fit mapping as uncaptured mode, so captured movement feels exactly like the host cursor
//! (macOS pointer ballistics are already in the deltas, and libinput never accelerates an
//! absolute device). Motion the edge clamp eats is forwarded to the relative-mouse device as
//! *pressure* so mutter's barriers (GNOME hot corner) still fire. System key combos
//! (Cmd-Tab/Cmd-Space/media keys) are consumed and forwarded the same way. Because the tap is
//! session-wide, the grab arm of the toggle only engages while our window is key.
//! It needs **Accessibility** permission (System Settings →
//! Privacy & Security → Accessibility); if that's not granted `CGEventTapCreate` returns NULL and
//! we fall back to the (leaky) local-monitor warp path — but not silently: a failed install is
//! kept retryable ([`retry_install`], the grant takes effect on a fresh create without a VM
//! restart) and the first capture toggle without the tap raises the system Accessibility prompt
//! ([`prompt_accessibility_once`]).

use std::cell::Cell;
use std::os::fd::RawFd;
use std::os::raw::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSView, NSWindowStyleMask};
use objc2_core_graphics::CGEvent;
use objc2_foundation::NSPoint;

use limina_input::auxkey::{
    decode_aux_data1, nx_key_bucket, nx_key_to_linux, route_aux_event_key, AuxBucket, GrabMode,
    NX_SUBTYPE_AUX_CONTROL_BUTTONS,
};
use limina_input::constants::{
    ABS_MAX, ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_ABS, EV_KEY,
};
use limina_input::InputEvent;

use super::fit::{self, FitRect};
use super::input::{
    match_host_shortcut, send_edge_overflow, send_event, HostShortcut, InputState, RevealSrc,
    UngrabAction,
};
use super::WorkerConn;

type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
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
    fn CGEventGetDoubleValueField(event: CGEventRef, field: u32) -> f64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CFMachPortCreateRunLoopSource(
        alloc: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetMain() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CGWarpMouseCursorPosition(point: NSPoint) -> i32;
    /// Where the event says the cursor is, in CG global coordinates (top-left origin), *after*
    /// this motion was applied — the authoritative position, including the window server's own
    /// clamping to the union of the displays.
    fn CGEventGetLocation(event: CGEventRef) -> NSPoint;
    /// How many displays contain this global point. Zero means the point is nowhere the cursor
    /// can actually be — see [`point_on_a_display`].
    fn CGGetDisplaysWithPoint(
        point: NSPoint,
        max_displays: u32,
        displays: *mut u32,
        matching: *mut u32,
    ) -> i32;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopCommonModes: CFStringRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFBooleanTrue: *const c_void;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// Returns whether the process is trusted for Accessibility; with
    /// `kAXTrustedCheckOptionPrompt = true` it ALSO raises the system prompt that registers the
    /// app in System Settings → Privacy & Security → Accessibility (TCC attributes the request
    /// to the responsible app — the Limina bundle — not this supervisor process).
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
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
/// `NX_SYSDEFINED` — the class the special/media top row arrives in (see
/// [`limina_input::auxkey`]); its subtype-8 events carry the key in `NSEvent.data1`.
const SYS_DEFINED: u32 = 14;
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
// (The two scroll *line* fields used to be read here; scrolling now goes through the shared
// `InputState::emit_scroll` via the NSEvent bridge, which sees the precise deltas those fields
// quantize away.)

/// The tap port, so the callback can re-enable itself after the system disables it (there is one
/// window/tap). Set once in [`install`]; read in the callback.
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Heap context handed to the C callback (leaked for the app's lifetime). Only ever touched on
/// the main run loop, so the `Cell` accumulators are safe.
struct TapCtx {
    captured: Arc<AtomicBool>,
    conn: Arc<WorkerConn>,
    /// The shared input translator: keyboard forwarding, capture toggles
    /// ([`InputState::toggle_capture`] — host-cursor transition, release warp, modifier
    /// reconciliation), and the Ctrl+Option ungrab-chord state all live there, so the tap
    /// and the local monitor can never disagree. Main thread only, like everything here.
    input: Rc<InputState>,
    /// Soft keyboard grab enabled (policy; `--no-soft-kbd-grab` turns it off).
    soft_enabled: bool,
    /// Ctrl-Opt muted the soft grab; cleared when the window regains key status.
    soft_muted: Cell<bool>,
    /// Whether the window was key at the last event — the regain edge un-mutes.
    was_key: Cell<bool>,
    /// The current fit rect (letterbox geometry), shared with the render path — the virtual
    /// cursor moves and clamps in its space, so captured motion maps to guest pixels exactly
    /// like uncaptured motion does.
    fit: Rc<Cell<FitRect>>,
    /// The virtual cursor position (view points, bottom-left origin), shared with
    /// `InputState`: uncaptured motion keeps it at the pointer's last position over the
    /// content (the grab seed); captured motion integrates the macOS-accelerated deltas into
    /// it; a release warps the host cursor back to it.
    pos: Rc<Cell<Option<(f64, f64)>>>,
    /// The guest view, for mapping the virtual cursor back to screen coordinates on release.
    view: Retained<NSView>,
    /// Seconds of sustained edge press that release the fullscreen grab (`[display]
    /// edge-resistance`, migrated to a duration — see [`crate::vmlib::schema::EdgeHold`]).
    /// Zero means `Off`: never grab, exactly today's free pointer.
    hold: f64,
    /// Charge built by the current edge press, and which edge it is against. See [`fit::Charge`].
    grab_charge: Cell<fit::Charge>,
    grab_edge: Cell<Option<fit::Edge>>,
    /// When the pointer became [`fit::deep_inside`] the content, for the re-grab dwell.
    inside_since: Cell<Option<std::time::Instant>>,
    /// This grab is the FULLSCREEN POLICY's: a sustained edge press releases it, and leaving
    /// fullscreen ends it.
    ///
    /// Set only by the policy — **never** by Cmd-Ctrl-G, even in fullscreen. The two grabs are
    /// deliberately different tools. The policy grab is a convenience with a way out at every
    /// edge; the explicit grab is what you reach for when the pointer must not leave the VM *for
    /// any reason*, which is also why it is not the default. Giving it the edge release would make
    /// it the same thing as the policy grab wherever it matters most, leaving no way to ask for an
    /// unconditional hold. Its ways out stay Cmd-Ctrl-G and the Ctrl-Opt chord.
    fullscreen_grab: Cell<bool>,
    /// The user let the pointer go on purpose (Cmd-Ctrl-G or the Ctrl-Opt chord), so the policy
    /// must not take it straight back. Cleared when the window regains key status, exactly like
    /// [`Self::soft_muted`] — clicking away and back is the way to ask for it again. NOT set by
    /// the edge-press release, which is the designed way out and whose oscillation guard is
    /// [`fit::may_regrab`].
    user_released: Cell<bool>,
    /// Which mouse buttons are down (bitmask, [`btn_bit`]). A drag must neither release the grab
    /// (it would drop a guest window on the next display) nor let the policy take it.
    buttons: Cell<u8>,
    /// Whether the window is in PANEL fullscreen (`notch = extend`). That mechanism is a
    /// borderless window, not a Space, so it never sets `NSWindowStyleMask::FullScreen` — and
    /// `Borderless` is zero, so there is nothing to test for on the window itself. The grab has
    /// to hold there too: it is the mode most in need of it, being the one that owns the whole
    /// panel. Shared handle onto `window::PanelFullscreen`'s own flag.
    panel_fs: Arc<AtomicBool>,
}

/// One bit per mouse button, for [`TapCtx::buttons`].
fn btn_bit(etype: u32, button_number: i64) -> u8 {
    match etype {
        LMB_DOWN | LMB_UP => 1,
        RMB_DOWN | RMB_UP => 2,
        OMB_DOWN | OMB_UP if button_number == 2 => 4,
        _ => 0,
    }
}

impl TapCtx {
    /// Whether the guest window is showing fullscreen — either a real Space or the `notch =
    /// extend` panel.
    fn fullscreen(&self) -> bool {
        self.panel_fs.load(Ordering::Relaxed)
            || self
                .view
                .window()
                .is_some_and(|w| w.styleMask().contains(NSWindowStyleMask::FullScreen))
    }

    /// Forget everything about the current edge press and the re-grab dwell.
    fn reset_grab_gesture(&self) {
        self.grab_charge.set(fit::Charge::default());
        self.grab_edge.set(None);
        self.inside_since.set(None);
    }
}

/// A distance floor under the edge-press release, so pointer jitter against a clamped edge cannot
/// satisfy the hold. Deliberately small: the hold is what makes the gesture deliberate, and a
/// larger floor would quietly turn `Light` back into a distance gate — the unit this whole design
/// exists to get away from.
const GRAB_PUSH: f64 = 24.0;

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

    // Key-window tracking: the SOFT keyboard grab engages only while our window is key, and
    // a Ctrl-Opt mute of it lasts until the window regains key status (losing and retaking
    // focus is the natural "reset" — clicking back into the VM means you want it again).
    let is_key = ctx.view.window().is_some_and(|w| w.isKeyWindow());
    if is_key && !ctx.was_key.replace(true) {
        ctx.soft_muted.set(false);
        ctx.user_released.set(false);
    }
    if !is_key {
        ctx.was_key.set(false);
        // Losing key ALWAYS hands the pointer back. A background VM has no claim on it, and under
        // the fullscreen grab this is routine rather than exotic: our own close-policy Ask sheet,
        // a system alert, the Accessibility prompt. Without it the dialog is unclickable and the
        // cursor is hidden — the tap consumes every mouse event regardless of key state, and
        // nothing else in the app releases capture on focus loss. (An earlier draft of the design
        // claimed this was already handled. It was not.)
        if ctx.captured.load(Ordering::Acquire) {
            log::info!("pointer capture: released — the window lost focus");
            ctx.fullscreen_grab.set(false);
            ctx.reset_grab_gesture();
            ctx.input.toggle_capture(&ctx.view);
        }
    }

    // A POLICY grab lives only in fullscreen. Leaving it — Cmd-Ctrl-F, or the Space being torn
    // down — must hand the pointer back here, because the edge-press release is itself gated on
    // fullscreen: without this the pointer would be held in a windowed VM with no gesture that
    // could free it but the chord. An explicit Cmd-Ctrl-G grab is the user's and survives.
    if ctx.fullscreen_grab.get() && ctx.captured.load(Ordering::Acquire) && !ctx.fullscreen() {
        log::info!("pointer capture: released — no longer fullscreen");
        ctx.fullscreen_grab.set(false);
        ctx.reset_grab_gesture();
        ctx.input.toggle_capture(&ctx.view);
    }

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
            // GRAB only when our window is key: this is a SESSION tap, so it sees the combo
            // even while another app (or another VM) is focused — grabbing then steals the
            // user's mouse out from under whatever they were doing. Pass the combo through
            // instead so the focused party (possibly another VM's key-gated tap) handles it.
            // RELEASE is deliberately not gated — the escape hatch must always work.
            if now && !is_key {
                return event;
            }
            // The user is the other owner of `captured`. Without this latch a level-triggered
            // fullscreen policy would simply re-grab on the next event and Cmd-Ctrl-G would be a
            // no-op in fullscreen — the state the review flagged as two owners of one flag.
            ctx.user_released.set(!now);
            // An explicit grab is never the policy's, in any mode — see `fullscreen_grab`.
            ctx.fullscreen_grab.set(false);
            ctx.reset_grab_gesture();
            ctx.input.toggle_capture(&ctx.view);
            return std::ptr::null_mut(); // consume — the toggle never reaches macOS or the guest
        }
    }

    let captured = ctx.captured.load(Ordering::Acquire);
    // SOFT keyboard grab: while our window is key (and not Ctrl-Opt-muted), keyboard input —
    // including system combos like Cmd-Tab / Cmd-Space — goes to the guest, but the mouse
    // stays free (absolute mode via the local monitor, cursor can leave the window). Losing
    // key status disengages it instantly, so clicking anywhere else returns the keyboard to
    // the host with no chord needed.
    let soft = !captured && ctx.soft_enabled && is_key && !ctx.soft_muted.get();

    // Aux keys (the special/media top row, which arrives as NX_SYSDEFINED rather than as a
    // keycode — see `limina_input::auxkey`). Ownership is per BUCKET, not per grab mode alone:
    // media follows either grab, volume needs the full grab, brightness never leaves the host.
    // Anything the policy doesn't claim is returned untouched, so macOS still dims the screen.
    if etype == SYS_DEFINED {
        let mode = match (captured, soft) {
            (true, _) => GrabMode::Full,
            (false, true) => GrabMode::Soft,
            (false, false) => GrabMode::None,
        };
        return route_aux_event(ctx, event, mode);
    }

    // Keyboard: while captured (or soft-grabbed), system combos (Cmd-Tab, Cmd-Space,
    // Ctrl-arrows, fn keys, …) go to the GUEST, not macOS — forward through the shared
    // input translator (same remap, caps sync, and pressed-set bookkeeping as the local
    // monitor, so a focus-loss flush covers tap-forwarded keys too) and consume.
    // Capture-release (Cmd-Ctrl-G) was already handled above; the other host shortcuts pass
    // through so the local monitor still gets them.
    if matches!(etype, KEY_DOWN | KEY_UP | FLAGS_CHANGED) && (captured || soft) {
        let keycode = geti(FIELD_KEYBOARD_KEYCODE) as u16;
        let flags = unsafe { CGEventGetFlags(event) };
        match etype {
            KEY_DOWN => {
                // Any real key mid-chord means a guest combo (Ctrl-Alt-T…), not an ungrab.
                ctx.input.cancel_ungrab_chord();
                // Let our host shortcuts reach the local monitor (it toggles fullscreen/capture).
                if match_host_shortcut(keycode, flags).is_some() {
                    return event;
                }
                // A keycode with no guest mapping is dropped, NOT handed to macOS — see
                // `why_unmapped_keys_die_in_the_grab` below.
                if !ctx.input.maps_to_guest(keycode) {
                    log::debug!("input: dropped unmapped keycode {keycode:#04x} (grabbed)");
                    return std::ptr::null_mut();
                }
                // Skip autorepeat: the guest compositor generates its own key repeat from one press.
                if geti(FIELD_KEYBOARD_AUTOREPEAT) == 0 {
                    ctx.input.tap_key(keycode, true, flags);
                }
            }
            KEY_UP => {
                ctx.input.tap_key(keycode, false, flags);
            }
            _ => {
                // Ungrab chord (Ctrl+Option pressed and released alone). Captured: release
                // the grab (toggle_capture force-releases all guest modifiers, so the edges
                // this chord already forwarded can't stay wedged down). Soft: mute the soft
                // grab until the window regains key status, flushing modifiers the same way.
                match ctx.input.observe_ungrab_flags(keycode, flags) {
                    UngrabAction::Fire => {
                        if captured {
                            ctx.user_released.set(true);
                            ctx.fullscreen_grab.set(false);
                            ctx.reset_grab_gesture();
                            ctx.input.toggle_capture(&ctx.view);
                        } else {
                            ctx.soft_muted.set(true);
                            ctx.input.flush_modifiers();
                            log::info!(
                                "soft keyboard grab: muted (Ctrl-Opt) — host combos return \
                                 until the window regains focus"
                            );
                        }
                        return std::ptr::null_mut();
                    }
                    // The chord is armed: this edge is withheld from the guest until we know
                    // whether it was an ungrab or the head of a guest combo. Consume it.
                    UngrabAction::Withhold => return std::ptr::null_mut(),
                    UngrabAction::Forward => ctx.input.tap_flags(keycode, flags),
                }
            }
        }
        return std::ptr::null_mut(); // consume — the combo went to the guest
    }

    // Button bookkeeping for both paths: the grab must never be taken or released mid-drag.
    let bit = btn_bit(etype, geti(FIELD_BUTTON_NUMBER));
    if bit != 0 {
        let down = matches!(etype, LMB_DOWN | RMB_DOWN | OMB_DOWN);
        ctx.buttons.set(if down {
            ctx.buttons.get() | bit
        } else {
            ctx.buttons.get() & !bit
        });
    }

    if !captured {
        // Uncaptured: the local monitor drives absolute mode from the real host cursor, so the
        // event always passes through. Two things still happen here — the `notch = extend` chrome
        // ask is fed, and in fullscreen the policy may take the pointer into the grab.
        if matches!(etype, MOUSE_MOVED | LMB_DRAG | RMB_DRAG | OMB_DRAG) {
            return uncaptured_edges(ctx, event);
        }
        return event;
    }

    // Same snapshot rule as the keyboard path above: the Arc keeps the fd open across the sends.
    // Captured pointer traffic drives the ABSOLUTE tablet — the same device as uncaptured mode —
    // via the virtual cursor; the relative-mouse device carries only the edge-clamped overflow
    // as pressure (send_edge_overflow).
    let io = ctx.conn.io();
    let fd: RawFd = io.ptr_fd();
    let send = |ev: InputEvent| send_event(fd, ev);
    // Send the virtual cursor's absolute position (stepped by `(dx, dy)`, clamped to the
    // fit); motion the clamp eats goes to the relative device as edge pressure (hot corner).
    let send_pos = |dx: f64, dy: f64| {
        let fit = ctx.fit.get();
        let step = fit::capture_step(ctx.pos.get(), dx, dy, fit);
        ctx.pos.set(Some(step.pos));
        let (x, y) = fit::abs_through_fit(step.pos.0, step.pos.1, fit, ABS_MAX as i32);
        send(InputEvent::new(EV_ABS, ABS_X, x));
        send(InputEvent::new(EV_ABS, ABS_Y, y));
        send(InputEvent::syn());
        send_edge_overflow(&ctx.conn, step.overflow);
        step.pos
    };
    // A press re-sends the position first — same staleness guard as the uncaptured path.
    // Buttons also disarm the ungrab chord (clicking mid-chord = interacting, not ungrabbing).
    let send_click = |btn: u16, down: bool| {
        if down {
            ctx.input.cancel_ungrab_chord();
            let _ = send_pos(0.0, 0.0);
        }
        send(InputEvent::new(EV_KEY, btn, i32::from(down)));
        send(InputEvent::syn());
    };
    match etype {
        MOUSE_MOVED | LMB_DRAG | RMB_DRAG | OMB_DRAG => {
            // The deltas carry the pointer-ballistics-processed motion the macOS cursor would
            // have made, so integrating them moves the virtual cursor exactly like the host
            // cursor moves outside capture (the absolute device adds no guest acceleration).
            // Double-valued reads: the integer field truncates, which would eat slow sub-point
            // motion; the f64 position integrates fractions losslessly.
            let getd = |field: u32| unsafe { CGEventGetDoubleValueField(event, field) };
            let (dx, dy) = (getd(FIELD_DELTA_X), getd(FIELD_DELTA_Y));
            let pos = send_pos(dx, dy);
            if edge_trace() {
                eprintln!(
                    "[CAP] t={:.1} d=({dx:.1},{dy:.1}) -> pos=({:.1},{:.1})",
                    trace_ms(),
                    pos.0,
                    pos.1
                );
            }
            // Park the hidden cursor at centre so it can't reach a hot corner / screen edge.
            // NOTE: re-pinning every event fights any OTHER agent that also moves the macOS cursor
            // — notably a remote-desktop client driving this Mac — which reads as jitter under RD.
            // It works well locally. A cleaner containment scheme (how do VNC/RDP servers solve the
            // same capture problem?) is parked in docs/hardening-backlog.md.
            // Re-pin to the park point INSIDE our own window. Zero-length while the cursor is
            // disassociated, so unlike the old main-display-centre park it injects nothing.
            unsafe { CGWarpMouseCursorPosition(ctx.input.park_point()) };
            // Strictly AFTER the re-pin: the release does its own single warp, to just outside the
            // edge being pressed, and parking at centre on top of that would throw the pointer to
            // the middle of the screen at the exact moment the user is trying to leave.
            if let Some((edge, release)) = grab_release_edge(ctx, pos, dx, dy) {
                release_grab(ctx, pos, edge, release);
            }
        }
        LMB_DOWN => send_click(BTN_LEFT, true),
        LMB_UP => send_click(BTN_LEFT, false),
        RMB_DOWN => send_click(BTN_RIGHT, true),
        RMB_UP => send_click(BTN_RIGHT, false),
        OMB_DOWN | OMB_UP => {
            // Middle button only (buttonNumber 2); ignore higher buttons for now.
            if geti(FIELD_BUTTON_NUMBER) == 2 {
                send_click(BTN_MIDDLE, etype == OMB_DOWN);
            }
        }
        SCROLL => {
            // Straight to the shared scroll translator, via the same NSEvent bridge the aux keys
            // use. The tap used to read the two integer *line* fields directly, which is a legacy
            // one-notch-per-event mapping: no precise deltas, no v120 hi-res wheel, no momentum
            // phase. That was tolerable while capture was a short-lived mouselook mode entered on
            // purpose; the fullscreen grab makes captured scrolling the DEFAULT way a trackpad
            // scrolls in a fullscreen guest, and shipping the grab over this would have read as
            // "fullscreen broke scrolling". Bridging (rather than decoding the CG scroll fields
            // here) is deliberate: two owners of one translation is the mistake this file has
            // already made with the chrome ask.
            if let Some(ns) = NSEvent::eventWithCGEvent(unsafe { &*(event as *const CGEvent) }) {
                ctx.input.emit_scroll(&ns);
            }
        }
        _ => {}
    }
    std::ptr::null_mut() // consume — nothing escapes to host windows
}

// `why_unmapped_keys_die_in_the_grab` (the rule applied in the KEY_DOWN arm above)
//
// A grabbed tap drops a key it has no guest mapping for, rather than handing it back to macOS.
// That is deliberate and it is the *safe* direction, not merely the simple one.
//
// The tempting alternative — "we can't use it, so let the host have it" — is fail-dangerous. We
// cannot enumerate what an unknown key does on an arbitrary keyboard, and some of them are
// destructive: a keyboard with a reboot/sleep/eject key, pressed by a user who is aiming it at
// the *guest*, would act on the host instead. The user is grabbed at that moment and cannot
// ungrab fast enough to cancel it. A dropped key costs one keystroke and a retry; a host reboot
// costs the session. Note also that the genuine "recapture control" combos (force-quit, lock,
// power) run through secure-input paths a session tap never sees, so nothing safety-critical
// depends on our passing keys through.
//
// This is consistent with the aux buckets rather than in tension with them: those hand macOS
// only keys we have *identified and deliberately classified* (brightness stays host because we
// know it's brightness). The rule is knowledge-based — classify a key and route it on purpose,
// or drop it. Never forward blind.
//
// The cost is real and accepted: `spikes/fn-key-probe` found Mission Control, Spotlight,
// Dictation and Do Not Disturb (fn+F3–F6) arrive as ordinary keyDowns with keycodes
// 0xA0/0xB1/0xB0/0xB2 — a third mechanism, neither the NX_SYSDEFINED aux class nor anything in
// our keymap — and the Globe key (0xB3) is the same shape. All of them are inert while the VM
// window is focused. Ctrl-Opt (mute the soft grab) is the way to reach them. Promoting one to
// the guest later is a `keymap.rs` entry, NOT an `auxkey` bucket edit.

/// Say once per run that a media key went to the VM *under the soft grab*. That's the case
/// worth explaining: the soft grab engages on mere focus, with no gesture from the user, and
/// the failure mode is silent in both directions — if the guest has no media player listening,
/// the key does nothing there AND the host player never sees it, which reads as a dead key
/// rather than as a routing choice. Under a full grab the user explicitly took the keyboard
/// (and Ctrl-Opt means something else there — it drops capture entirely), so the message would
/// be both unsurprising and wrong about the way out.
fn warn_once_on_media_capture(nx_key: u16, mode: GrabMode) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if mode != GrabMode::Soft
        || nx_key_bucket(nx_key) != AuxBucket::Media
        || SAID.swap(true, Ordering::Relaxed)
    {
        return;
    }
    log::info!(
        "media keys are going to the VM while its window is focused (Ctrl-Opt mutes the soft \
         keyboard grab if you want them back on the host)"
    );
}

/// Decide one `NX_SYSDEFINED` event: forward it to the guest as an evdev key and consume it, or
/// hand it back for macOS to act on. Returning the event is the safe default at every step —
/// a subtype we don't understand, a `data1` shape we can't decode, or a key whose bucket the
/// current grab doesn't claim all keep working exactly as they do outside the VM.
fn route_aux_event(ctx: &TapCtx, event: CGEventRef, mode: GrabMode) -> CGEventRef {
    // Ungrabbed with nothing held is the overwhelmingly common case (every aux key pressed
    // while the VM isn't focused) — take it without bridging to NSEvent. It is NOT the same as
    // `mode == None`: a key pressed under a grab and released after focus moved away still has
    // to reach the guest, or it stays down there (the release-follows-press rule below). The
    // focus-loss flush would clean that up within a frame, but correctness shouldn't lean on a
    // 60 Hz poll when the check is one `is_empty`.
    if mode == GrabMode::None && !ctx.input.any_aux_pressed() {
        return event;
    }
    // SAFETY: inside a tap callback `event` is a live `CGEventRef`; `CGEvent` is the same
    // opaque CF type, borrowed only for this call (NSEvent copies what it needs).
    let cg: &CGEvent = unsafe { &*(event as *const CGEvent) };
    // The key lives in `NSEvent.data1`, which CoreGraphics exposes no field for — bridging to
    // NSEvent is the documented way to read it.
    let Some(ns) = NSEvent::eventWithCGEvent(cg) else {
        return event;
    };
    if ns.subtype().0 != NX_SUBTYPE_AUX_CONTROL_BUTTONS {
        return event; // window-server bookkeeping (screen changed, app activated, …), not a key
    }
    let Some(aux) = decode_aux_data1(ns.data1() as i64) else {
        return event;
    };
    // Policy, plus "a release always follows its press": a grab released mid-press must not
    // strand the key down in the guest (see `route_aux_event_key`).
    let held = nx_key_to_linux(aux.nx_key).is_some_and(|c| ctx.input.is_aux_pressed(c));
    let Some(code) = route_aux_event_key(aux.nx_key, mode, aux.down, held) else {
        return event; // wrong bucket for this grab, or no guest equivalent — macOS keeps it
    };
    warn_once_on_media_capture(aux.nx_key, mode);
    // Consume repeats without forwarding: the guest kernel repeats from the held-down state,
    // exactly as for ordinary keys. Still consumed, or macOS would act on the repeats alone.
    if !aux.repeat {
        ctx.input.cancel_ungrab_chord();
        ctx.input.tap_aux_key(code, aux.down);
    }
    std::ptr::null_mut()
}

thread_local! {
    /// A failed install's context, kept so [`retry_install`] can re-attempt the tap when
    /// Accessibility is granted mid-run. Main-thread only (like everything else here).
    static PENDING_CTX: Cell<*mut TapCtx> = const { Cell::new(std::ptr::null_mut()) };
}

/// Create + enable the tap for `ctx`. On success the tap owns `ctx` for the app's lifetime.
fn try_create(ctx: *mut TapCtx) -> bool {
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
        | (1 << SYS_DEFINED)
        | (1 << SCROLL)
        | (1 << OMB_DOWN)
        | (1 << OMB_UP)
        | (1 << OMB_DRAG);
    // tap=kCGSessionEventTap(1), place=kCGHeadInsertEventTap(0), options=kCGEventTapOptionDefault(0,
    // i.e. active/consuming).
    let port = unsafe { CGEventTapCreate(1, 0, 0, mask, tap_callback, ctx as *mut c_void) };
    if port.is_null() {
        return false;
    }
    TAP_PORT.store(port, Ordering::Release);
    unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
        CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
    }
    true
}

/// The uncaptured pointer in fullscreen: feed the chrome ask, and decide whether the grab takes
/// the pointer. Always passes the event through — nothing here consumes or warps.
///
/// The old edge *resistance* lived here and did both: it swallowed the motion that would have
/// crossed an edge and warped the host cursor back. That mechanism is gone
/// (`docs/design/fullscreen-pointer-grab.md`); a `CGEventTap` sees motion after the window server
/// has already moved the cursor, so "hold the pointer" could only ever mean "put it back", one
/// visible flick per event.
fn uncaptured_edges(ctx: &TapCtx, event: CGEventRef) -> CGEventRef {
    let fullscreen_and_key = ctx.view.window().is_some_and(|w| w.isKeyWindow()) && ctx.fullscreen();
    let duties = fit::edge_duties(fullscreen_and_key, ctx.hold > 0.0);
    if !duties.ask {
        // Not our pointer to think about. Drop any half-earned dwell so the *next* fullscreen
        // session cannot inherit it and grab on its first twitch.
        ctx.reset_grab_gesture();
        return event;
    }

    let fit = ctx.fit.get();
    let Some(cur) =
        super::input::cg_global_to_view_point(&ctx.view, unsafe { CGEventGetLocation(event) })
    else {
        return event;
    };
    let getd = |field: u32| unsafe { CGEventGetDoubleValueField(event, field) };

    // The chrome ask has exactly one implementation, in `InputState::reveal_step`, and the tap
    // defers to it. It used to have its own — the resistance breakthrough — which stayed
    // distance-based after the monitor's became a hold, so with the tap installed (i.e. whenever
    // Accessibility *is* granted) a two-event flick still summoned the menu bar. Two owners of
    // one gesture is how that happens; now there is one.
    //
    // Placement is load-bearing twice over, each learned by breaking it:
    //   - ABOVE the `duties.grab` return, so `Edge resist: Off` still leaves a way out of the
    //     overlay (see `fit::edge_duties`) — with the grab off there is no edge-press gesture at
    //     all, and this is the ONLY route to the menu bar;
    //   - UNCONDITIONAL, never `if overlaid`, because releasing the ask is the only thing that
    //     brings the overlay back, so skipping the call while it is down strands the guest below
    //     the camera housing. `reveal_step` tests the overlay itself, where arming needs it and
    //     releasing must not.
    ctx.input
        .reveal_step(cur, getd(FIELD_DELTA_Y), fit, RevealSrc::Tap);
    if !duties.grab {
        // `Off`: the pointer is free at every edge and behaves exactly as it does windowed. The
        // guest's own barriers still get their pressure — `emit_motion` forwards it, since the
        // event passes through to the window untouched.
        return event;
    }

    // Re-grab hysteresis. The pointer must come a real margin back INSIDE the content and stay
    // there, with no button down, before the grab retakes it. On a single display a released
    // cursor cannot leave the window at all — fullscreen *is* the screen, and the window server
    // pins it to the top row, which is still window territory — so re-grabbing on mere
    // containment would take the pointer back on the first inward jitter, warp it to centre and
    // hide it. That is the likeliest way this design ships worse than what it replaces.
    let now = std::time::Instant::now();
    if fit::deep_inside(cur, fit) {
        if ctx.inside_since.get().is_none() {
            ctx.inside_since.set(Some(now));
        }
    } else {
        ctx.inside_since.set(None);
    }
    let inside_for = ctx.inside_since.get().map(|t| now.duration_since(t));
    if edge_trace() {
        // Where the FREE pointer actually is, every event. This is what answers "the release put
        // it somewhere I did not push": compare the first few of these against the release target
        // logged by `release_grab`. A pointer that reappears far from that target did not travel
        // there — something moved it.
        eprintln!(
            "[EDGE] t={:.1} cur=({:.1},{:.1}) d=({:.1},{:.1}) fit=({:.1},{:.1} {:.1}x{:.1}) \
             deep={} inside_for={:?} latched={}",
            trace_ms(),
            cur.0,
            cur.1,
            getd(FIELD_DELTA_X),
            getd(FIELD_DELTA_Y),
            fit.x,
            fit.y,
            fit.w,
            fit.h,
            fit::deep_inside(cur, fit),
            inside_for.map(|d| d.as_millis()),
            ctx.user_released.get(),
        );
    }
    if !ctx.user_released.get() && fit::may_regrab(cur, fit, inside_for, ctx.buttons.get() != 0) {
        ctx.reset_grab_gesture();
        ctx.fullscreen_grab.set(true);
        // The virtual cursor the grab is about to hand the guest, BEFORE the toggle: if it
        // disagrees with `cur` (where the tap says the pointer is) the guest's cursor visibly
        // teleports at the instant of the grab, which is what dogfood saw on re-entry from the
        // other display. Logging both is the only way to tell a stale `pos` from a bad `cur`.
        if edge_trace() {
            eprintln!(
                "[GRAB] t={:.1} grabbing: cur=({:.1},{:.1}) pos={:?} \
                 fit=({:.1},{:.1} {:.1}x{:.1}) overlaid={}",
                trace_ms(),
                cur.0,
                cur.1,
                ctx.pos.get(),
                fit.x,
                fit.y,
                fit.w,
                fit.h,
                ctx.panel_fs.load(Ordering::Relaxed),
            );
        }
        ctx.input.toggle_capture(&ctx.view);
    }
    event
}

/// Whether any display contains this global point.
///
/// `CGWarpMouseCursorPosition` does not fail on a point that is off every display — it silently
/// **clamps into the display union**, so "just past this edge" lands wherever the arrangement
/// happens to put the nearest valid pixel. That is only half the reason this exists, though: even
/// with no warp at all, freeing the pointer at an edge with nothing beyond it hands it to the
/// window server still travelling, which carries it to whatever display *is* reachable. Measured
/// on a two-display Mac with the external screen low and to the right: a bottom-edge press
/// released in place and the pointer still ended up on the external display.
fn point_on_a_display(p: NSPoint) -> bool {
    let mut matching: u32 = 0;
    unsafe { CGGetDisplaysWithPoint(p, 0, std::ptr::null_mut(), &mut matching) };
    matching > 0
}

/// What a completed edge press does. Not every press lets the pointer go.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Release {
    /// Free the pointer and move it just past the edge, onto the display that is there.
    Out((f64, f64)),
    /// Free the pointer where it is — there is nothing beyond this edge to move onto, but the
    /// user still wants it back (to reach the guest's own dash at the bottom, say). `chrome` also
    /// asks for the macOS menu bar, which only the top edge does.
    InPlace { chrome: bool },
}

/// Decide what a sustained press at `edge` earns — **the pointer is only let go where there is
/// somewhere for it to go**.
///
/// This is the rule dogfood asked for after watching the first cut throw the cursor onto a display
/// that was neither above nor below anything: a press against a dead edge is not a request to
/// visit whichever screen the arrangement can reach. Three cases, and only one of them is generic:
///
///   - **Bottom: releases in place.** There is nothing below a fullscreen window to move onto, so
///     the pointer stays exactly where it is — but it is freed, because the guest's own dash lives
///     down there and poking at it takes a real cursor.
///   - **Top: releases in place, and asks for the chrome.** A fullscreen window's top edge IS the
///     top of the screen, so there is never anything above it — but this is the one edge whose
///     press means something other than "let me out". Under `notch = extend` it is the ask for the
///     menu bar, and the menu bar is a *host* affordance: it needs the host pointer, so the grab
///     has to end for it to be usable at all.
///   - **Sides: release only onto a real display**, checked at the exact point of the press, so an
///     arrangement where the neighbour spans only part of the edge behaves correctly along its
///     whole length.
fn release_for(ctx: &TapCtx, pos: (f64, f64), edge: fit::Edge) -> Option<Release> {
    let out = |edge| {
        let p = fit::release_point(pos, edge, ctx.fit.get());
        super::input::view_point_to_cg_global(&ctx.view, p)
            .filter(|g| point_on_a_display(*g))
            .map(|_| Release::Out(p))
    };
    match edge {
        // Below a fullscreen window there is nothing to move onto, but the pointer is still
        // wanted back — the guest's dash lives down there and poking at it needs a free cursor.
        fit::Edge::Bottom => Some(out(edge).unwrap_or(Release::InPlace { chrome: false })),
        fit::Edge::Top => Some(Release::InPlace { chrome: true }),
        fit::Edge::Left | fit::Edge::Right => out(edge),
    }
}

/// Let the pointer go, per [`release_for`]. At most one warp, and only ever onto a display that
/// exists — `toggle_capture` warps the host cursor to wherever the virtual one ended, so handing
/// it the release point IS the release warp. No second warp, and no separate code path that could
/// forget `end_warp_suppression`.
fn release_grab(ctx: &TapCtx, pos: (f64, f64), edge: fit::Edge, release: Release) {
    ctx.pos.set(Some(match release {
        Release::Out(p) => p,
        Release::InPlace { .. } => pos,
    }));
    ctx.reset_grab_gesture();
    ctx.fullscreen_grab.set(false);
    // The top edge is ONE gesture: under `notch = extend` the reason to press upward is to reach
    // the menu bar, so the same press that frees the pointer also puts the overlay down. Moving
    // back into the guest releases the ask and brings the overlay back, exactly as before.
    if release == (Release::InPlace { chrome: true }) {
        ctx.input.grant_chrome();
    }
    log::info!("pointer capture: released — sustained press at the {edge:?} edge ({release:?})");
    if edge_trace() {
        let target = ctx.pos.get();
        eprintln!(
            "[GRAB] t={:.1} releasing {edge:?} from ({:.1},{:.1}) to view {target:?} = cg {:?}",
            trace_ms(),
            pos.0,
            pos.1,
            target
                .and_then(|p| super::input::view_point_to_cg_global(&ctx.view, p))
                .map(|p| (p.x, p.y)),
        );
    }
    ctx.input.toggle_capture(&ctx.view);
}

/// Charge the edge press this captured motion represents, and answer with the edge if it has now
/// been held long enough to release the grab.
///
/// `pos` is the VIRTUAL cursor after the clamp, which is what makes this simple: it is exactly at
/// the edge while the deltas keep flowing, so a sustained push is a stream of genuinely-pushing
/// events with no "pinned, zero delta" case to special-case (the uncaptured chrome ask, driven by
/// a cursor the window server pins, has to).
fn grab_release_edge(
    ctx: &TapCtx,
    pos: (f64, f64),
    dx: f64,
    dy: f64,
) -> Option<(fit::Edge, Release)> {
    // Only the policy's grab. An explicit Cmd-Ctrl-G is an unconditional hold — see
    // `fullscreen_grab` — and the chord is its way out.
    if !ctx.fullscreen_grab.get() || ctx.hold <= 0.0 || !ctx.fullscreen() {
        return None;
    }
    // Mid-drag never releases: dragging a guest window against an edge would otherwise ungrab and
    // drop it on the next display. A press only counts once it starts after button-up.
    if ctx.buttons.get() != 0 {
        ctx.grab_charge.set(fit::Charge::default());
        return None;
    }
    let fit = ctx.fit.get();
    let Some(edge) = fit::pressed_edge(pos, dx, dy, fit) else {
        // Inside the content, or travelling along an edge rather than into it.
        let mut c = ctx.grab_charge.get();
        c.lapse();
        ctx.grab_charge.set(c);
        return None;
    };
    // Changing edges starts a new gesture rather than inheriting the old one's charge.
    if ctx.grab_edge.replace(Some(edge)) != Some(edge) {
        ctx.grab_charge.set(fit::Charge::default());
    }
    let push = match edge {
        fit::Edge::Left | fit::Edge::Right => dx.abs(),
        fit::Edge::Top | fit::Edge::Bottom => dy.abs(),
    };
    let mut c = ctx.grab_charge.get();
    let (charge, pushed) = c.push(std::time::Instant::now(), push);
    ctx.grab_charge.set(c);
    if edge_trace() {
        eprintln!(
            "[GRAB] t={:.1} press {edge:?} at ({:.1},{:.1}) d=({dx:.1},{dy:.1}) \
             charge={charge:.3}/{:.2} push={pushed:.1}",
            trace_ms(),
            pos.0,
            pos.1,
            ctx.hold,
        );
    }
    if charge < ctx.hold || pushed < GRAB_PUSH {
        return None;
    }
    // Earned it — but a press against a dead edge earns nothing, and must not sit there fully
    // charged waiting to fire the instant the pointer slides somewhere releasable. Lapse instead,
    // so sliding along an edge into a reachable stretch of it starts a fresh, deliberate press.
    let Some(release) = release_for(ctx, pos, edge) else {
        let mut c = ctx.grab_charge.get();
        c.lapse();
        ctx.grab_charge.set(c);
        return None;
    };
    Some((edge, release))
}

/// Whether the session event tap is live. The tap is session-wide, so when it is up it sees every
/// pointer event and the local monitor must not *also* drive anything stateful — see the reveal
/// call in `InputState::emit_motion`.
pub(crate) fn installed() -> bool {
    !TAP_PORT.load(Ordering::Relaxed).is_null()
}

/// Whether to log every resistance decision to stderr (`LIMINA_EDGE_TRACE=1`).
pub(crate) fn edge_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIMINA_EDGE_TRACE").is_some_and(|v| v != "0"))
}

/// Milliseconds since the first traced event, stamped on every trace line.
///
/// The trace is the recording instrument for gesture tuning, and the gestures are *timed* —
/// charge accumulates per unit of motion time. A stream of positions with no clock cannot say
/// whether two events are one stroke or two, so nothing in it can be used to pick a constant.
pub(crate) fn trace_ms() -> f64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
        * 1000.0
}

/// Install the capture tap on the main run loop. Returns `true` if the tap was created; `false`
/// if Accessibility permission is missing (capture then falls back to the local-monitor warp
/// path — leaky, but it still does *something* — and [`retry_install`] can pick the tap up
/// later). Call once, on the main thread, before the app run loop starts.
// Eight plumbing parameters, each a distinct shared handle the callback needs for the app's
// lifetime; bundling them into a struct would just move the same list one line up.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install(
    conn: Arc<WorkerConn>,
    captured: Arc<AtomicBool>,
    input: Rc<InputState>,
    soft_kbd_grab: bool,
    fit: Rc<Cell<FitRect>>,
    pos: Rc<Cell<Option<(f64, f64)>>>,
    view: Retained<NSView>,
    edge_resistance: f64,
    panel_fs: Arc<AtomicBool>,
) -> bool {
    let ctx = Box::into_raw(Box::new(TapCtx {
        captured,
        conn,
        input,
        soft_enabled: soft_kbd_grab,
        soft_muted: Cell::new(false),
        was_key: Cell::new(false),
        fit,
        pos,
        view,
        // One choke point for the unit. `[display] edge-resistance` is seconds now, but a
        // hand-edited vm.toml (or a `--edge-resistance 100` in someone's shell history) can still
        // carry the pre-2026-08 points encoding; `EdgeHold` migrates by preset position.
        hold: crate::vmlib::schema::EdgeHold::from_toml(edge_resistance).seconds(),
        grab_charge: Cell::new(fit::Charge::default()),
        grab_edge: Cell::new(None),
        inside_since: Cell::new(None),
        fullscreen_grab: Cell::new(false),
        user_released: Cell::new(false),
        buttons: Cell::new(0),
        panel_fs,
    }));
    if try_create(ctx) {
        log::info!("pointer capture: CGEventTap installed (session-level, consuming)");
        return true;
    }
    log::warn!(
        "pointer capture: CGEventTap unavailable — grant Accessibility permission (System \
         Settings → Privacy & Security → Accessibility) for reliable capture; falling back \
         to the leaky warp path (will retry on each Cmd-Ctrl-G)"
    );
    PENDING_CTX.with(|p| p.set(ctx));
    false
}

/// Re-attempt a failed [`install`] — an Accessibility grant given mid-run takes effect on a
/// fresh `CGEventTapCreate`, no process restart needed. Returns whether the tap is installed
/// after the call (`true` when it already was). Cheap when there is nothing to retry. Main
/// thread only.
pub(crate) fn retry_install() -> bool {
    let ctx = PENDING_CTX.with(|p| p.replace(std::ptr::null_mut()));
    if ctx.is_null() {
        return !TAP_PORT.load(Ordering::Acquire).is_null();
    }
    if try_create(ctx) {
        log::info!("pointer capture: CGEventTap installed on retry (Accessibility granted)");
        return true;
    }
    PENDING_CTX.with(|p| p.set(ctx));
    false
}

/// Raise the system Accessibility prompt, once per process run. Called on the first capture
/// toggle that engages WITHOUT the tap: the prompt both tells the user why capture is degraded
/// and registers the app in the Accessibility list (no hunting with the "+" button); after
/// granting, the next Cmd-Ctrl-G heals live via [`retry_install`].
///
/// Returns `true` iff it actually raised the dialog this call (i.e. the first tap-less toggle),
/// `false` on every later call (already prompted). The caller uses this to NOT grab the pointer
/// on the toggle that opened the dialog — a captured cursor is parked at screen centre and
/// consumed, so the user could not click the dialog to grant the permission.
pub(crate) fn prompt_accessibility_once() -> bool {
    static PROMPTED: AtomicBool = AtomicBool::new(false);
    if PROMPTED.swap(true, Ordering::Relaxed) {
        return false;
    }
    log::warn!(
        "pointer capture: engaging WITHOUT the consuming tap — system key combos (Cmd-Tab, \
         media keys) will leak to macOS; raising the Accessibility prompt"
    );
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks as *const c_void,
            &kCFTypeDictionaryValueCallBacks as *const c_void,
        );
        let _ = AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
    }
    true
}
