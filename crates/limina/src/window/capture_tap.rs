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
use objc2_foundation::{NSPoint, NSRect};

use limina_input::auxkey::{
    decode_aux_data1, nx_key_bucket, nx_key_to_linux, route_aux_event_key, AuxBucket, GrabMode,
    NX_SUBTYPE_AUX_CONTROL_BUTTONS,
};
use limina_input::constants::{
    ABS_MAX, ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL,
    REL_WHEEL,
};
use limina_input::InputEvent;

use super::fit::{self, FitRect};
use super::input::{
    match_host_shortcut, send_abs_position, send_edge_overflow, send_event, HostShortcut,
    InputState, UngrabAction,
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
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> NSRect;
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
    /// Fullscreen edge resistance for the UNCAPTURED pointer (`[display] edge-resistance`).
    /// Disabled (threshold 0) unless configured; see [`fit::EdgeResist`].
    resist: Cell<fit::EdgeResist>,
    /// Whether the window is in PANEL fullscreen (`notch = extend`). That mechanism is a
    /// borderless window, not a Space, so it never sets `NSWindowStyleMask::FullScreen` — and
    /// `Borderless` is zero, so there is nothing to test for on the window itself. Resistance
    /// has to hold there too: it is the mode most in need of it, being the one that owns the
    /// whole panel. Shared handle onto `window::PanelFullscreen`'s own flag.
    panel_fs: Arc<AtomicBool>,
    /// Set when the user shoves at the top edge hard enough to break through while the overlay
    /// is up: the deliberate "let me at the menu bar" gesture. Nothing can reveal over the
    /// overlay, so the only way to reach the chrome (and the VM's own menu) is to put the overlay
    /// down — which `ExtendOverlay::reconcile` does while this is set. Cleared as soon as the
    /// pointer is back inside the guest. Uncaptured only, so a grabbed pointer can never trip it.
    reveal_chrome: Arc<AtomicBool>,
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

    // Key-window tracking: the SOFT keyboard grab engages only while our window is key, and
    // a Ctrl-Opt mute of it lasts until the window regains key status (losing and retaking
    // focus is the natural "reset" — clicking back into the VM means you want it again).
    let is_key = ctx.view.window().is_some_and(|w| w.isKeyWindow());
    if is_key && !ctx.was_key.replace(true) {
        ctx.soft_muted.set(false);
    }
    if !is_key {
        ctx.was_key.set(false);
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

    if !captured {
        // Uncaptured: the local monitor drives absolute mode from the real host cursor, so the
        // event passes through — EXCEPT that in fullscreen we make the guest's edges resist.
        // Without this the cursor reaching the top edge instantly reveals the macOS menu bar and
        // title bar, and a side edge silently hands the pointer to the next display; both are one
        // flick away and make a fullscreen guest feel leaky. See [`fit::EdgeResist`].
        if matches!(etype, MOUSE_MOVED | LMB_DRAG | RMB_DRAG | OMB_DRAG) {
            return resist_edges(ctx, event);
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
    };
    // A press re-sends the position first — same staleness guard as the uncaptured path.
    // Buttons also disarm the ungrab chord (clicking mid-chord = interacting, not ungrabbing).
    let send_click = |btn: u16, down: bool| {
        if down {
            ctx.input.cancel_ungrab_chord();
            send_pos(0.0, 0.0);
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
            send_pos(getd(FIELD_DELTA_X), getd(FIELD_DELTA_Y));
            // Park the hidden cursor at centre so it can't reach a hot corner / screen edge.
            // NOTE: re-pinning every event fights any OTHER agent that also moves the macOS cursor
            // — notably a remote-desktop client driving this Mac — which reads as jitter under RD.
            // It works well locally. A cleaner containment scheme (how do VNC/RDP servers solve the
            // same capture problem?) is parked in docs/hardening-backlog.md.
            unsafe { CGWarpMouseCursorPosition(main_display_center()) };
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
            ctx.input.cancel_ungrab_chord();
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

/// Apply fullscreen edge resistance to one uncaptured motion event.
///
/// Returns the event to pass through, or NULL to consume it. Consuming is what makes the
/// resistance real: we swallow the motion that would have crossed the edge and warp the host
/// cursor back to the boundary, which itself generates a motion event that the local monitor
/// turns into the guest's pointer position — so the guest still tracks the cursor exactly, it
/// just cannot be pushed out of the guest's own screen by accident.
///
/// Resistance applies ONLY while fullscreen and key. Windowed, the pointer must be free to leave
/// (it is a window), and an unfocused window has no business holding the pointer at all.
fn resist_edges(ctx: &TapCtx, event: CGEventRef) -> CGEventRef {
    let mut resist = ctx.resist.get();
    if !resist.enabled() {
        return event;
    }
    let fullscreen_and_key = ctx.view.window().is_some_and(|w| {
        (ctx.panel_fs.load(Ordering::Relaxed)
            || w.styleMask().contains(NSWindowStyleMask::FullScreen))
            && w.isKeyWindow()
    });
    if !fullscreen_and_key {
        // Drop any half-earned push: a shove accumulated in fullscreen must not let the pointer
        // out of the *next* fullscreen session on its first twitch.
        resist.reset();
        ctx.resist.set(resist);
        return event;
    }

    // Where the cursor actually IS, read off the event. Not `ctx.pos`: that is maintained by the
    // window's own motion handler, which sees nothing while we are consuming events and nothing
    // at all once the pointer has left the window for another display — so it freezes on the
    // edge, and steering by it used to drag an escaped pointer back into the guest.
    let fit = ctx.fit.get();
    let Some(cur) =
        super::input::cg_global_to_view_point(&ctx.view, unsafe { CGEventGetLocation(event) })
    else {
        return event;
    };
    // Under the overlay there is nothing to protect the top edge FROM — the chrome cannot appear
    // over it — so drop the keep-out band and let the pointer reach the guest's own top edge,
    // which is where its top bar and top-left hot corner want it.
    let overlaid = ctx.panel_fs.load(Ordering::Relaxed);
    resist.set_keepout(!overlaid);
    // Back inside the guest: whatever the chrome was doing, we want the overlay again. The same
    // margin the local monitor uses, not mere containment — `point_in_fit` includes the top row,
    // so re-entering there would snap the overlay back over the very menu the user reached for.
    if fit::point_in_fit(cur.0, cur.1, fit) && cur.1 < fit.y + fit.h - super::input::REVEAL_MARGIN {
        ctx.reveal_chrome.store(false, Ordering::Relaxed);
    }
    let getd = |field: u32| unsafe { CGEventGetDoubleValueField(event, field) };
    let (dx, dy) = (getd(FIELD_DELTA_X), getd(FIELD_DELTA_Y));
    let step = resist.step(cur, dx, dy, fit);
    ctx.resist.set(resist);
    if edge_trace() {
        // The host half of the edge oracle. The guest half is its own evdev stream — see
        // `spikes/edge-pressure/`. Together they answer the only question that matters when a
        // guest-side barrier won't fire: did we absorb the push and forward it, or did we let go
        // first? Reasoning about it from the outside got the corner threshold wrong twice.
        eprintln!(
            "[EDGE] cur=({:.1},{:.1}) d=({dx:.1},{dy:.1}) fit=({:.1},{:.1} {:.1}x{:.1}) \
             overlaid={overlaid} free={} overflow=({:.1},{:.1}) revealed={}",
            cur.0,
            cur.1,
            fit.x,
            fit.y,
            fit.w,
            fit.h,
            step.free,
            step.overflow.0,
            step.overflow.1,
            step.revealed,
        );
    }
    // The chrome ask has exactly one implementation, in `InputState::reveal_step`, and the tap
    // defers to it. It used to have its own — the resistance breakthrough — which stayed
    // distance-based after the monitor's became a hold, so with the tap installed (i.e. whenever
    // Accessibility *is* granted) a two-event flick still summoned the menu bar. Two owners of one
    // gesture is how that happens; now there is one.
    if overlaid {
        ctx.input.reveal_step(cur, dy, fit);
    }
    if step.free {
        return event;
    }

    // Held at the edge: park the host cursor just inside it and hand the guest the motion we ate,
    // so mutter's pressure barriers (GNOME hot corner, a guest panel's reveal edge) still fire.
    //
    // Pin the guest's pointer to the clamped position first. While we are consuming events the
    // monitor never runs, so the last absolute position the guest saw is the one from *before*
    // the clamp — several tens of points short of the edge. The guest then spends the first part
    // of the forwarded push walking its cursor to the corner instead of charging the barrier
    // there, and a shove measured at 142 px arrived as ~90 px of pressure: just under mutter's
    // 100, which is why the hot corner worked one time in three.
    send_abs_position(&ctx.conn, step.pos, fit);
    if step.pos != cur {
        if let Some(p) = super::input::view_point_to_cg_global(&ctx.view, step.pos) {
            unsafe { CGWarpMouseCursorPosition(p) };
            super::input::end_warp_suppression();
        }
    }
    send_edge_overflow(&ctx.conn, step.overflow);
    std::ptr::null_mut()
}

/// Whether to log every resistance decision to stderr (`LIMINA_EDGE_TRACE=1`).
fn edge_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIMINA_EDGE_TRACE").is_some_and(|v| v != "0"))
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
    reveal_chrome: Arc<AtomicBool>,
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
        resist: Cell::new(fit::EdgeResist::new(edge_resistance)),
        panel_fs,
        reveal_chrome,
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
