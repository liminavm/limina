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
//   - `CGWarpMouseCursorPosition` does the re-pin; crucially `NSEvent.deltaX/deltaY` are *hardware*
//     deltas, unaffected by warping, so re-centring never corrupts the motion we send the guest.
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
/// cursor at centre, and hide it; on release, re-couple, show it, and re-assert the guest shape.
/// Shared by the local-monitor toggle ([`InputState::toggle_capture`]) and the capture tap, so the
/// two stay byte-identical. Main thread only (NSCursor hide/unhide must balance).
pub(crate) fn apply_capture_cursor(on: bool, host_cursor: &HostCursor) {
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
        unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
        if CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
            NSCursor::unhide();
        }
        host_cursor.reassert();
        log::info!("pointer capture: OFF");
    }
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
    /// Pointer-capture (relative/mouselook) mode: when set, the host cursor is grabbed (frozen
    /// and hidden) and pointer events go to the guest's relative-mouse device as `REL_X`/`REL_Y`
    /// deltas instead of the absolute tablet. Toggled by `Cmd-Ctrl-G`. Shared with the render
    /// timer (an `Arc<AtomicBool>`), which composites the guest cursor at its reported position
    /// while this is set (the host cursor is hidden, so the guest cursor has to be drawn).
    captured: Arc<AtomicBool>,
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
    ) -> Self {
        Self {
            conn,
            pressed_mods: RefCell::new(HashSet::new()),
            caps: RefCell::new(CapsLockSync::new()),
            guest_buttons: Cell::new(0),
            host_cursor,
            remap,
            captured,
            fit,
        }
    }

    fn is_captured(&self) -> bool {
        self.captured.load(Ordering::Acquire)
    }

    /// Toggle pointer capture. On grab: decouple the hardware mouse from the cursor (so deltas
    /// flow while the cursor stays frozen — mouselook) and hide the cursor. On release: restore
    /// both. Returns the new captured state. Main thread only.
    pub fn toggle_capture(&self) -> bool {
        let now = !self.is_captured();
        self.captured.store(now, Ordering::Release);
        apply_capture_cursor(now, &self.host_cursor);
        now
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
                self.emit_modifier(event.keyCode(), event.modifierFlags().0 as u64);
                true
            }
            NSEventType::MouseMoved => {
                if self.is_captured() {
                    self.emit_rel_motion(event);
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
                    self.emit_rel_motion(event);
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
        }
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
        if self.is_captured() {
            // Captured: no view gate, no absolute position — the relative mouse just clicks.
            self.guest_buttons
                .set(self.guest_buttons.get() | btn_bit(btn));
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

    fn emit_motion(&self, event: &NSEvent, view: &NSView) {
        let (x, y) = self.abs_coords(event, view);
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
        self.send_ptr(InputEvent::syn());
    }

    /// Map the event's window-local cursor position to the absolute device range through
    /// the current fit rect (letterbox offset + scale, Y flipped — AppKit is bottom-left
    /// origin, evdev top-left). Positions outside the content (drags that left it) clamp
    /// to the nearest content edge. With the fit at the full view (dynamic mode) this is
    /// the legacy full-bounds mapping, bit for bit.
    fn abs_coords(&self, event: &NSEvent, view: &NSView) -> (i32, i32) {
        // `None` source view = the point is in window base coordinates.
        let p = view.convertPoint_fromView(event.locationInWindow(), None);
        super::fit::abs_through_fit(p.x, p.y, self.fit.get(), ABS_MAX as i32)
    }

    /// Capture mode: forward the event's relative delta to the guest's relative-mouse device.
    /// `deltaX/deltaY` are raw movement (present even with the cursor frozen), and evdev `REL_Y`
    /// is positive-down like AppKit's mouse `deltaY`, so they pass through directly.
    fn emit_rel_motion(&self, event: &NSEvent) {
        let dx = event.deltaX().round() as i32;
        let dy = event.deltaY().round() as i32;
        // Re-pin the (hidden) host cursor to the display centre so it can't drift onto windows
        // behind us — CGAssociate(false) alone doesn't reliably freeze it. The warp doesn't
        // affect `deltaX/deltaY` (hardware deltas), so the guest still gets clean motion.
        unsafe { CGWarpMouseCursorPosition(main_display_center()) };
        if dx == 0 && dy == 0 {
            return;
        }
        if dx != 0 {
            self.send_ptr(InputEvent::new(EV_REL, REL_X, dx));
        }
        if dy != 0 {
            self.send_ptr(InputEvent::new(EV_REL, REL_Y, dy));
        }
        self.send_ptr(InputEvent::syn());
    }

    fn emit_scroll(&self, event: &NSEvent) {
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

    /// Send to the pointer sink for the current mode: the relative-mouse device while
    /// captured, the absolute pointer otherwise. Same snapshot-held-across-the-send rule
    /// as `send_kbd`.
    fn send_ptr(&self, ev: InputEvent) {
        let io = self.conn.io();
        let fd = if self.is_captured() {
            io.rel_ptr_fd()
        } else {
            io.ptr_fd()
        };
        send_event(fd, ev);
    }
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
}
