// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Input plumbing shared by the limina supervisor (NSEvent capture) and worker (virtio-input).
//!
//! The supervisor translates native macOS events into Linux evdev `(type, code, value)`
//! triples ([`InputEvent`]) and writes them — one per datagram — to a socket the worker
//! inherits. The worker's virtio-input backends ([`backends`], behind the `backends`
//! feature) read those triples and feed them to the guest. Both sides share the evdev
//! [`constants`] and the [`keymap`] so the wire stays consistent.

pub mod constants;
pub mod keymap;

#[cfg(feature = "backends")]
pub mod backends;

/// One Linux input event, matching the on-wire layout the worker reads (8 bytes: a
/// virtio/evdev event minus the timestamp). `value` is signed so relative deltas (wheel)
/// and key states (0/1) share one type.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

/// The fixed wire size of one [`InputEvent`] (sent as a single datagram).
pub const WIRE_LEN: usize = 8;

impl InputEvent {
    pub const fn new(type_: u16, code: u16, value: i32) -> Self {
        Self { type_, code, value }
    }

    /// Serialize to the 8-byte little-endian wire form.
    pub fn to_bytes(self) -> [u8; WIRE_LEN] {
        let mut b = [0u8; WIRE_LEN];
        b[0..2].copy_from_slice(&self.type_.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }

    /// Parse the 8-byte little-endian wire form.
    pub fn from_bytes(b: &[u8; WIRE_LEN]) -> Self {
        Self {
            type_: u16::from_le_bytes([b[0], b[1]]),
            code: u16::from_le_bytes([b[2], b[3]]),
            value: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        }
    }

    /// A `EV_SYN`/`SYN_REPORT` terminator (every batch of events ends with one).
    pub fn syn() -> Self {
        Self::new(constants::EV_SYN, constants::SYN_REPORT, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::*;
    use crate::keymap::{macos_keycode_to_linux, modifier_is_down};

    #[test]
    fn wire_roundtrip_including_negative_values() {
        for ev in [
            InputEvent::new(EV_KEY, KEY_A, 1),
            InputEvent::new(EV_ABS, ABS_X, 32767),
            InputEvent::new(EV_REL, REL_WHEEL, -1), // negative delta must survive
            InputEvent::syn(),
        ] {
            assert_eq!(InputEvent::from_bytes(&ev.to_bytes()), ev);
        }
        assert_eq!(WIRE_LEN, 8);
    }

    #[test]
    fn keymap_hits_known_positions() {
        // macOS kVK_ANSI_A = 0x00, kVK_Return = 0x24, kVK_Space = 0x31, kVK_Escape = 0x35.
        assert_eq!(macos_keycode_to_linux(0x00), Some(KEY_A));
        assert_eq!(macos_keycode_to_linux(0x24), Some(KEY_ENTER));
        assert_eq!(macos_keycode_to_linux(0x31), Some(KEY_SPACE));
        assert_eq!(macos_keycode_to_linux(0x35), Some(KEY_ESC));
        // Left vs right Command map to distinct metas.
        assert_eq!(macos_keycode_to_linux(0x37), Some(KEY_LEFTMETA));
        assert_eq!(macos_keycode_to_linux(0x36), Some(KEY_RIGHTMETA));
        // An unused keycode maps to nothing.
        assert_eq!(macos_keycode_to_linux(0xFF), None);
    }

    #[test]
    fn modifier_direction_comes_from_flags_not_a_toggle() {
        // Device-dependent (left/right) masks: LCMD=0x08, RCMD=0x10, LSHIFT=0x02.
        const LCMD: u64 = 0x08;
        const RCMD: u64 = 0x10;

        // Left Command (kVK 0x37): down iff its device bit is set in the flags.
        assert_eq!(modifier_is_down(0x37, LCMD), Some(true));
        assert_eq!(modifier_is_down(0x37, 0), Some(false));

        // The core regression: even if we somehow *missed* the press, the *release*
        // flagsChanged (LCMD bit now clear) still reads as "up" — so Super can't wedge
        // down in the guest and swallow every subsequent key. A blind toggle would have
        // inverted here and reported "down".
        assert_eq!(modifier_is_down(0x37, 0), Some(false));

        // Holding left Command while right Command taps: device bits keep them distinct,
        // so releasing one doesn't strand the other.
        assert_eq!(modifier_is_down(0x36, LCMD | RCMD), Some(true)); // right down
        assert_eq!(modifier_is_down(0x36, LCMD), Some(false)); // right up, left still held
        assert_eq!(modifier_is_down(0x37, LCMD), Some(true)); // left still down

        // Fallback to the device-independent class bit when the low word is clear
        // (some events don't carry device bits): Command class = 1<<20, Shift = 1<<17.
        assert_eq!(modifier_is_down(0x37, 1 << 20), Some(true));
        assert_eq!(modifier_is_down(0x38, 1 << 17), Some(true));
        assert_eq!(modifier_is_down(0x38, 0), Some(false));

        // Non-modifier keycodes aren't handled here.
        assert_eq!(modifier_is_down(0x00, 0xffff_ffff), None);
    }

    #[test]
    fn every_mappable_key_is_advertised() {
        // Anything the keymap can emit must be in the device's capability bitmap, or the
        // guest silently drops it.
        for kc in 0u16..=0xFF {
            if let Some(code) = macos_keycode_to_linux(kc) {
                assert!(
                    SUPPORTED_KEYBOARD_KEYS.contains(&code),
                    "keycode {kc:#x} -> KEY {code} not in SUPPORTED_KEYBOARD_KEYS"
                );
            }
        }
    }
}
