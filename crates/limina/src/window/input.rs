// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Translate captured `NSEvent`s into Linux evdev events and ship them to the worker.
//!
//! Runs entirely on the main thread (from the window's local event monitor). Keyboard and
//! pointer go to separate sockets matching the worker's two virtio-input devices. Each
//! logical change is committed with a trailing `EV_SYN`/`SYN_REPORT`, exactly as a real
//! evdev device would. The pointer is absolute: window-local cursor coordinates are scaled
//! into the device's `0..=ABS_MAX` range, so the guest cursor tracks the macOS cursor 1:1.
//!
//! Pointer events are gated to the guest view: AppKit keeps delivering `MouseMoved` to the
//! key window while the cursor is *outside* it (with `locationInWindow` in screen
//! coordinates when no window is associated), so without the gate the guest pointer
//! wanders while the host pointer isn't even over the VM. Motion/press/scroll require the
//! point to be inside the content view; drags and releases follow a press that happened
//! inside (macOS capture semantics), with coordinates clamped to the view.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::WorkerConn;

use objc2::rc::Retained;
use objc2_app_kit::{NSCursor, NSEvent, NSEventType, NSView};
use objc2_foundation::{NSPoint, NSRect};

use limina_input::constants::{
    ABS_MAX, ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL,
    REL_WHEEL, REL_X, REL_Y,
};
use limina_input::keymap::{
    capslock_on, macos_keycode_to_linux_remapped, modifier_emit, CapsLockSync, KeyRemap, ModEmit,
    MACOS_KC_CAPSLOCK,
};
use limina_input::InputEvent;

// CoreGraphics (already linked): pointer-capture / mouselook primitives.
//   - `CGAssociateMouseAndMouseCursorPosition(0)` asks the HID layer to stop driving the cursor
//     from the mouse. It's unreliable on its own (the cursor still drifted onto windows behind us
//     on macOS 26), so we ALSO re-pin the cursor to the display centre on every captured move.
//   - `CGWarpMouseCursorPosition` does the re-pin; crucially `NSEvent.deltaX/deltaY` come from the
//     mouse, unaffected by warping, so re-centring never corrupts the motion we integrate.
// `connected` is a `boolean_t` (C `int`); 0 = decoupled (captured), 1 = normal.
extern "C" {
    fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
    fn CGWarpMouseCursorPosition(point: NSPoint) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> NSRect;
}

/// Centre of the main display in global (points, top-left origin) coordinates — the fixed point
/// we keep the host cursor pinned to while captured.
fn main_display_center() -> NSPoint {
    // SAFETY: plain CoreGraphics queries, no preconditions.
    let b = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    NSPoint::new(
        b.origin.x + b.size.width / 2.0,
        b.origin.y + b.size.height / 2.0,
    )
}

/// Apply the host-cursor side of a capture transition: on grab, decouple the HW mouse, park the
/// cursor at centre, and hide it; on release, re-couple, warp the cursor to `release_to` (where
/// the captured virtual cursor ended, so leaving capture is as seamless as entering it), show it,
/// and re-assert the guest shape. Shared by the local-monitor toggle
/// ([`InputState::toggle_capture`]) and the capture tap, so the two stay byte-identical. Main
/// thread only (NSCursor hide/unhide must balance).
pub(crate) fn apply_capture_cursor(
    on: bool,
    host_cursor: &HostCursor,
    release_to: Option<NSPoint>,
) {
    if on {
        unsafe {
            CGAssociateMouseAndMouseCursorPosition(0);
            CGWarpMouseCursorPosition(main_display_center());
        }
        // Idempotent: NSCursor hide/unhide is a counter — a double-hide (e.g. a stray double
        // toggle) would need a matching double-unhide or the cursor stays gone forever. Only
        // hide if we haven't already, so the count never drifts.
        if !CURSOR_HIDDEN.swap(true, Ordering::AcqRel) {
            NSCursor::hide();
        }
        log::info!("pointer capture: ON (Cmd-Ctrl-G to release)");
    } else {
        unsafe {
            CGAssociateMouseAndMouseCursorPosition(1);
            // Warp while still hidden, so the cursor *appears* at the release point rather
            // than visibly jumping from the parked centre.
            if let Some(p) = release_to {
                CGWarpMouseCursorPosition(p);
            }
        }
        if CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
            NSCursor::unhide();
        }
        host_cursor.reassert();
        log::info!("pointer capture: OFF");
    }
}

/// Map a view point (bottom-left origin, view coordinates) to CG *global* coordinates
/// (top-left origin of the primary display) — the space `CGWarpMouseCursorPosition` speaks.
/// `None` when the view isn't in a window. NS global coordinates share the primary display's
/// origin with CG but grow upward, so the flip goes through the primary display height.
pub(crate) fn view_point_to_cg_global(view: &NSView, p: (f64, f64)) -> Option<NSPoint> {
    let window = view.window()?;
    let base = view.convertPoint_toView(NSPoint::new(p.0, p.1), None);
    let scr = window.convertPointToScreen(base);
    let h = unsafe { CGDisplayBounds(CGMainDisplayID()) }.size.height;
    Some(NSPoint::new(scr.x, h - scr.y))
}

/// Whether we currently have the host cursor hidden for capture — keeps `NSCursor::hide`/`unhide`
/// (a reference count) balanced no matter how the toggle is driven.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// A host-intercepted shortcut: recognized BEFORE the key reaches the guest, so the combo
/// drives limina itself. The seed of the configurable-keybinding system (M8); for now just
/// the fullscreen toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostShortcut {
    ToggleFullScreen,
    /// Toggle pointer capture (relative/mouselook mode): grab the host cursor and feed the
    /// guest relative motion, or release it.
    ToggleCapture,
}

/// Recognize a host shortcut from a key-down's macOS keycode + raw `modifierFlags` bitmask,
/// or `None` if the key should go to the guest. Uses the **device-independent class** flag
/// bits, so it's independent of left/right *and* of `--swap-cmd-opt` (the swap changes only
/// which evdev code we emit to the guest, never the macOS modifier state read here). All
/// shortcuts require EXACTLY Command+Control (no Option/Shift) so richer combos still reach
/// the guest.
pub fn match_host_shortcut(keycode: u16, flags: u64) -> Option<HostShortcut> {
    const SHIFT: u64 = 1 << 17;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    const KC_F: u16 = 0x03; // Cmd-Ctrl-F — macOS-standard Enter/Exit-Full-Screen
    const KC_G: u16 = 0x05; // Cmd-Ctrl-G — grab/release the pointer (capture mode)
    let held = |m: u64| flags & m != 0;
    if !(held(COMMAND) && held(CONTROL)) || held(OPTION) || held(SHIFT) {
        return None;
    }
    match keycode {
        KC_F => Some(HostShortcut::ToggleFullScreen),
        KC_G => Some(HostShortcut::ToggleCapture),
        _ => None,
    }
}

/// One step of the modifier-only ungrab chord (Control+Option, VMware-style): given whether
/// the chord is currently armed and the new `flagsChanged` bitmask, returns `(armed, fire)`.
/// Arms when EXACTLY Control+Option are held (no Command/Shift); fires when the chord then
/// breaks cleanly (either key lifts, nothing else joined). Any other modifier joining
/// disarms, and the caller disarms on any key/button/scroll — so guest combos that *start*
/// with Ctrl+Alt (Ctrl-Alt-T, Ctrl-Alt-arrows) still reach the guest while captured: they
/// press another key mid-chord, which cancels the ungrab.
pub(crate) fn ungrab_chord_step(armed: bool, flags: u64) -> (bool, bool) {
    const SHIFT: u64 = 1 << 17;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    let held = |m: u64| flags & m != 0;
    let (ctrl, opt, cmd, shift) = (held(CONTROL), held(OPTION), held(COMMAND), held(SHIFT));
    if ctrl && opt && !cmd && !shift {
        return (true, false);
    }
    // Named parts (not one expression) — clippy's "minimal" form is unreadable here.
    let chord_broke = !(ctrl && opt);
    let nothing_else_held = !cmd && !shift;
    (false, armed && chord_broke && nothing_else_held)
}

/// The macOS virtual keycodes of every held-modifier key (left/right pairs of
/// Command/Shift/Option/Control) — the set force-released at an ungrab boundary.
const MODIFIER_KEYCODES: [u16; 8] = [0x37, 0x36, 0x38, 0x3C, 0x3A, 0x3D, 0x3B, 0x3E];

/// The host pointer's adoption of the guest cursor (main thread only). `cursor` is what
/// the macOS pointer should look like over the guest view (the guest's current cursor
/// image, or a blank cursor while the guest hides it); `inside` tracks whether the pointer
/// is currently over the view so shape updates from the worker apply immediately.
pub struct HostCursor {
    cursor: RefCell<Retained<NSCursor>>,
    inside: Cell<bool>,
}

impl HostCursor {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            // Default to a BLANK cursor over the view, not the arrow. Many guests (e.g. mutter on
            // our virtio-gpu) software-render their cursor into the framebuffer and never send a
            // hardware cursor; showing the macOS arrow on top then gives TWO cursors. Staying blank
            // until/unless the guest sends a real hardware-cursor shape (`update`) means the guest's
            // own cursor is the only one visible. Guests that DO use a hardware cursor replace this
            // with their shape on first hover.
            cursor: RefCell::new(super::blank_cursor().unwrap_or_else(NSCursor::arrowCursor)),
            inside: Cell::new(false),
        })
    }

    /// Adopt a new guest cursor shape (or blank). Takes effect now if the pointer is over
    /// the view, otherwise on the next entry.
    pub fn update(&self, cursor: Retained<NSCursor>) {
        if self.inside.get() {
            cursor.set();
        }
        *self.cursor.borrow_mut() = cursor;
    }

    /// Re-assert the guest cursor shape if the pointer is currently over the view (used when
    /// leaving pointer-capture mode, where we hid the cursor).
    fn reassert(&self) {
        if self.inside.get() {
            self.cursor.borrow().set();
        }
    }

    /// Track the pointer crossing the view boundary. Re-asserts the guest cursor on every
    /// inside motion (AppKit can reset the cursor behind our back — resize edges, window
    /// transitions); restores the arrow when leaving.
    fn on_motion(&self, inside: bool) {
        if inside {
            self.cursor.borrow().set();
        } else if self.inside.get() {
            NSCursor::arrowCursor().set();
        }
        self.inside.set(inside);
    }
}

/// Main-thread input translator. Tracks which modifier keys are currently held so
/// `flagsChanged` (which carries no up/down) can be turned into press/release pairs.
pub struct InputState {
    /// The current worker's input sink fds (swapped on a reboot relaunch). Read fresh per event.
    conn: Arc<WorkerConn>,
    /// macOS keycodes of *held* modifiers believed to be down (toggled on each flagsChanged).
    pressed_mods: RefCell<HashSet<u16>>,
    /// macOS keycodes of *held* non-modifier keys we forwarded as down but not yet up. Tracked
    /// so a focus loss mid-press (Cmd-Tab) can release them — the local monitor stops delivering
    /// events the instant focus leaves, so the key-up would be lost and the key would stick down.
    pressed_keys: RefCell<HashSet<u16>>,
    /// Caps Lock is a *lock* key, not a held modifier: kept in sync with the host LED on every
    /// event (see [`InputState::sync_capslock`]), separate from `pressed_mods`.
    caps: RefCell<CapsLockSync>,
    /// Bitmask of mouse buttons whose *press* we forwarded to the guest. Releases and
    /// drags are forwarded only while set, so a click that started on the title bar (or
    /// outside the window) never reaches the guest in any form.
    guest_buttons: Cell<u8>,
    host_cursor: Rc<HostCursor>,
    /// Keyboard remap policy (e.g. the Command/Option swap), applied to every key/modifier.
    remap: KeyRemap,
    /// Pointer-capture mode: when set, the host cursor is grabbed (frozen and hidden) and the
    /// macOS-accelerated motion deltas integrate into a *virtual* cursor position
    /// ([`InputState::capture_pos`]) that drives the same absolute tablet as uncaptured mode —
    /// so movement feels exactly like the host cursor (same acceleration, same fit mapping, and
    /// libinput never accelerates an absolute device on top). Toggled by `Cmd-Ctrl-G`. Shared
    /// with the render timer (an `Arc<AtomicBool>`), which composites the guest cursor at its
    /// reported position while this is set (the host cursor is hidden, so the guest cursor has
    /// to be drawn).
    captured: Arc<AtomicBool>,
    /// Ungrab-chord (Ctrl+Option) arm state, shared by whichever path is consuming
    /// `flagsChanged` (the tap while captured, this monitor in degraded capture) — see
    /// [`ungrab_chord_step`]. Main thread only.
    ungrab_armed: Cell<bool>,
    /// The virtual cursor position in view points (bottom-left origin, the fit rect's space).
    /// Uncaptured motion keeps it at the pointer's last position over the content, so a grab
    /// starts exactly where the cursor was; captured motion integrates deltas into it
    /// ([`fit::capture_step`]); a release warps the host cursor back to it. `None` until the
    /// pointer has ever been placed (then capture seeds at the content centre). Shared with the
    /// capture tap (same main thread → `Rc<Cell>`).
    capture_pos: Rc<Cell<Option<(f64, f64)>>>,
    /// Where the guest scanout currently sits inside the content view, written by the render
    /// path every tick (dynamic mode: the full view — the legacy mapping; host/fixed: the
    /// letterboxed fit rect). The absolute-pointer transform and the inside-gate go through
    /// it, so the pointer can never disagree with the pixels. Same main thread → `Rc<Cell>`.
    fit: Rc<Cell<super::fit::FitRect>>,
}

impl InputState {
    pub fn new(
        conn: Arc<WorkerConn>,
        host_cursor: Rc<HostCursor>,
        remap: KeyRemap,
        captured: Arc<AtomicBool>,
        fit: Rc<Cell<super::fit::FitRect>>,
        capture_pos: Rc<Cell<Option<(f64, f64)>>>,
    ) -> Self {
        Self {
            conn,
            pressed_mods: RefCell::new(HashSet::new()),
            pressed_keys: RefCell::new(HashSet::new()),
            caps: RefCell::new(CapsLockSync::new()),
            guest_buttons: Cell::new(0),
            host_cursor,
            remap,
            captured,
            ungrab_armed: Cell::new(false),
            capture_pos,
            fit,
        }
    }

    fn is_captured(&self) -> bool {
        self.captured.load(Ordering::Acquire)
    }

    /// Toggle pointer capture. On grab: decouple the hardware mouse from the cursor (so deltas
    /// flow while the cursor stays frozen) and hide the cursor. On release: restore both and put
    /// the host cursor where the virtual cursor ended, so the transition is seamless in both
    /// directions. Returns the new captured state. Main thread only.
    pub fn toggle_capture(&self, view: &NSView) -> bool {
        let now = !self.is_captured();
        let release_to = if now {
            None
        } else {
            self.capture_pos
                .get()
                .and_then(|p| view_point_to_cg_global(view, p))
        };
        self.captured.store(now, Ordering::Release);
        apply_capture_cursor(now, &self.host_cursor, release_to);
        self.ungrab_armed.set(false);
        // Reconcile modifier bookkeeping across the boundary: while captured the TAP forwards
        // modifier edges, so this monitor's believed-pressed sets go stale — and stale state
        // makes `modifier_emit` swallow a later edge (a stuck or missed modifier in the guest).
        // On grab, release everything WE forwarded (the tap re-emits what's still held on its
        // next flagsChanged); on ungrab, force-release every modifier key — the ungrab chord
        // itself is always mid-press at this moment (releases of un-pressed keys are dropped
        // by the guest's input core, so over-releasing is safe).
        if now {
            self.release_all_held();
        } else {
            self.release_all_modifiers();
        }
        now
    }

    /// Force-release every held-modifier key in the guest and reset the believed-pressed
    /// sets. Called on capture release: whatever the tap left pressed (the ungrab chord at
    /// minimum) must not stay wedged down in the guest.
    fn release_all_modifiers(&self) {
        for &kc in &MODIFIER_KEYCODES {
            if let Some(code) = macos_keycode_to_linux_remapped(kc, &self.remap) {
                self.send_kbd(InputEvent::new(EV_KEY, code, 0));
                self.send_kbd(InputEvent::syn());
            }
        }
        self.pressed_mods.borrow_mut().clear();
    }

    /// Tap-side key forwarding: same bookkeeping as the local monitor (caps-lock sync,
    /// believed-pressed tracking so a focus-loss flush releases tap-forwarded keys too).
    /// The tap calls this for keyDown/keyUp it consumes (captured or soft-grab mode).
    pub(crate) fn tap_key(&self, macos_keycode: u16, down: bool, flags: u64) {
        self.sync_capslock(flags);
        self.emit_key(macos_keycode, down);
    }

    /// Tap-side `flagsChanged` forwarding — the modifier twin of [`InputState::tap_key`].
    pub(crate) fn tap_flags(&self, macos_keycode: u16, flags: u64) {
        self.sync_capslock(flags);
        self.emit_modifier(macos_keycode, flags);
    }

    /// Exit the SOFT keyboard grab (Ctrl+Option while focused but not captured): flush the
    /// modifiers the chord pushed into the guest so nothing stays wedged. The caller mutes
    /// soft mode until the window regains key status.
    pub(crate) fn flush_modifiers(&self) {
        self.release_all_modifiers();
    }

    /// Feed a captured-mode `flagsChanged` bitmask to the ungrab chord. Returns `true` when
    /// the chord fired (the caller should release capture and consume the event).
    pub(crate) fn observe_ungrab_flags(&self, flags: u64) -> bool {
        let (armed, fire) = ungrab_chord_step(self.ungrab_armed.get(), flags);
        self.ungrab_armed.set(armed);
        fire
    }

    /// Disarm the ungrab chord — any non-modifier activity (key, button, scroll) between the
    /// chord press and its break means the user was typing a combo, not ungrabbing.
    pub(crate) fn cancel_ungrab_chord(&self) {
        self.ungrab_armed.set(false);
    }

    /// Handle one captured event. Returns `true` if it should be swallowed (not passed on
    /// to AppKit) — we swallow keys so unhandled keystrokes don't beep, but let mouse
    /// events through so the title bar / close button keep working.
    pub fn handle(&self, event: &NSEvent, view: &NSView) -> bool {
        // Caps Lock is a lock key kept aligned with the host LED on every event (each carries
        // the live caps bit). This applies deliberate toggles and, crucially, heals drift from
        // a caps toggle done while the VM was unfocused — the monitor sees no event for that and
        // macOS sends no reconciling flagsChanged on refocus, so the next key/pointer event here
        // is what re-syncs the guest. See [`InputState::sync_capslock`].
        self.sync_capslock(event.modifierFlags().0 as u64);
        match event.r#type() {
            NSEventType::KeyDown => {
                self.cancel_ungrab_chord();
                // The guest kernel autorepeats from key-down state; drop macOS repeats.
                if !event.isARepeat() {
                    self.emit_key(event.keyCode(), true);
                }
                true
            }
            NSEventType::KeyUp => {
                self.emit_key(event.keyCode(), false);
                true
            }
            NSEventType::FlagsChanged => {
                // Degraded (tap-less) capture consumes flagsChanged here — give the ungrab
                // chord (Ctrl+Option) the same meaning it has under the tap.
                if self.is_captured() && self.observe_ungrab_flags(event.modifierFlags().0 as u64) {
                    self.toggle_capture(view);
                    return true;
                }
                self.emit_modifier(event.keyCode(), event.modifierFlags().0 as u64);
                true
            }
            NSEventType::MouseMoved => {
                if self.is_captured() {
                    self.emit_captured_motion(event);
                    return false;
                }
                let inside = self.pointer_inside(event, view);
                self.host_cursor.on_motion(inside);
                if inside {
                    self.emit_motion(event, view);
                }
                false
            }
            NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDragged => {
                if self.is_captured() {
                    self.emit_captured_motion(event);
                    return false;
                }
                let inside = self.pointer_inside(event, view);
                self.host_cursor.on_motion(inside);
                // A drag continues a press: forward it (clamped) even outside the view,
                // but only if the press itself went to the guest.
                if self.guest_buttons.get() != 0 || inside {
                    self.emit_motion(event, view);
                }
                false
            }
            NSEventType::LeftMouseDown => self.emit_press(event, view, BTN_LEFT),
            NSEventType::LeftMouseUp => self.emit_release(BTN_LEFT),
            NSEventType::RightMouseDown => self.emit_press(event, view, BTN_RIGHT),
            NSEventType::RightMouseUp => self.emit_release(BTN_RIGHT),
            NSEventType::OtherMouseDown => self.emit_other_button(event, view, true),
            NSEventType::OtherMouseUp => self.emit_other_button(event, view, false),
            NSEventType::ScrollWheel => {
                if self.is_captured() || self.pointer_inside(event, view) {
                    self.emit_scroll(event);
                }
                false
            }
            _ => false,
        }
    }

    /// Is the event's pointer position inside the guest *content* (the fit rect — in
    /// host/fixed modes the letterbox bars are outside, like window chrome)? `false` for
    /// events with no associated window (their location is in screen coordinates and can't
    /// be mapped).
    fn pointer_inside(&self, event: &NSEvent, view: &NSView) -> bool {
        // SAFETY: we only run on the main thread (the local event monitor's thread).
        let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };
        if event.window(mtm).is_none() {
            return false;
        }
        let p = view.convertPoint_fromView(event.locationInWindow(), None);
        super::fit::point_in_fit(p.x, p.y, self.fit.get())
    }

    fn emit_key(&self, macos_keycode: u16, down: bool) {
        if let Some(code) = macos_keycode_to_linux_remapped(macos_keycode, &self.remap) {
            self.send_kbd(InputEvent::new(EV_KEY, code, down as i32));
            self.send_kbd(InputEvent::syn());
            // Track the held key so a focus loss mid-press can release it (see `release_all_held`).
            if down {
                self.pressed_keys.borrow_mut().insert(macos_keycode);
            } else {
                self.pressed_keys.borrow_mut().remove(&macos_keycode);
            }
        }
    }

    /// Release every key we've forwarded as held — the modifiers (`pressed_mods`) and the
    /// non-modifier keys (`pressed_keys`) — and forget them. Called when the VM window loses key
    /// focus (e.g. the user hit Cmd-Tab): the local event monitor stops delivering events the
    /// instant focus leaves, so the matching key-ups never arrive and the keys would stick "down"
    /// in the guest — a wedged Command then makes the guest compositor eat every later key. Cheap
    /// and idempotent when nothing is held. State is re-learned once focus returns (modifiers from
    /// the next `flagsChanged`, keys from the next key-down), so over-releasing here is safe.
    pub fn release_all_held(&self) {
        let mut mods = self.pressed_mods.borrow_mut();
        let mut keys = self.pressed_keys.borrow_mut();
        if mods.is_empty() && keys.is_empty() {
            return;
        }
        for &macos_keycode in mods.iter().chain(keys.iter()) {
            if let Some(code) = macos_keycode_to_linux_remapped(macos_keycode, &self.remap) {
                self.send_kbd(InputEvent::new(EV_KEY, code, 0));
                self.send_kbd(InputEvent::syn());
            }
        }
        log::debug!(
            "input: released {} held key(s) on focus loss",
            mods.len() + keys.len()
        );
        mods.clear();
        keys.clear();
    }

    /// `flagsChanged` reports which modifier key changed but not the direction. [`modifier_emit`]
    /// reads the modifier's *actual* state from the event's flag bitmask rather than toggling a
    /// guess — a single dropped `flagsChanged` (macOS suppresses events while Command is held,
    /// and across focus changes) can't then wedge a modifier "down" in the guest, which would
    /// make the compositor eat every later key. We keep a pressed-set to de-duplicate held
    /// modifiers (one press/one release per change). Lock keys (Caps Lock) instead emit a full
    /// press+release tap per toggle — the guest toggles its own lock on press, so an edge would
    /// stick it (see [`ModEmit::Tap`]); they're not tracked in the pressed-set.
    fn emit_modifier(&self, macos_keycode: u16, raw_flags: u64) {
        // The remap changes which evdev code we emit; `modifier_emit` stays keyed on the
        // *physical* keycode (the macOS modifier-flag state is the physical key's). Caps Lock
        // returns `None` here — it's a lock key handled by `sync_capslock`, not a held modifier.
        let Some(code) = macos_keycode_to_linux_remapped(macos_keycode, &self.remap) else {
            return;
        };
        let was = self.pressed_mods.borrow().contains(&macos_keycode);
        let Some(emit) = modifier_emit(macos_keycode, raw_flags, was) else {
            return;
        };
        match emit {
            ModEmit::None => {}
            ModEmit::Edge(down) => {
                if down {
                    self.pressed_mods.borrow_mut().insert(macos_keycode);
                } else {
                    self.pressed_mods.borrow_mut().remove(&macos_keycode);
                }
                self.send_kbd(InputEvent::new(EV_KEY, code, down as i32));
                self.send_kbd(InputEvent::syn());
            }
        }
    }

    /// Align the guest's caps-lock with the host caps LED (carried by every event's modifier
    /// flags). Emits a press+release tap only when the host LED differs from the believed guest
    /// state ([`CapsLockSync`]), so it both applies deliberate toggles and heals drift from a
    /// caps toggle done while the VM was unfocused — the monitor gets no event for that and
    /// macOS sends no reconciling flagsChanged on refocus, so the next event here re-syncs.
    fn sync_capslock(&self, raw_flags: u64) {
        if self.caps.borrow_mut().observe(capslock_on(raw_flags)) {
            if let Some(code) = macos_keycode_to_linux_remapped(MACOS_KC_CAPSLOCK, &self.remap) {
                self.send_kbd(InputEvent::new(EV_KEY, code, 1));
                self.send_kbd(InputEvent::syn());
                self.send_kbd(InputEvent::new(EV_KEY, code, 0));
                self.send_kbd(InputEvent::syn());
            }
        }
    }

    /// Forward a button press if it lands inside the guest view; remember it so the
    /// matching release (and intervening drags) follow even if the pointer has left.
    fn emit_press(&self, event: &NSEvent, view: &NSView, btn: u16) -> bool {
        self.cancel_ungrab_chord();
        if self.is_captured() {
            // Captured: no view gate — the virtual cursor is always over the content. Re-send
            // its position with the press (same staleness guard as the uncaptured path below).
            self.guest_buttons
                .set(self.guest_buttons.get() | btn_bit(btn));
            self.send_captured_pos();
            self.send_ptr(InputEvent::new(EV_KEY, btn, 1));
            self.send_ptr(InputEvent::syn());
        } else if self.pointer_inside(event, view) {
            self.guest_buttons
                .set(self.guest_buttons.get() | btn_bit(btn));
            // Send the position with the press so the guest clicks where the host did,
            // even if the last forwarded motion is stale (pointer re-entered the view).
            self.emit_motion(event, view);
            self.send_ptr(InputEvent::new(EV_KEY, btn, 1));
            self.send_ptr(InputEvent::syn());
        }
        false
    }

    /// Forward a release only for presses the guest saw (never leaves a button stuck).
    fn emit_release(&self, btn: u16) -> bool {
        if self.guest_buttons.get() & btn_bit(btn) != 0 {
            self.guest_buttons
                .set(self.guest_buttons.get() & !btn_bit(btn));
            self.send_ptr(InputEvent::new(EV_KEY, btn, 0));
            self.send_ptr(InputEvent::syn());
        }
        false
    }

    fn emit_other_button(&self, event: &NSEvent, view: &NSView, down: bool) -> bool {
        // buttonNumber 2 is the middle button; ignore further buttons for now.
        if event.buttonNumber() == 2 {
            if down {
                self.emit_press(event, view, BTN_MIDDLE);
            } else {
                self.emit_release(BTN_MIDDLE);
            }
        }
        false
    }

    /// Map the event's window-local cursor position to the absolute device range through
    /// the current fit rect (letterbox offset + scale, Y flipped — AppKit is bottom-left
    /// origin, evdev top-left). Positions outside the content (drags that left it) clamp
    /// to the nearest content edge. With the fit at the full view (dynamic mode) this is
    /// the legacy full-bounds mapping, bit for bit. Also remembers the (clamped) position
    /// as the capture seed, so a grab starts exactly where the cursor was.
    fn emit_motion(&self, event: &NSEvent, view: &NSView) {
        // `None` source view = the point is in window base coordinates.
        let p = view.convertPoint_fromView(event.locationInWindow(), None);
        let fit = self.fit.get();
        self.capture_pos.set(Some(
            super::fit::capture_step(Some((p.x, p.y)), 0.0, 0.0, fit).pos,
        ));
        let (x, y) = super::fit::abs_through_fit(p.x, p.y, fit, ABS_MAX as i32);
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
        self.send_ptr(InputEvent::syn());
    }

    /// Capture mode: integrate the event's motion delta into the virtual cursor and drive the
    /// absolute tablet with it. `deltaX/deltaY` carry the pointer-ballistics-processed motion
    /// the macOS cursor would have made, and the absolute device adds no guest-side
    /// acceleration, so captured movement feels exactly like the host cursor.
    fn emit_captured_motion(&self, event: &NSEvent) {
        // Re-pin the (hidden) host cursor to the display centre so it can't drift onto windows
        // behind us — CGAssociate(false) alone doesn't reliably freeze it. The warp doesn't
        // affect `deltaX/deltaY`, so the virtual cursor still integrates clean motion.
        unsafe { CGWarpMouseCursorPosition(main_display_center()) };
        let fit = self.fit.get();
        let step =
            super::fit::capture_step(self.capture_pos.get(), event.deltaX(), event.deltaY(), fit);
        self.capture_pos.set(Some(step.pos));
        let (x, y) = super::fit::abs_through_fit(step.pos.0, step.pos.1, fit, ABS_MAX as i32);
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
        self.send_ptr(InputEvent::syn());
        send_edge_overflow(&self.conn, step.overflow);
    }

    /// Send the virtual cursor's current absolute position (seeding it if nothing ever
    /// placed it) — the captured-mode analogue of the position-with-press staleness guard.
    fn send_captured_pos(&self) {
        let fit = self.fit.get();
        let p = super::fit::capture_step(self.capture_pos.get(), 0.0, 0.0, fit).pos;
        self.capture_pos.set(Some(p));
        let (x, y) = super::fit::abs_through_fit(p.0, p.1, fit, ABS_MAX as i32);
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
        self.send_ptr(InputEvent::syn());
    }

    fn emit_scroll(&self, event: &NSEvent) {
        self.cancel_ungrab_chord();
        // macOS deltas are continuous; emit one wheel notch per event in the delta's
        // direction (crude but usable; precise pixel scrolling is a later refinement).
        let dy = event.scrollingDeltaY();
        let dx = event.scrollingDeltaX();
        let mut any = false;
        if dy.abs() > 0.0 {
            self.send_ptr(InputEvent::new(EV_REL, REL_WHEEL, dy.signum() as i32));
            any = true;
        }
        if dx.abs() > 0.0 {
            // Natural macOS scroll: right swipe = negative dx; REL_HWHEEL right = +1.
            self.send_ptr(InputEvent::new(EV_REL, REL_HWHEEL, (-dx.signum()) as i32));
            any = true;
        }
        if any {
            self.send_ptr(InputEvent::syn());
        }
    }

    fn send_kbd(&self, ev: InputEvent) {
        // Snapshot the current worker's endpoints and hold the Arc across the send, so a
        // reboot relaunch can't close (or let the OS reuse) the fd mid-write.
        let io = self.conn.io();
        send_event(io.kbd_fd(), ev);
    }

    /// Send to the absolute-pointer device — both modes drive it now (captured mode moves a
    /// virtual cursor through the same mapping). The relative-mouse device carries only the
    /// edge-clamped overflow ([`send_edge_overflow`]). Same snapshot-held-across-the-send
    /// rule as `send_kbd`.
    fn send_ptr(&self, ev: InputEvent) {
        let io = self.conn.io();
        send_event(io.ptr_fd(), ev);
    }
}

/// Forward clamped-off captured motion to the guest's relative-mouse device as edge
/// *pressure*. mutter's pressure barriers (GNOME's hot corner) fire on motion pushed INTO a
/// barrier while the pointer is pinned — something a pre-clamped absolute stream can never
/// express, which is why the hot corner went dead in capture mode. Away from edges the
/// overflow is zero and the device stays silent; when it does fire, the resulting relative
/// motion cannot drift the guest cursor, because the compositor clamps it at the same screen
/// edge the absolute position is already pinned to.
pub(crate) fn send_edge_overflow(conn: &Arc<WorkerConn>, overflow: (f64, f64)) {
    let dx = overflow.0.round() as i32;
    let dy = overflow.1.round() as i32;
    if dx == 0 && dy == 0 {
        return;
    }
    // Snapshot rule as everywhere: hold the Arc so the fd stays open across the sends.
    let io = conn.io();
    let fd = io.rel_ptr_fd();
    if dx != 0 {
        send_event(fd, InputEvent::new(EV_REL, REL_X, dx));
    }
    if dy != 0 {
        send_event(fd, InputEvent::new(EV_REL, REL_Y, dy));
    }
    send_event(fd, InputEvent::syn());
}

/// One bit per mouse button for the forwarded-press mask.
fn btn_bit(btn: u16) -> u8 {
    match btn {
        BTN_LEFT => 1,
        BTN_RIGHT => 2,
        BTN_MIDDLE => 4,
        _ => 0,
    }
}

pub(crate) fn send_event(fd: RawFd, ev: InputEvent) {
    let bytes = ev.to_bytes();
    // One datagram per event. The fd is non-blocking (set in the supervisor) so a full
    // socket can never block the AppKit main thread and freeze the whole UI — we drop the
    // event instead. With the generous socket buffers we set, EAGAIN only happens if the
    // worker has stopped draining (e.g. its input thread died), so warn loudly: it points
    // at a real stall rather than normal backpressure.
    let n = unsafe { libc::send(fd, bytes.as_ptr() as *const libc::c_void, bytes.len(), 0) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) | Some(libc::ENOBUFS) => {
                log::warn!("input: dropped event {ev:?} — worker socket full (worker stalled?)")
            }
            _ => log::trace!("input send failed: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    const SHIFT: u64 = 1 << 17;
    const KC_F: u16 = 0x03;
    const KC_A: u16 = 0x00;

    #[test]
    fn cmd_ctrl_f_is_the_fullscreen_toggle() {
        assert_eq!(
            match_host_shortcut(KC_F, COMMAND | CONTROL),
            Some(HostShortcut::ToggleFullScreen)
        );
    }

    #[test]
    fn fullscreen_combo_requires_exactly_cmd_and_ctrl() {
        // Missing either modifier → not a host shortcut (goes to the guest).
        assert_eq!(match_host_shortcut(KC_F, COMMAND), None);
        assert_eq!(match_host_shortcut(KC_F, CONTROL), None);
        // Extra modifiers → pass through (so Cmd-Ctrl-Opt-F etc. reach the guest).
        assert_eq!(match_host_shortcut(KC_F, COMMAND | CONTROL | OPTION), None);
        assert_eq!(match_host_shortcut(KC_F, COMMAND | CONTROL | SHIFT), None);
    }

    #[test]
    fn cmd_ctrl_g_is_the_capture_toggle() {
        assert_eq!(
            match_host_shortcut(0x05, COMMAND | CONTROL), // G
            Some(HostShortcut::ToggleCapture)
        );
    }

    #[test]
    fn other_keys_with_cmd_ctrl_are_not_intercepted() {
        // Cmd-Ctrl-A must still reach the guest.
        assert_eq!(match_host_shortcut(KC_A, COMMAND | CONTROL), None);
    }

    #[test]
    fn bare_f_goes_to_the_guest() {
        assert_eq!(match_host_shortcut(KC_F, 0), None);
    }

    #[test]
    fn ungrab_chord_arms_on_exactly_ctrl_opt_and_fires_on_break() {
        // Ctrl alone: nothing.
        assert_eq!(ungrab_chord_step(false, CONTROL), (false, false));
        // Ctrl+Opt: armed, no fire yet.
        assert_eq!(ungrab_chord_step(false, CONTROL | OPTION), (true, false));
        // One of them lifts: fire.
        assert_eq!(ungrab_chord_step(true, CONTROL), (false, true));
        // Or both lift at once: fire.
        assert_eq!(ungrab_chord_step(true, 0), (false, true));
    }

    #[test]
    fn ungrab_chord_disarms_when_another_modifier_joins() {
        // Cmd joins the chord: disarm, and the later break must NOT fire.
        assert_eq!(
            ungrab_chord_step(true, CONTROL | OPTION | COMMAND),
            (false, false)
        );
        // Shift prevents arming in the first place.
        assert_eq!(
            ungrab_chord_step(false, CONTROL | OPTION | SHIFT),
            (false, false)
        );
        // Breaking an unarmed chord is inert.
        assert_eq!(ungrab_chord_step(false, CONTROL), (false, false));
    }

    #[test]
    fn ungrab_chord_rearms_after_a_disarm_once_exactly_ctrl_opt_again() {
        // Cmd joined (disarmed), then Cmd lifted leaving exactly Ctrl+Opt: armed again.
        assert_eq!(ungrab_chord_step(false, CONTROL | OPTION), (true, false));
    }
}
