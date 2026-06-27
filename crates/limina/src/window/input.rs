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
use std::sync::Arc;

use super::WorkerConn;

use objc2::rc::Retained;
use objc2_app_kit::{NSCursor, NSEvent, NSEventType, NSView};

use limina_input::constants::{
    ABS_MAX, ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL,
    REL_WHEEL,
};
use limina_input::keymap::{macos_keycode_to_linux_remapped, modifier_is_down, KeyRemap};
use limina_input::InputEvent;

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
            cursor: RefCell::new(NSCursor::arrowCursor()),
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
    /// macOS keycodes of modifiers believed to be down (toggled on each flagsChanged).
    pressed_mods: RefCell<HashSet<u16>>,
    /// Bitmask of mouse buttons whose *press* we forwarded to the guest. Releases and
    /// drags are forwarded only while set, so a click that started on the title bar (or
    /// outside the window) never reaches the guest in any form.
    guest_buttons: Cell<u8>,
    host_cursor: Rc<HostCursor>,
    /// Keyboard remap policy (e.g. the Command/Option swap), applied to every key/modifier.
    remap: KeyRemap,
}

impl InputState {
    pub fn new(conn: Arc<WorkerConn>, host_cursor: Rc<HostCursor>, remap: KeyRemap) -> Self {
        Self {
            conn,
            pressed_mods: RefCell::new(HashSet::new()),
            guest_buttons: Cell::new(0),
            host_cursor,
            remap,
        }
    }

    /// Handle one captured event. Returns `true` if it should be swallowed (not passed on
    /// to AppKit) — we swallow keys so unhandled keystrokes don't beep, but let mouse
    /// events through so the title bar / close button keep working.
    pub fn handle(&self, event: &NSEvent, view: &NSView) -> bool {
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
                if self.pointer_inside(event, view) {
                    self.emit_scroll(event);
                }
                false
            }
            _ => false,
        }
    }

    /// Is the event's pointer position inside the guest view? `false` for events with no
    /// associated window (their location is in screen coordinates and can't be mapped).
    fn pointer_inside(&self, event: &NSEvent, view: &NSView) -> bool {
        // SAFETY: we only run on the main thread (the local event monitor's thread).
        let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };
        if event.window(mtm).is_none() {
            return false;
        }
        let p = view.convertPoint_fromView(event.locationInWindow(), None);
        let b = view.bounds();
        p.x >= 0.0 && p.y >= 0.0 && p.x < b.size.width && p.y < b.size.height
    }

    fn emit_key(&self, macos_keycode: u16, down: bool) {
        if let Some(code) = macos_keycode_to_linux_remapped(macos_keycode, &self.remap) {
            self.send_kbd(InputEvent::new(EV_KEY, code, down as i32));
            self.send_kbd(InputEvent::syn());
        }
    }

    /// `flagsChanged` reports which modifier key changed but not the direction. We read the
    /// modifier's *actual* state from the event's flag bitmask (see [`modifier_is_down`])
    /// rather than toggling a guess — a single dropped `flagsChanged` (macOS suppresses
    /// events while Command is held, and across focus changes) can't then wedge a modifier
    /// "down" in the guest, which would make the compositor eat every later key. We still
    /// keep a pressed-set, but only to de-duplicate (emit one press/one release per change).
    fn emit_modifier(&self, macos_keycode: u16, raw_flags: u64) {
        // The remap changes which evdev code we emit; `modifier_is_down` below stays keyed on
        // the *physical* keycode (the macOS modifier-flag state is the physical key's).
        let Some(code) = macos_keycode_to_linux_remapped(macos_keycode, &self.remap) else {
            return;
        };
        let Some(down) = modifier_is_down(macos_keycode, raw_flags) else {
            return;
        };
        {
            let mut pressed = self.pressed_mods.borrow_mut();
            let was = pressed.contains(&macos_keycode);
            if down == was {
                return; // no actual change for this key; don't double-emit
            }
            if down {
                pressed.insert(macos_keycode);
            } else {
                pressed.remove(&macos_keycode);
            }
        }
        self.send_kbd(InputEvent::new(EV_KEY, code, down as i32));
        self.send_kbd(InputEvent::syn());
    }

    /// Forward a button press if it lands inside the guest view; remember it so the
    /// matching release (and intervening drags) follow even if the pointer has left.
    fn emit_press(&self, event: &NSEvent, view: &NSView, btn: u16) -> bool {
        if self.pointer_inside(event, view) {
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
        let (x, y) = abs_coords(event, view);
        self.send_ptr(InputEvent::new(EV_ABS, ABS_X, x));
        self.send_ptr(InputEvent::new(EV_ABS, ABS_Y, y));
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
        send_event(self.conn.kbd_fd(), ev);
    }

    fn send_ptr(&self, ev: InputEvent) {
        send_event(self.conn.ptr_fd(), ev);
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

/// Map the event's window-local cursor position to the absolute device range, flipping Y
/// (AppKit is bottom-left origin; evdev is top-left). Out-of-view positions (drags that
/// left the window) clamp to the nearest edge.
fn abs_coords(event: &NSEvent, view: &NSView) -> (i32, i32) {
    let loc = event.locationInWindow();
    // `None` source view = the point is in window base coordinates.
    let p = view.convertPoint_fromView(loc, None);
    let bounds = view.bounds();
    let w = bounds.size.width.max(1.0);
    let h = bounds.size.height.max(1.0);
    let fx = (p.x / w).clamp(0.0, 1.0);
    let fy = (1.0 - p.y / h).clamp(0.0, 1.0);
    (
        (fx * ABS_MAX as f64).round() as i32,
        (fy * ABS_MAX as f64).round() as i32,
    )
}

fn send_event(fd: RawFd, ev: InputEvent) {
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
