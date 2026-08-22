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
use objc2_app_kit::{NSEvent, NSView};
use objc2_core_graphics::CGEvent;
use objc2_foundation::NSPoint;

use limina_input::auxkey::{
    decode_aux_data1, nx_key_bucket, nx_key_to_linux, route_aux_event_key, AuxBucket, GrabMode,
    NX_SUBTYPE_AUX_CONTROL_BUTTONS,
};
use limina_input::constants::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY};
use limina_input::InputEvent;

use super::fit;
use super::grab_policy::{self, Release};
use super::input::{
    match_host_shortcut, send_event, HostShortcut, InputState, RevealSrc, UngrabAction,
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
    /// Where the event says the cursor is, in CG global coordinates (top-left origin), *after*
    /// this motion was applied — the authoritative position, including the window server's own
    /// clamping to the union of the displays.
    fn CGEventGetLocation(event: CGEventRef) -> NSPoint;
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
    /// The guest view, for mapping the virtual cursor back to screen coordinates on release.
    view: Retained<NSView>,
    /// Seconds of sustained edge press that release the fullscreen grab (`[display]
    /// edge-resistance`, migrated to a duration — see [`crate::vmlib::schema::EdgeHold`]).
    /// Zero means `Off`: never grab, exactly today's free pointer.
    hold: f64,
    /// Which mouse buttons are down (bitmask, [`btn_bit`]). A drag must neither release the grab
    /// (it would drop a guest window on the next display) nor let the policy take it.
    buttons: Cell<u8>,
    /// Whether the window is in PANEL fullscreen (`notch = extend`). That mechanism is a
    /// borderless window, not a Space, so it never sets `NSWindowStyleMask::FullScreen` — and
    /// `Borderless` is zero, so there is nothing to test for on the window itself. The
    /// *judgments* read this through the facts snapshot (`WindowFacts::fullscreen` folds it in
    /// — `InputState` shares the same flag); this handle remains only for the `[GRAB]` trace's
    /// `overlaid` column.
    panel_fs: Arc<AtomicBool>,
}

/// What a Ctrl+Opt ungrab-chord fire does to each grab, given whether the pointer is captured
/// (an explicit hard grab or the fullscreen policy grab — the chord doesn't distinguish) at the
/// moment it fires. Pure so the two-grab sequence is testable without a CGEvent in sight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChordFire {
    /// Latch the policy against an instant re-grab and toggle the pointer capture off.
    release_pointer: bool,
    /// Mute the soft keyboard grab until the window regains key status.
    mute_soft: bool,
}

fn chord_fire(captured: bool) -> ChordFire {
    ChordFire {
        release_pointer: captured,
        // ALWAYS mute, not just on a soft-only fire: the chord means "give me the host back",
        // and after a captured fire (fullscreen policy grab, typically) the window is still key,
        // so an unmuted soft grab would swallow the host combo the user chorded to reach —
        // Ctrl held through the chord + arrow for a workspace switch went to the guest instead,
        // and the chord had to be pressed twice, once per grab. The mute self-heals: regaining
        // key status (clicking back into the VM) clears it.
        mute_soft: true,
    }
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
    /// Read-modify-write the policy state — the only way this file touches it.
    fn with_grab<T>(&self, f: impl FnOnce(&mut grab_policy::GrabState) -> T) -> T {
        // The state lives on `InputState`: the tick's screen-gain trigger needs it too, and it
        // fires when no event is arriving here at all.
        self.input.with_grab(f)
    }
}

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user: *mut c_void,
) -> CGEventRef {
    // The system disables the tap on timeout / certain user input — re-enable and pass through.
    //
    // Say so. Between the disable and this callback, every real event reached the app without
    // passing here at all: no grab taken, no grab stood down, and — before this line — not one
    // word in the log. That is indistinguishable from a click the policy considered and
    // refused, so a silent re-enable turns a system-level hiccup into a limina bug hunt.
    if etype == DISABLED_TIMEOUT || etype == DISABLED_USERINPUT {
        let port = TAP_PORT.load(Ordering::Acquire);
        if !port.is_null() {
            unsafe { CGEventTapEnable(port, true) };
        }
        log::warn!(
            "pointer capture: the system disabled our event tap ({}) — re-enabled; events in the gap reached the app untapped, so a click there took no grab",
            if etype == DISABLED_TIMEOUT {
                "we were too slow answering it"
            } else {
                "user input"
            }
        );
        return event;
    }
    // SAFETY: `user` is the leaked `TapCtx` from `install`; the callback only runs on the main
    // run loop while that allocation is alive (the app's lifetime).
    let ctx = unsafe { &*(user as *const TapCtx) };

    // Parked window (task #18): the guest is suspended, so the tap must claim NOTHING — no
    // soft keyboard grab (Cmd-W has to close the window, Cmd-Q quit, media keys stay
    // macOS's), no pointer bookkeeping. Live-caught: Cmd-W on a parked window vanished into
    // the dead worker's input fd because this tap, not the parked-aware NSEvent monitor,
    // consumes grabbed combos. (Same thread as the render timer — the main run loop — so
    // the phase read is safe.)
    if super::parked() {
        return event;
    }

    let geti = |field: u32| unsafe { CGEventGetIntegerValueField(event, field) };

    // ONE ownership snapshot for this event ([`InputState::window_facts`]); every judgment
    // below reads it, so the tap cannot answer "is this window ours to act for" two ways in
    // one event.
    let facts = ctx.input.window_facts(&ctx.view);
    let pf = grab_policy::primary_facts(&facts);
    // Key-window tracking: the SOFT keyboard grab engages only while our window is key, and
    // a Ctrl-Opt mute of it lasts until the window regains key status (losing and retaking
    // focus is the natural "reset" — clicking back into the VM means you want it again). The
    // POINTER grab's explicit-release latch is not cleared here: its re-arm is a click on
    // guest content (`grab_policy::free_step`), which says the same thing directly.
    // "Does the VM have the keyboard", not "does the primary" — clicking a secondary makes IT
    // key, and that is focus moving inside the VM, not away from it.
    let is_key = facts.iter().any(|f| f.key);
    // Our window's Space being on screen is a *different* question from key status, which a
    // window keeps across a Space switch. Both grabs need it (see `grab_policy`).
    let space_visible = pf.on_active_space;
    if is_key && !ctx.was_key.replace(true) {
        ctx.soft_muted.set(false);
    }
    if !is_key {
        ctx.was_key.set(false);
        // Losing key ALWAYS hands the pointer back — see `grab_policy::key_loss_releases`.
        // The tap consumes every mouse event regardless of key state, and this per-event check
        // is the low-latency consumer (the window tick is the backstop). (An earlier draft of
        // the design claimed this was already handled. It was not.)
        if grab_policy::key_loss_releases(ctx.captured.load(Ordering::Acquire), &facts) {
            log::info!("pointer capture: released — the window lost focus");
            ctx.with_grab(|st| st.stop_holding());
            ctx.input.toggle_capture(&ctx.view);
        }
    }

    // A POLICY grab lives only in fullscreen — see `grab_policy::fullscreen_exit_releases`.
    // An explicit Cmd-Ctrl-G grab is the user's and survives.
    if grab_policy::fullscreen_exit_releases(
        ctx.input.grab_state().holding(),
        ctx.captured.load(Ordering::Acquire),
        &pf,
    ) {
        log::info!("pointer capture: released — no longer fullscreen");
        ctx.with_grab(|st| st.stop_holding());
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
            // Cmd-Ctrl-G toggles the HARD grab, not `captured`.
            //
            // Toggling `captured` was unusable once the fullscreen policy existed, and dogfood hit
            // it immediately: in fullscreen the policy holds the pointer nearly all the time, so
            // the combo always found it grabbed and always released — and by the time a second
            // press arrived the policy had retaken it, so the two states just alternated and the
            // hard grab was unreachable. Making the combo mean "hard grab / no grab" gives each
            // press one meaning: from soft or policy-held it becomes a hard grab, from a hard grab
            // it lets go. (Ctrl-Opt still always releases.)
            let held = ctx.captured.load(Ordering::Acquire);
            let hard = held && !ctx.input.grab_state().holding();
            // GRAB only when our window is key: this is a SESSION tap, so it sees the combo
            // even while another app (or another VM) is focused — grabbing then steals the
            // user's mouse out from under whatever they were doing. Pass the combo through
            // instead so the focused party (possibly another VM's key-gated tap) handles it.
            // RELEASE is deliberately not gated — the escape hatch must always work.
            if !hard && !is_key {
                return event;
            }
            // The user is the other owner of `captured`. Without the latch a level-triggered
            // fullscreen policy would simply re-grab on the next event — the state the review
            // flagged as two owners of one flag. A *promotion* latches nothing: the pointer stays
            // held, the policy just stops owning it.
            ctx.with_grab(|st| st.release_by_user(hard));
            if held != hard {
                // Already held, but by the policy: promote it in place. No capture transition —
                // the pointer does not move, the guest sees nothing, only the terms change.
                log::info!(
                    "pointer capture: promoted to a hard grab (Ctrl-Opt or Cmd-Ctrl-G to release)"
                );
            } else {
                ctx.input.toggle_capture(&ctx.view);
            }
            return std::ptr::null_mut(); // consume — the toggle never reaches macOS or the guest
        }
    }

    let captured = ctx.captured.load(Ordering::Acquire);
    // SOFT keyboard grab: while our window is key (and not Ctrl-Opt-muted), keyboard input —
    // including system combos like Cmd-Tab / Cmd-Space — goes to the guest, but the mouse
    // stays free (absolute mode via the local monitor, cursor can leave the window). Losing
    // key status disengages it instantly, so clicking anywhere else returns the keyboard to
    // the host with no chord needed.
    let soft = grab_policy::soft_keyboard_engaged(
        captured,
        ctx.soft_enabled,
        is_key,
        ctx.soft_muted.get(),
        space_visible,
    );

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

    // The chord keeps its meaning after it has muted the soft grab — it does not go deaf the
    // instant it fires. Gated on the mute (not merely on "ungrabbed"), so this claims Ctrl+Option
    // only in the state the chord itself created: a user who runs with `--no-soft-kbd-grab` and
    // no pointer grab keeps Ctrl+Option as an ordinary guest combo.
    //
    // Why it is needed: with the Cmd/Option swap on (the default) the guest's Super IS macOS's
    // Option, so "Ctrl held + Super" and the ungrab chord are the same physical gesture. Once the
    // first fire muted the soft grab, every later press with Control still held fell through to
    // the local monitor and reached the guest as a bare Super — GNOME's overview, twice over.
    // Reported 2026-08-09; evidence in `spikes/modifier-drift/`.
    if grab_policy::chord_survives_mute(captured, soft, ctx.soft_muted.get(), is_key, space_visible)
        && etype == FLAGS_CHANGED
    {
        let keycode = geti(FIELD_KEYBOARD_KEYCODE) as u16;
        let flags = unsafe { CGEventGetFlags(event) };
        match ctx.input.observe_ungrab_flags_ungrabbed(keycode, flags) {
            // No grab is left to drop and nothing needs flushing: this path never forwards a
            // chord edge to the guest, so the only modifiers it holds are ones the resync put
            // there because the user really is holding them. A flush here would release them and
            // the next edge would immediately put them back — the trace of the first fix showed
            // exactly that, one spurious Control release/press pair per chord. Consuming the
            // event is the whole job.
            UngrabAction::Fire | UngrabAction::Withhold => return std::ptr::null_mut(),
            // Not chord business — fall through and leave the event to macOS and the monitor.
            UngrabAction::Forward => {}
        }
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
                // Ungrab chord (Ctrl+Option pressed and released alone) = "give me the host
                // back": what it does to each grab is [`chord_fire`]'s call. Releasing the
                // pointer goes through toggle_capture, which force-releases all guest modifiers
                // (so the edges this chord already forwarded can't stay wedged down); a
                // soft-only fire has no capture transition to do that, so it flushes directly.
                // The soft mute lasts until the window regains key status.
                match ctx.input.observe_ungrab_flags(keycode, flags) {
                    UngrabAction::Fire => {
                        let fire = chord_fire(captured);
                        if fire.release_pointer {
                            ctx.with_grab(|st| st.release_by_user(true));
                            ctx.input.toggle_capture(&ctx.view);
                        } else {
                            ctx.input.flush_modifiers();
                        }
                        if fire.mute_soft {
                            ctx.soft_muted.set(true);
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
        // ...and into the shared state, which the local monitor cannot keep current while the
        // tap is consuming these events. See `InputState::note_tap_button`.
        ctx.input.note_tap_button(bit, down);
    }

    if !captured {
        // Uncaptured: the local monitor drives absolute mode from the real host cursor, so the
        // event passes through. Two things still happen here — the `notch = extend` chrome ask
        // is fed, and in fullscreen the policy may take the pointer into the grab: silently
        // after the dwell, or at once on a click, which is the user's explicit ask.
        let is_down = matches!(etype, LMB_DOWN | RMB_DOWN | OMB_DOWN);
        if !is_down && !matches!(etype, MOUSE_MOVED | LMB_DRAG | RMB_DRAG | OMB_DRAG) {
            return event;
        }
        let (event, grabbed) = uncaptured_edges(ctx, event, &facts, is_down);
        // EVERY press the tap sees, at info, whatever happened to it — including the ones
        // refused before the grab policy ever ran. A click that does nothing used to print
        // nothing, which left "clicking doesn't grab" with no evidence at all; the facts that
        // decide it are cheap and a click is user-paced, so there is no reason to ration them.
        if is_down {
            let loc = unsafe { CGEventGetLocation(event) };
            log::info!(
                "pointer capture: click at ({:.0},{:.0}) — grabbed={grabbed}; fullscreen={} key={} space={} on-screen={} grab-enabled={} latched={}",
                loc.x,
                loc.y,
                pf.fullscreen,
                pf.key,
                pf.on_active_space,
                pf.has_screen,
                ctx.hold > 0.0,
                ctx.input.grab_state().user_released(),
            );
        }
        if !(grabbed && is_down) {
            return event;
        }
        // A click took the grab. This very press is delivered through the captured path below
        // — position re-send, then the button — and consumed, so the guest sees exactly one
        // press and macOS none; AppKit never learns of a click that started a capture.
    }

    // Same snapshot rule as the keyboard path above: the Arc keeps the fd open across the sends.
    // Captured pointer traffic drives the ABSOLUTE tablet — the same device as uncaptured mode —
    // via the virtual cursor in the capture window. The stepping, mapping, and edge-pressure
    // forwarding all live in `InputState::captured_step_and_emit`, shared with the degraded
    // no-tap path, so the tap and the monitor cannot map differently by construction.
    let io = ctx.conn.io();
    let fd: RawFd = io.ptr_fd();
    let send = |ev: InputEvent| send_event(fd, ev);
    // A press re-sends the position first — same staleness guard as the uncaptured path.
    // Buttons also disarm the ungrab chord (clicking mid-chord = interacting, not ungrabbing).
    let send_click = |btn: u16, down: bool| {
        if down {
            ctx.input.cancel_ungrab_chord();
            let _ = ctx.input.captured_step_and_emit(0.0, 0.0, &ctx.view);
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
            let (rdx, rdy) = (getd(FIELD_DELTA_X), getd(FIELD_DELTA_Y));
            // A re-park's injected delta rides one of the first real motion events after the
            // warp — detected and subtracted here, where CG deltas are actually read (the
            // synthetic click re-send must never consume this). See `WarpSwallow`.
            let (dx, dy) = ctx.input.swallow_warp(rdx, rdy);
            let step = ctx.input.captured_step_and_emit(dx, dy, &ctx.view);
            if edge_trace() {
                if let Some(s) = &step {
                    eprintln!(
                        "[CAP] t={:.1} d=({dx:.1},{dy:.1}) -> slot={} view=({:.1},{:.1}) range=({:.0},{:.0})",
                        trace_ms(),
                        s.slot,
                        s.view_point.0,
                        s.view_point.1,
                        s.range.0,
                        s.range.1,
                    );
                }
            }
            // Park the hidden cursor so it can't reach a hot corner / screen edge.
            // NOTE: re-pinning every event fights any OTHER agent that also moves the macOS cursor
            // — notably a remote-desktop client driving this Mac — which reads as jitter under RD.
            // It works well locally. A cleaner containment scheme (how do VNC/RDP servers solve the
            // same capture problem?) is parked in docs/hardening-backlog.md.
            // Re-pin to the park point INSIDE our own window. Zero-length while the cursor is
            // disassociated, so unlike the old main-display-centre park it injects nothing.
            if warp_probe() {
                let (last, sign, count) = WARP_PROBE_STATE.with(|c| c.get());
                if count < 12 {
                    eprintln!(
                        "[WARPPROBE] t={:.1} n={count} d=({dx:.2},{dy:.2})",
                        trace_ms()
                    );
                }
                let due = last.is_none_or(|t| t.elapsed().as_secs_f64() > 3.0);
                let park = ctx.input.park_point();
                if due {
                    let target = NSPoint::new(park.x + 200.0 * sign, park.y);
                    eprintln!(
                        "[WARPPROBE] t={:.1} WARP vec=({:.0},0) park=({:.0},{:.0})",
                        trace_ms(),
                        200.0 * sign,
                        park.x,
                        park.y,
                    );
                    ctx.input.warp_probe_to(target);
                    WARP_PROBE_STATE.with(|c| c.set((Some(std::time::Instant::now()), -sign, 0)));
                } else {
                    ctx.input.repin_park(&ctx.view);
                    WARP_PROBE_STATE.with(|c| c.set((last, sign, count.saturating_add(1))));
                }
            } else {
                ctx.input.repin_park(&ctx.view);
            }
            // Strictly AFTER the re-pin: the release does its own single warp, to just outside the
            // edge being pressed, and parking at centre on top of that would throw the pointer to
            // the middle of the screen at the exact moment the user is trying to leave.
            //
            // The press and release are judged in the CAPTURE window's view space — the window
            // the grab was taken in, whose fit clamps the virtual cursor. Every edge of that
            // fit can pin and so can press, a seam to a neighbouring guest window included.
            if let Some(s) = &step {
                if let Some((edge, release)) = grab_release_edge(ctx, s, dx, dy, pf.fullscreen) {
                    release_grab(ctx, s, edge, release);
                }
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
fn uncaptured_edges(
    ctx: &TapCtx,
    event: CGEventRef,
    facts: &[grab_policy::WindowFacts],
    click: bool,
) -> (CGEventRef, bool) {
    let loc = unsafe { CGEventGetLocation(event) };
    // Resolve the guest window UNDER the pointer: the grab spans every covered panel, so the
    // regrab is judged against whichever window's content the free pointer is over — that is
    // what extends capture beyond the primary. Off every guest content, fall back to the
    // primary's (affine, out-of-fit) conversion, which is exactly the "outside" `free_step`
    // needs to disarm and to time the dwell.
    let (fit, cur, hit_slot) = match ctx.input.guest_surface_at_global(loc, &ctx.view) {
        Some(hit) => (hit.fit, hit.point, Some(hit.slot)),
        None => {
            let Some(cur) = super::input::cg_global_to_view_point(&ctx.view, loc) else {
                return (event, false);
            };
            (ctx.input.primary_fit(), cur, None)
        }
    };
    let getd = |field: u32| unsafe { CGEventGetDoubleValueField(event, field) };
    let arming = grab_policy::free_arming(facts, hit_slot);
    let sample = grab_policy::Free {
        now: std::time::Instant::now(),
        pos: cur,
        fit,
        // Judged against the window the pointer is OVER ([`grab_policy::free_arming`]): the
        // fullscreen and Space facts are that window's own — with the primary's panel swiped
        // to a workspace, the covering guest still on glass must be able to re-arm — while
        // `key` stays the primary's (the keyboard reaches the guest through it alone, and a
        // covered secondary never becomes key). The two flags stay separate terms because key
        // and Space are *different* questions: a window keeps key status across a Space
        // switch, which is exactly where the old single-flag bug lived.
        fullscreen_and_key: arming.0,
        space_visible: arming.1,
        grab_enabled: ctx.hold > 0.0,
        buttons_down: ctx.buttons.get() != 0,
        click,
    };
    // The window server's own hit test, deferred until the policy has a reason to spend it (a
    // click, or a dwell that has already earned the re-grab). Deliberately NOT short-circuited
    // on the fit: a click in the letterbox is still on our own window, and must not be read as
    // the user leaving for macOS.
    let hit_window = std::cell::Cell::new(None::<isize>);
    let on_guest = || {
        let t = ctx.input.guest_is_topmost_at(loc, &ctx.view);
        hit_window.set(Some(t.hit));
        t.ours
    };
    let out = ctx.with_grab(|st| grab_policy::free_step(st, &sample, on_guest));
    // A click on the guest's own picture that did not take the grab was intercepted by whatever
    // macOS has in front — a menu, a dialog, the revealed chrome. Naming the window is the
    // difference between "clicking stopped working" and a diagnosis, so say it at info rather
    // than only under the trace.
    if click && !out.grab && hit_slot.is_some() && super::fit::point_in_fit(cur.0, cur.1, fit) {
        log::info!(
            "pointer capture: the click on slot {} was taken by window {} in front of the guest — the grab stands down",
            hit_slot.map(|s| s as i64).unwrap_or(-1),
            hit_window.get().unwrap_or(0),
        );
    }
    if click && edge_trace() {
        // Every click, with the two facts that decide it: is the point inside this window's
        // content, and is the guest what a click there would actually hit. A click that neither
        // grabs nor stands the grab down was refused earlier — by the duties gate (not
        // fullscreen, not key, our Space not on screen, or the grab turned off), which the
        // preceding [EDGE] line's own fields spell out.
        eprintln!(
            "[CLICK] t={:.1} loc=({:.1},{:.1}) cur=({:.1},{:.1}) slot={hit_slot:?} \
             in_fit={} fs_key={} space={} enabled={} grab={} stood_down={} latched={}",
            trace_ms(),
            loc.x,
            loc.y,
            cur.0,
            cur.1,
            super::fit::point_in_fit(cur.0, cur.1, fit),
            sample.fullscreen_and_key,
            sample.space_visible,
            sample.grab_enabled,
            out.grab,
            out.left_guest,
            ctx.input.grab_state().user_released(),
        );
    }

    // The chrome ask has exactly one implementation, in `InputState::reveal_step`, and the tap
    // defers to it. It used to have its own — the resistance breakthrough — which stayed
    // distance-based after the monitor's became a hold, so with the tap installed (i.e. whenever
    // Accessibility *is* granted) a two-event flick still summoned the menu bar. Two owners of
    // one gesture is how that happens; now there is one.
    //
    // `out.ask` is deliberately not conditioned on the grab (see `grab_policy::free_step`), and the
    // call is unconditional, never `if overlaid`: releasing the ask is the only thing that brings
    // the overlay back, so skipping it while the overlay is down strands the guest below the camera
    // housing. `reveal_step` tests the overlay itself, where arming needs it and releasing must not.
    // Targeted at the guest window on the panel the pointer is ON (every covered panel has a
    // band to ask past, not just the primary's) — and never fit-contained: the menu bar the ask
    // reveals sits above every fit, so fit-containment would release the ask the moment the
    // user reached it. Release when the pointer leaves needs no separate path: a step from a
    // different slot releases the old owner's ask inside `reveal_step` (the owner prologue),
    // and over no guest panel at all the primary's far-outside conversion stands in, which can
    // only release. Nothing here can freeze an ask down.
    if out.ask {
        let (slot, pcur, fit) = ctx.input.reveal_target_at_global(loc, &ctx.view);
        ctx.input
            .reveal_step(slot, pcur, getd(FIELD_DELTA_Y), fit, RevealSrc::Tap);
    }
    if out.left_guest {
        log::info!(
            "pointer capture: {} — the fullscreen grab stands down until the guest is asked for again",
            if click {
                "the click landed on macOS, not the guest"
            } else {
                "the pointer left the guest"
            }
        );
    }
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
            out.inside_for.map(|d| d.as_millis()),
            ctx.input.grab_state().user_released(),
        );
    }
    if out.grab {
        if edge_trace() {
            // `cur` is this window's view point under the pointer at the grab; the virtual
            // cursor is re-seeded from the live pointer in `toggle_capture`, so what must stay
            // true is that the pointer does not move at the grab: watch the `[CAP]` deltas
            // that follow.
            eprintln!(
                "[GRAB] t={:.1} grabbing: cur=({:.1},{:.1}) slot={hit_slot:?} \
                 fit=({:.1},{:.1} {:.1}x{:.1}) overlaid={} click={click}",
                trace_ms(),
                cur.0,
                cur.1,
                fit.x,
                fit.y,
                fit.w,
                fit.h,
                ctx.panel_fs.load(Ordering::Relaxed),
            );
        }
        if click {
            log::info!("pointer capture: taken — click on guest content");
        }
        ctx.input.toggle_capture(&ctx.view);
    }
    (event, out.grab)
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
    !super::hostdisplay::displays_at(p).is_empty()
}

/// Is there a host display at this view point of `view` — [`grab_policy::press_step`]'s
/// `reachable` predicate, judged against the **capture** window.
///
/// The policy takes the predicate rather than calling CoreGraphics itself because
/// `CGGetDisplaysWithPoint` can only be asked about the Mac it is running on — the release rule
/// was untestable until the arrangement became a parameter.
///
/// A display beyond the edge is enough, covered by one of our own guest windows or not: the
/// captured cursor lives in one window, so the way onto a neighbouring panel IS this release —
/// the pointer is freed onto it and, if that panel is a fullscreen guest, the policy re-grabs
/// it there after its dwell.
fn releasable(view: &NSView, p: (f64, f64)) -> bool {
    let Some(global) = super::input::view_point_to_cg_global(view, p) else {
        return false;
    };
    point_on_a_display(global)
}

/// Let the pointer go, per [`grab_policy::Release`]. At most one warp, and only ever onto a display that
/// exists — `toggle_capture_to` warps the host cursor to the target we hand it, so handing
/// it the release point IS the release warp. No second warp, and no separate code path that could
/// forget `end_warp_suppression`. The target is computed in the CAPTURE window (the grab may
/// have been taken on any covered panel), and it may sit on a different panel than the park —
/// that warp is the release's deliberate discontinuity, excluded from the conservation check.
fn release_grab(
    ctx: &TapCtx,
    step: &super::input::CapturedStep,
    edge: fit::Edge,
    release: Release,
) {
    // The step was resolved in the capture window, and the chrome grant that follows acts on
    // the capture slot — the two must name the same display.
    assert_eq!(
        step.slot,
        ctx.input.capture_slot(),
        "pointer capture: releasing at the {edge:?} edge judged in slot {}'s window, but the \
         capture slot is {} — the press was judged against the wrong display",
        step.slot,
        ctx.input.capture_slot(),
    );
    // And the GUEST's pointer must be at that edge too: the press lasted a sustained hold at
    // our cursor's pinned position, so the guest's cursor plane is on the capture slot's
    // scanout, at its edge — not across a seam on the next one (`window/echo.rs`).
    if let Err(why) = super::echo::at_edge_verdict(step.slot, edge, &super::echo::snapshot()) {
        log::warn!(
            "guest pointer: {why}; view point {:?} in fit ({:.1},{:.1} {:.1}x{:.1}); report: {:?}; arrangement: {}",
            step.view_point,
            step.fit.x,
            step.fit.y,
            step.fit.w,
            step.fit.h,
            super::arrangement::reported_logical_sizes(),
            super::hostdisplay::describe_arrangement(),
        );
    }
    let owning_displays = step
        .view
        .window()
        .map(|w| super::hostdisplay::displays_under_window(&w))
        .unwrap_or_default();
    let (target, aim) = match release {
        // Past the edge, onto whatever display lies there: `releasable` found one, and the
        // broker re-asserts it; no window of ours is expected to cover it.
        Release::Out(p) => (
            p,
            super::warp::Aim {
                stage: "release",
                slot: step.slot,
                displays: Vec::new(),
            },
        ),
        // A point ON the content's boundary can convert to a global pixel one past the display,
        // which the window server then clamps onto a neighbour — see `fit::RELEASE_INSET`. In
        // place means on the capture window's own display: the reveal must happen THERE.
        Release::InPlaceForChrome => (
            fit::pull_inside(step.view_point, step.fit, fit::RELEASE_INSET),
            super::warp::Aim {
                stage: "release",
                slot: step.slot,
                displays: owning_displays.clone(),
            },
        ),
    };
    let global = super::input::view_point_to_cg_global(&step.view, target);
    assert!(
        global.is_some(),
        "pointer capture: releasing at the {edge:?} edge from slot {}'s window, which has no \
         screen position (view point {target:?}); owning displays {owning_displays:?}; arrangement: {}",
        step.slot,
        super::hostdisplay::describe_arrangement(),
    );
    ctx.with_grab(|st| st.stop_holding());
    // The top edge is ONE gesture: under `notch = extend` the reason to press upward is to reach
    // the menu bar, so the same press that frees the pointer also puts the overlay down. Moving
    // back into the guest releases the ask and brings the overlay back, exactly as before.
    if release == Release::InPlaceForChrome {
        ctx.input.grant_chrome();
    }
    log::info!("pointer capture: released — sustained press at the {edge:?} edge ({release:?})");
    if edge_trace() {
        eprintln!(
            "[GRAB] t={:.1} releasing {edge:?} from view ({:.1},{:.1}) to view {target:?} = \
             cg {:?}",
            trace_ms(),
            step.view_point.0,
            step.view_point.1,
            global.map(|p| (p.x, p.y)),
        );
    }
    ctx.input
        .toggle_capture_to(&ctx.view, global.map(|g| (g, aim)));
}

/// Charge the edge press this captured motion represents, and answer with the release it earned.
/// The decision is [`grab_policy::press_step`]'s; this reads the arrangement and traces. The
/// press geometry (`pos`, `fit`) is the capture window's, whose fit clamps the cursor — a press
/// can exist at any of its edges.
fn grab_release_edge(
    ctx: &TapCtx,
    step: &super::input::CapturedStep,
    dx: f64,
    dy: f64,
    fullscreen: bool,
) -> Option<(fit::Edge, Release)> {
    let pos = step.view_point;
    let sample = grab_policy::Press {
        now: std::time::Instant::now(),
        pos,
        delta: (dx, dy),
        fit: step.fit,
        buttons_down: ctx.buttons.get() != 0,
        hold: ctx.hold,
        side: side_tuning(),
        fullscreen,
    };
    let out =
        ctx.with_grab(|st| grab_policy::press_step(st, &sample, |p| releasable(&step.view, p)));
    if let (true, Some(p)) = (edge_trace(), out.pressing) {
        eprintln!(
            "[GRAB] t={:.1} press {:?} at ({:.1},{:.1}) d=({dx:.1},{dy:.1}) \
             charge={:.3}/{:.2} push={:.1} decay={:.2}",
            trace_ms(),
            p.edge,
            pos.0,
            pos.1,
            p.charge,
            p.hold,
            p.push,
            p.decay,
        );
    }
    out.release
}

/// The side-edge feel, overridable per session so a dogfood run can dial it without a rebuild:
/// `LIMINA_SIDE_HOLD_FACTOR` (fraction of the configured hold) and `LIMINA_SIDE_DECAY` (seconds of
/// grace between strokes). Read once; a bad or absent value keeps the shipped default.
///
/// These are tuning aids for exactly this kind of question, not a supported interface — when a value
/// proves better than the default it becomes the default (or a preset), it does not stay in the
/// environment.
fn side_tuning() -> fit::SideTuning {
    static TUNING: std::sync::OnceLock<fit::SideTuning> = std::sync::OnceLock::new();
    *TUNING.get_or_init(|| {
        let read = |name: &str, min: f64, max: f64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= min && *v <= max)
        };
        let mut t = fit::SideTuning::default();
        if let Some(f) = read("LIMINA_SIDE_HOLD_FACTOR", 0.05, 1.0) {
            t.factor = f;
        }
        if let Some(d) = read("LIMINA_SIDE_DECAY", 0.05, 5.0) {
            t.decay = d;
        }
        if t != fit::SideTuning::default() {
            log::info!(
                "pointer grab: side edges tuned to factor={:.2} decay={:.2}s",
                t.factor,
                t.decay
            );
        }
        t
    })
}

/// Whether the session event tap is live. The tap is session-wide, so when it is up it sees every
/// pointer event and the local monitor must not *also* drive anything stateful — see the reveal
/// call in `InputState::emit_motion`.
pub(crate) fn installed() -> bool {
    !TAP_PORT.load(Ordering::Relaxed).is_null()
}

/// Whether to log every resistance decision to stderr (`LIMINA_EDGE_TRACE=1`).
/// `LIMINA_WARP_PROBE=1`: measure what a NONZERO warp does during steady-state capture.
///
/// **Answered 2026-08-20, SAME-DISPLAY ONLY (32/32 warps, two-panel rig):** the warp's
/// vector arrives in the next event's delta, NEGATED (+200 warp → ≈−200 delta), and there
/// is NO suppression gap. This law does NOT extend across displays: the seam-crossing
/// re-park's warp arrives POSITIVELY (+W, `[CAP]` trace the same day, 10/10 crossings) —
/// which is why the re-park detects the injection instead of predicting it
/// Kept because it is the only instrument that can re-verify
/// the behavior on a new macOS release.
///
/// While captured, every ~3 s one motion event's re-pin goes to `park + (±200, 0)` instead of
/// the park (the next event's ordinary re-pin pulls it back — a second 200 pt warp, doubling
/// the samples). The dozen deltas after each probe warp are printed with timestamps. Read:
/// - a ±200 spike in the deltas = the warp vector is injected;
/// - a ~250 ms gap in the timestamps = the warp opens the suppression window.
fn warp_probe() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIMINA_WARP_PROBE").is_some_and(|v| v != "0"))
}

thread_local! {
    /// (last probe warp, direction sign, motion events since that warp).
    static WARP_PROBE_STATE: std::cell::Cell<(Option<std::time::Instant>, f64, u32)> =
        const { std::cell::Cell::new((None, 1.0, u32::MAX)) };
}

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
// Six plumbing parameters, each a distinct shared handle the callback needs for the app's
// lifetime; bundling them into a struct would just move the same list one line up.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install(
    conn: Arc<WorkerConn>,
    captured: Arc<AtomicBool>,
    input: Rc<InputState>,
    soft_kbd_grab: bool,
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
        view,
        // One choke point for the unit. `[display] edge-resistance` is seconds now, but a
        // hand-edited vm.toml (or a `--edge-resistance 100` in someone's shell history) can still
        // carry the pre-2026-08 points encoding; `EdgeHold` migrates by preset position.
        hold: crate::vmlib::schema::EdgeHold::from_toml(edge_resistance).seconds(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_fire_from_a_pointer_grab_also_mutes_the_soft_keyboard() {
        // The dogfood sequence: fullscreen, the POLICY holds the pointer (`captured`, no
        // Cmd-Ctrl-G involved). Ctrl+Opt breaks — Option up, Ctrl still held — intending a host
        // workspace switch (Ctrl+arrow). The window stays key after the pointer is released, so
        // unless the fire ALSO mutes the soft keyboard grab, the still-held Ctrl + arrow is
        // forwarded to the guest and macOS never sees the combo: the chord has to be pressed
        // twice, once per grab. Ctrl+Opt means "give me the host back" — one press, both grabs.
        let fire = chord_fire(true);
        assert!(fire.release_pointer);
        assert!(fire.mute_soft);
    }

    #[test]
    fn chord_fire_with_no_pointer_grab_mutes_the_soft_keyboard_only() {
        let fire = chord_fire(false);
        assert!(!fire.release_pointer);
        assert!(fire.mute_soft);
    }
}
