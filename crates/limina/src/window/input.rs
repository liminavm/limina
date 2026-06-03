// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Translate captured `NSEvent`s into Linux evdev events and ship them to the worker.
//!
//! Runs entirely on the main thread (from the window's local event monitor). Keyboard and
//! pointer go to separate sockets matching the worker's two virtio-input devices. Each
//! logical change is committed with a trailing `EV_SYN`/`SYN_REPORT`, exactly as a real
//! evdev device would. The pointer is absolute: window-local cursor coordinates are scaled
//! into the device's `0..=ABS_MAX` range, so the guest cursor tracks the macOS cursor 1:1.

use std::cell::RefCell;
use std::collections::HashSet;
use std::os::fd::RawFd;

use objc2_app_kit::{NSEvent, NSEventType, NSView};

use limina_input::constants::{
    ABS_MAX, ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL,
    REL_WHEEL,
};
use limina_input::keymap::{macos_keycode_to_linux, modifier_is_down};
use limina_input::InputEvent;

/// The supervisor ends of the two input sockets (one datagram = one event).
#[derive(Clone, Copy)]
pub struct InputSinks {
    pub kbd_fd: RawFd,
    pub ptr_fd: RawFd,
}

/// Main-thread input translator. Tracks which modifier keys are currently held so
/// `flagsChanged` (which carries no up/down) can be turned into press/release pairs.
pub struct InputState {
    sinks: InputSinks,
    /// macOS keycodes of modifiers believed to be down (toggled on each flagsChanged).
    pressed_mods: RefCell<HashSet<u16>>,
}

impl InputState {
    pub fn new(sinks: InputSinks) -> Self {
        Self {
            sinks,
            pressed_mods: RefCell::new(HashSet::new()),
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
            NSEventType::MouseMoved
            | NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDragged => {
                self.emit_motion(event, view);
                false
            }
            NSEventType::LeftMouseDown => self.emit_button(BTN_LEFT, true),
            NSEventType::LeftMouseUp => self.emit_button(BTN_LEFT, false),
            NSEventType::RightMouseDown => self.emit_button(BTN_RIGHT, true),
            NSEventType::RightMouseUp => self.emit_button(BTN_RIGHT, false),
            NSEventType::OtherMouseDown => self.emit_other_button(event, true),
            NSEventType::OtherMouseUp => self.emit_other_button(event, false),
            NSEventType::ScrollWheel => {
                self.emit_scroll(event);
                false
            }
            _ => false,
        }
    }

    fn emit_key(&self, macos_keycode: u16, down: bool) {
        if let Some(code) = macos_keycode_to_linux(macos_keycode) {
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
        let Some(code) = macos_keycode_to_linux(macos_keycode) else {
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

    fn emit_button(&self, btn: u16, down: bool) -> bool {
        self.send_ptr(InputEvent::new(EV_KEY, btn, down as i32));
        self.send_ptr(InputEvent::syn());
        false
    }

    fn emit_other_button(&self, event: &NSEvent, down: bool) -> bool {
        // buttonNumber 2 is the middle button; ignore further buttons for now.
        if event.buttonNumber() == 2 {
            self.emit_button(BTN_MIDDLE, down);
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
        send_event(self.sinks.kbd_fd, ev);
    }

    fn send_ptr(&self, ev: InputEvent) {
        send_event(self.sinks.ptr_fd, ev);
    }
}

/// Map the event's window-local cursor position to the absolute device range, flipping Y
/// (AppKit is bottom-left origin; evdev is top-left).
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
