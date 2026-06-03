// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! macOS virtual keycode (`NSEvent.keyCode`, `kVK_*` from `<HIToolbox/Events.h>`) →
//! Linux evdev `KEY_*`. macOS keycodes are positional (US ANSI layout positions), so this
//! is a layout-position map — exactly what evdev wants (the guest applies its own layout).
//!
//! Modifiers keep their left/right distinction (macOS has separate keycodes for each).
//! Command → META and Option → ALT for now; the Command/Option *swap* and custom remaps
//! are a planned policy layer that will sit on top of this raw positional map.

use crate::constants::*;

/// Map a macOS virtual keycode to a Linux `KEY_*`, or `None` if limina doesn't handle it.
pub fn macos_keycode_to_linux(keycode: u16) -> Option<u16> {
    let k = match keycode {
        // Letters.
        0x00 => KEY_A,
        0x0B => KEY_B,
        0x08 => KEY_C,
        0x02 => KEY_D,
        0x0E => KEY_E,
        0x03 => KEY_F,
        0x05 => KEY_G,
        0x04 => KEY_H,
        0x22 => KEY_I,
        0x26 => KEY_J,
        0x28 => KEY_K,
        0x25 => KEY_L,
        0x2E => KEY_M,
        0x2D => KEY_N,
        0x1F => KEY_O,
        0x23 => KEY_P,
        0x0C => KEY_Q,
        0x0F => KEY_R,
        0x01 => KEY_S,
        0x11 => KEY_T,
        0x20 => KEY_U,
        0x09 => KEY_V,
        0x0D => KEY_W,
        0x07 => KEY_X,
        0x10 => KEY_Y,
        0x06 => KEY_Z,

        // Top-row digits.
        0x12 => KEY_1,
        0x13 => KEY_2,
        0x14 => KEY_3,
        0x15 => KEY_4,
        0x17 => KEY_5,
        0x16 => KEY_6,
        0x1A => KEY_7,
        0x1C => KEY_8,
        0x19 => KEY_9,
        0x1D => KEY_0,

        // Punctuation / symbols.
        0x1B => KEY_MINUS,
        0x18 => KEY_EQUAL,
        0x21 => KEY_LEFTBRACE,
        0x1E => KEY_RIGHTBRACE,
        0x2A => KEY_BACKSLASH,
        0x29 => KEY_SEMICOLON,
        0x27 => KEY_APOSTROPHE,
        0x32 => KEY_GRAVE,
        0x2B => KEY_COMMA,
        0x2F => KEY_DOT,
        0x2C => KEY_SLASH,

        // Whitespace / editing.
        0x31 => KEY_SPACE,
        0x24 => KEY_ENTER,
        0x30 => KEY_TAB,
        0x33 => KEY_BACKSPACE,
        0x35 => KEY_ESC,
        0x75 => KEY_DELETE, // forward delete

        // Modifiers (left/right distinct on macOS).
        0x37 => KEY_LEFTMETA,  // left Command
        0x36 => KEY_RIGHTMETA, // right Command
        0x38 => KEY_LEFTSHIFT,
        0x3C => KEY_RIGHTSHIFT,
        0x3A => KEY_LEFTALT, // left Option
        0x3D => KEY_RIGHTALT,
        0x3B => KEY_LEFTCTRL,
        0x3E => KEY_RIGHTCTRL,
        0x39 => KEY_CAPSLOCK,

        // Arrows.
        0x7B => KEY_LEFT,
        0x7C => KEY_RIGHT,
        0x7D => KEY_DOWN,
        0x7E => KEY_UP,

        // Navigation cluster.
        0x73 => KEY_HOME,
        0x77 => KEY_END,
        0x74 => KEY_PAGEUP,
        0x79 => KEY_PAGEDOWN,
        0x72 => KEY_INSERT, // Help

        // Function row.
        0x7A => KEY_F1,
        0x78 => KEY_F2,
        0x63 => KEY_F3,
        0x76 => KEY_F4,
        0x60 => KEY_F5,
        0x61 => KEY_F6,
        0x62 => KEY_F7,
        0x64 => KEY_F8,
        0x65 => KEY_F9,
        0x6D => KEY_F10,
        0x67 => KEY_F11,
        0x6F => KEY_F12,
        0x69 => KEY_F13,
        0x6B => KEY_F14,
        0x71 => KEY_F15,
        0x6A => KEY_F16,
        0x40 => KEY_F17,
        0x4F => KEY_F18,
        0x50 => KEY_F19,

        // Keypad.
        0x52 => KEY_KP0,
        0x53 => KEY_KP1,
        0x54 => KEY_KP2,
        0x55 => KEY_KP3,
        0x56 => KEY_KP4,
        0x57 => KEY_KP5,
        0x58 => KEY_KP6,
        0x59 => KEY_KP7,
        0x5B => KEY_KP8,
        0x5C => KEY_KP9,
        0x41 => KEY_KPDOT,
        0x43 => KEY_KPASTERISK,
        0x45 => KEY_KPPLUS,
        0x4E => KEY_KPMINUS,
        0x4B => KEY_KPSLASH,
        0x4C => KEY_KPENTER,
        0x51 => KEY_KPEQUAL,

        // Media.
        0x48 => KEY_VOLUMEUP,
        0x49 => KEY_VOLUMEDOWN,
        0x4A => KEY_MUTE,

        _ => return None,
    };
    Some(k)
}

/// Decide whether the modifier identified by `keycode` is **now down**, from the raw
/// `NSEvent.modifierFlags` bitmask carried by a `flagsChanged` event — or `None` if
/// `keycode` isn't a modifier we track.
///
/// `flagsChanged` reports *which* modifier key changed but not the direction, so we read the
/// resulting flag state instead of toggling a guess. This is self-correcting: a single
/// dropped `flagsChanged` (macOS suppresses events while Command is held and across focus
/// changes) can no longer leave a modifier wedged "down" in the guest forever.
///
/// Prefers the device-dependent (left/right-specific) low bits for an exact answer; if those
/// are absent (low word clear) it falls back to the device-independent *class* bit, which
/// loses the left/right distinction but never wedges every modifier off.
pub fn modifier_is_down(keycode: u16, flags: u64) -> Option<bool> {
    // Device-dependent masks (IOLLEvent.h `NX_DEVICE*KEYMASK`) and the matching
    // device-independent class mask (`NSEventModifierFlag*`).
    const SHIFT: u64 = 1 << 17;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    const CAPSLOCK: u64 = 1 << 16;
    let (dev, class): (u64, u64) = match keycode {
        0x37 => (0x0000_0008, COMMAND), // left Command
        0x36 => (0x0000_0010, COMMAND), // right Command
        0x38 => (0x0000_0002, SHIFT),   // left Shift
        0x3C => (0x0000_0004, SHIFT),   // right Shift
        0x3A => (0x0000_0020, OPTION),  // left Option
        0x3D => (0x0000_0040, OPTION),  // right Option
        0x3B => (0x0000_0001, CONTROL), // left Control
        0x3E => (0x0000_2000, CONTROL), // right Control
        0x39 => (0, CAPSLOCK),          // Caps Lock (latch; no left/right)
        _ => return None,
    };
    const DEV_MASK: u64 = 0x0000_ffff;
    if dev != 0 && flags & DEV_MASK != 0 {
        Some(flags & dev != 0)
    } else {
        Some(flags & class != 0)
    }
}
