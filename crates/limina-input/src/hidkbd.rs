// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! evdev → USB HID keyboard translation, for the gadget that carries keys through the
//! **pre-driver window** (`docs/design/usb-hid-keyboard.md`).
//!
//! Between `ExitBootServices` and the moment the guest binds `virtio_input`, the guest has no
//! keyboard: the firmware's `VirtioKeyboardDxe` resets the virtio device on its way out, and
//! neither stock initramfs generator ships `virtio_input`. Both *do* ship USB HID — a bare-metal
//! LUKS prompt requires it — so limina presents a USB keyboard gadget for exactly that window.
//!
//! This module is the lossy half of the trip. limina speaks evdev everywhere else (that is the
//! whole point of virtio-input: `KEY_*` codes cross verbatim), but HID has its own numbering, so
//! a key with no usage on page 0x07 simply cannot be expressed. That set is
//! [`KEYS_WITHOUT_HID_USAGE`] — the media transport keys, which would need a second
//! consumer-control collection. A passphrase is ASCII; the gap is accepted, not worked around.
//!
//! The report is the standard 8-byte keyboard shape: a modifier bitmap, a reserved byte, and a
//! six-slot array of held usages. It is a **diffed** report, not a stream of events, so
//! [`KeyboardReport`] holds the state the wire form is derived from.

use crate::constants::*;

/// Size of the input report the gadget sends (modifiers + reserved + 6 key slots).
pub const REPORT_LEN: usize = 8;

/// How many keys the report can hold at once (the classic 6-key rollover).
const ROLLOVER: usize = 6;

/// Keys limina's virtual keyboard advertises that have **no** usage on HID page 0x07, and so
/// cannot type through the USB gadget. They are the media transport keys, which live on the
/// consumer page (0x0C) and would need a second HID collection to express — deferred, because
/// nothing in the window this gadget serves (a passphrase, an emergency shell) needs them.
///
/// A key added to `SUPPORTED_KEYBOARD_KEYS` must either gain a usage below or be listed here,
/// so the choice is made deliberately rather than discovered from a key that silently does
/// nothing at a LUKS prompt.
pub const KEYS_WITHOUT_HID_USAGE: &[u16] = &[KEY_NEXTSONG, KEY_PLAYPAUSE, KEY_PREVIOUSSONG];

/// The modifier bit for a modifier key, matching HID usages 0xE0..=0xE7 in order:
/// left ctrl/shift/alt/meta then right ctrl/shift/alt/meta.
pub fn modifier_bit(key: u16) -> Option<u8> {
    let bit = match key {
        KEY_LEFTCTRL => 0,
        KEY_LEFTSHIFT => 1,
        KEY_LEFTALT => 2,
        KEY_LEFTMETA => 3,
        KEY_RIGHTCTRL => 4,
        KEY_RIGHTSHIFT => 5,
        KEY_RIGHTALT => 6,
        KEY_RIGHTMETA => 7,
        _ => return None,
    };
    Some(1 << bit)
}

/// The HID usage (page 0x07, "Keyboard/Keypad") for a non-modifier evdev key code. The inverse
/// of the kernel's `hid_keyboard[]` table in `drivers/hid/hid-input.c`, so a report we send
/// comes back out of the guest's `hid-generic` as the same `KEY_*` the supervisor sent.
pub fn key_usage(key: u16) -> Option<u8> {
    Some(match key {
        KEY_A => 0x04,
        KEY_B => 0x05,
        KEY_C => 0x06,
        KEY_D => 0x07,
        KEY_E => 0x08,
        KEY_F => 0x09,
        KEY_G => 0x0a,
        KEY_H => 0x0b,
        KEY_I => 0x0c,
        KEY_J => 0x0d,
        KEY_K => 0x0e,
        KEY_L => 0x0f,
        KEY_M => 0x10,
        KEY_N => 0x11,
        KEY_O => 0x12,
        KEY_P => 0x13,
        KEY_Q => 0x14,
        KEY_R => 0x15,
        KEY_S => 0x16,
        KEY_T => 0x17,
        KEY_U => 0x18,
        KEY_V => 0x19,
        KEY_W => 0x1a,
        KEY_X => 0x1b,
        KEY_Y => 0x1c,
        KEY_Z => 0x1d,
        KEY_1 => 0x1e,
        KEY_2 => 0x1f,
        KEY_3 => 0x20,
        KEY_4 => 0x21,
        KEY_5 => 0x22,
        KEY_6 => 0x23,
        KEY_7 => 0x24,
        KEY_8 => 0x25,
        KEY_9 => 0x26,
        KEY_0 => 0x27,
        KEY_ENTER => 0x28,
        KEY_ESC => 0x29,
        KEY_BACKSPACE => 0x2a,
        KEY_TAB => 0x2b,
        KEY_SPACE => 0x2c,
        KEY_MINUS => 0x2d,
        KEY_EQUAL => 0x2e,
        KEY_LEFTBRACE => 0x2f,
        KEY_RIGHTBRACE => 0x30,
        KEY_BACKSLASH => 0x31,
        KEY_SEMICOLON => 0x33,
        KEY_APOSTROPHE => 0x34,
        KEY_GRAVE => 0x35,
        KEY_COMMA => 0x36,
        KEY_DOT => 0x37,
        KEY_SLASH => 0x38,
        KEY_CAPSLOCK => 0x39,
        KEY_F1 => 0x3a,
        KEY_F2 => 0x3b,
        KEY_F3 => 0x3c,
        KEY_F4 => 0x3d,
        KEY_F5 => 0x3e,
        KEY_F6 => 0x3f,
        KEY_F7 => 0x40,
        KEY_F8 => 0x41,
        KEY_F9 => 0x42,
        KEY_F10 => 0x43,
        KEY_F11 => 0x44,
        KEY_F12 => 0x45,
        KEY_SCROLLLOCK => 0x47,
        KEY_INSERT => 0x49,
        KEY_HOME => 0x4a,
        KEY_PAGEUP => 0x4b,
        KEY_DELETE => 0x4c,
        KEY_END => 0x4d,
        KEY_PAGEDOWN => 0x4e,
        KEY_RIGHT => 0x4f,
        KEY_LEFT => 0x50,
        KEY_DOWN => 0x51,
        KEY_UP => 0x52,
        KEY_NUMLOCK => 0x53,
        KEY_KPSLASH => 0x54,
        KEY_KPASTERISK => 0x55,
        KEY_KPMINUS => 0x56,
        KEY_KPPLUS => 0x57,
        KEY_KPENTER => 0x58,
        KEY_KP1 => 0x59,
        KEY_KP2 => 0x5a,
        KEY_KP3 => 0x5b,
        KEY_KP4 => 0x5c,
        KEY_KP5 => 0x5d,
        KEY_KP6 => 0x5e,
        KEY_KP7 => 0x5f,
        KEY_KP8 => 0x60,
        KEY_KP9 => 0x61,
        KEY_KP0 => 0x62,
        KEY_KPDOT => 0x63,
        KEY_KPEQUAL => 0x67,
        KEY_F13 => 0x68,
        KEY_F14 => 0x69,
        KEY_F15 => 0x6a,
        KEY_F16 => 0x6b,
        KEY_F17 => 0x6c,
        KEY_F18 => 0x6d,
        KEY_F19 => 0x6e,
        KEY_MUTE => 0x7f,
        KEY_VOLUMEUP => 0x80,
        KEY_VOLUMEDOWN => 0x81,
        _ => return None,
    })
}

/// The HID report descriptor for the gadget: the standard keyboard collection — an 8-bit
/// modifier bitmap, a reserved byte, a 5-bit LED output report (+3 bits padding), and six
/// 8-bit key slots.
///
/// The key array declares usages 0x00..=0xFF rather than the boot descriptor's 0x00..=0x65,
/// so the usages above 101 that [`key_usage`] emits (mute and the two volume keys) are inside
/// the declared range; a report carrying an out-of-range usage is discarded by the guest's HID
/// core, which would look like a dead key rather than a descriptor bug.
#[rustfmt::skip]
pub fn report_descriptor() -> Vec<u8> {
    vec![
        0x05, 0x01,       // Usage Page (Generic Desktop)
        0x09, 0x06,       // Usage (Keyboard)
        0xa1, 0x01,       // Collection (Application)
        0x05, 0x07,       //   Usage Page (Keyboard/Keypad)
        0x19, 0xe0,       //   Usage Minimum (Left Control)
        0x29, 0xe7,       //   Usage Maximum (Right GUI)
        0x15, 0x00,       //   Logical Minimum (0)
        0x25, 0x01,       //   Logical Maximum (1)
        0x75, 0x01,       //   Report Size (1)
        0x95, 0x08,       //   Report Count (8)
        0x81, 0x02,       //   Input (Data, Var, Abs)     — modifier bitmap
        0x95, 0x01,       //   Report Count (1)
        0x75, 0x08,       //   Report Size (8)
        0x81, 0x03,       //   Input (Const, Var, Abs)    — reserved byte
        0x95, 0x05,       //   Report Count (5)
        0x75, 0x01,       //   Report Size (1)
        0x05, 0x08,       //   Usage Page (LEDs)
        0x19, 0x01,       //   Usage Minimum (Num Lock)
        0x29, 0x05,       //   Usage Maximum (Kana)
        0x91, 0x02,       //   Output (Data, Var, Abs)    — LED report (a SET_REPORT control)
        0x95, 0x01,       //   Report Count (1)
        0x75, 0x03,       //   Report Size (3)
        0x91, 0x03,       //   Output (Const, Var, Abs)   — LED padding
        0x95, 0x06,       //   Report Count (6)
        0x75, 0x08,       //   Report Size (8)
        0x15, 0x00,       //   Logical Minimum (0)
        0x26, 0xff, 0x00, //   Logical Maximum (255)
        0x05, 0x07,       //   Usage Page (Keyboard/Keypad)
        0x19, 0x00,       //   Usage Minimum (0)
        0x2a, 0xff, 0x00, //   Usage Maximum (255)
        0x81, 0x00,       //   Input (Data, Ary, Abs)     — six held-key slots
        0xc0,             // End Collection
    ]
}

/// The held-key state the 8-byte wire report is derived from. HID sends a *diff*: each report
/// is the full current state, so a keyboard gadget has to keep the state a stream of evdev
/// press/release events only implies.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardReport {
    mods: u8,
    keys: [u8; ROLLOVER],
}

impl KeyboardReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one evdev key event into the state, returning the report to send if it changed.
    ///
    /// `value` follows evdev: 0 = release, 1 = press, 2 = autorepeat. Autorepeat produces no
    /// report — the key is already in the array and USB hosts run their own typematic, so
    /// forwarding it would be a no-op report at best and a double-type at worst. A key with
    /// no HID usage ([`KEYS_WITHOUT_HID_USAGE`]) and a press beyond the six-key rollover both
    /// return `None`: nothing changed that the wire can express.
    pub fn apply(&mut self, code: u16, value: i32) -> Option<[u8; REPORT_LEN]> {
        if value == 2 {
            return None;
        }
        let down = value != 0;
        let before = *self;

        if let Some(bit) = modifier_bit(code) {
            if down {
                self.mods |= bit;
            } else {
                self.mods &= !bit;
            }
        } else {
            let usage = key_usage(code)?;
            if down {
                if !self.keys.contains(&usage) {
                    // Beyond the rollover the report cannot say the key is down; drop it
                    // rather than evict a key the user is still holding.
                    let slot = self.keys.iter().position(|&k| k == 0)?;
                    self.keys[slot] = usage;
                }
            } else if let Some(slot) = self.keys.iter().position(|&k| k == usage) {
                // Compact, so the array stays a prefix of held keys.
                self.keys.copy_within(slot + 1.., slot);
                self.keys[ROLLOVER - 1] = 0;
            }
        }

        (*self != before).then(|| self.wire())
    }

    /// Clear every held key and modifier, returning the report that says so. Sent when the
    /// gadget stops carrying keys (the guest's `virtio_input` bound, or the VM rebooted) so a
    /// key held across the handoff cannot stay stuck down in the guest.
    pub fn release_all(&mut self) -> [u8; REPORT_LEN] {
        *self = Self::default();
        self.wire()
    }

    /// True when no key or modifier is held.
    pub fn is_idle(&self) -> bool {
        *self == Self::default()
    }

    fn wire(&self) -> [u8; REPORT_LEN] {
        let mut r = [0u8; REPORT_LEN];
        r[0] = self.mods;
        r[2..].copy_from_slice(&self.keys);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key the virtual keyboard advertises must either translate to HID or be on the
    /// documented no-usage list. A new key in `SUPPORTED_KEYBOARD_KEYS` fails this until
    /// someone decides which it is — the alternative is a key that silently does nothing at a
    /// LUKS prompt and nowhere else, which is the hardest kind of gap to notice.
    #[test]
    fn every_advertised_key_is_translated_or_documented_as_untranslatable() {
        for &key in SUPPORTED_KEYBOARD_KEYS {
            let translated = modifier_bit(key).is_some() || key_usage(key).is_some();
            let excused = KEYS_WITHOUT_HID_USAGE.contains(&key);
            assert!(
                translated != excused,
                "KEY {key} is neither translated nor listed in KEYS_WITHOUT_HID_USAGE \
                 (or is listed but also translated)"
            );
        }
    }

    /// The usages must be distinct, or two keys collapse into one in the guest.
    #[test]
    fn usages_are_unique() {
        let mut seen = std::collections::HashMap::new();
        for &key in SUPPORTED_KEYBOARD_KEYS {
            if let Some(u) = key_usage(key) {
                if let Some(prev) = seen.insert(u, key) {
                    panic!("usage {u:#04x} claimed by both KEY {prev} and KEY {key}");
                }
            }
        }
    }

    #[test]
    fn a_press_and_release_round_trips_through_the_report() {
        let mut r = KeyboardReport::new();
        let down = r.apply(KEY_A, 1).expect("press changes the report");
        assert_eq!(down, [0, 0, 0x04, 0, 0, 0, 0, 0]);
        assert!(!r.is_idle());
        let up = r.apply(KEY_A, 0).expect("release changes the report");
        assert_eq!(up, [0u8; REPORT_LEN]);
        assert!(r.is_idle());
    }

    #[test]
    fn modifiers_live_in_the_bitmap_not_the_key_array() {
        let mut r = KeyboardReport::new();
        assert_eq!(
            r.apply(KEY_LEFTSHIFT, 1).unwrap(),
            [0x02, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            r.apply(KEY_1, 1).unwrap(),
            [0x02, 0, 0x1e, 0, 0, 0, 0, 0],
            "shift stays held while the key types"
        );
        assert_eq!(r.apply(KEY_1, 0).unwrap(), [0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r.apply(KEY_LEFTSHIFT, 0).unwrap(), [0u8; REPORT_LEN]);
    }

    /// evdev repeats a held key; USB does not. Forwarding a repeat would send a report
    /// identical to the last one at best, and re-type through the host's own typematic at
    /// worst — so a repeat produces nothing.
    #[test]
    fn autorepeat_produces_no_report() {
        let mut r = KeyboardReport::new();
        r.apply(KEY_A, 1).unwrap();
        assert_eq!(r.apply(KEY_A, 2), None);
        assert_eq!(r.apply(KEY_A, 2), None);
    }

    /// A release must not shift the *other* held keys out of the array, and a re-press of an
    /// already-held key must not consume a second slot.
    #[test]
    fn releasing_one_key_leaves_the_others_held() {
        let mut r = KeyboardReport::new();
        for k in [KEY_A, KEY_S, KEY_D] {
            r.apply(k, 1).unwrap();
        }
        assert_eq!(r.apply(KEY_A, 1), None, "already held; no change");
        let after = r.apply(KEY_S, 0).unwrap();
        assert_eq!(after[2..], [0x04, 0x07, 0, 0, 0, 0], "A and D still down");
    }

    /// Six is the rollover. A seventh key cannot be expressed, so it is dropped rather than
    /// evicting a key the user is still holding — and its release is then a no-op too, which
    /// is what keeps the state consistent.
    #[test]
    fn a_seventh_key_is_dropped_not_swapped_in() {
        let mut r = KeyboardReport::new();
        let six = [KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F];
        for k in six {
            r.apply(k, 1).unwrap();
        }
        assert_eq!(r.apply(KEY_G, 1), None, "no slot for a seventh key");
        assert_eq!(r.apply(KEY_G, 0), None, "and its release changes nothing");
        let after = r.apply(KEY_A, 0).unwrap();
        assert_eq!(after[2..], [0x05, 0x06, 0x07, 0x08, 0x09, 0], "B..F held");
    }

    /// The handoff report: whatever was held, the guest is told everything is up. Without it
    /// a modifier held across the moment `virtio_input` binds stays down in the guest forever.
    #[test]
    fn release_all_clears_modifiers_and_keys() {
        let mut r = KeyboardReport::new();
        r.apply(KEY_LEFTCTRL, 1).unwrap();
        r.apply(KEY_C, 1).unwrap();
        assert_eq!(r.release_all(), [0u8; REPORT_LEN]);
        assert!(r.is_idle());
    }

    /// The descriptor is parsed by the guest's HID core; a truncated or malformed one yields
    /// no input device at all. Pin the shape that matters: it ends the collection, and the key
    /// array's logical maximum covers the highest usage [`key_usage`] can emit.
    #[test]
    fn the_report_descriptor_declares_every_usage_we_can_send() {
        let d = report_descriptor();
        assert_eq!(*d.last().unwrap(), 0xc0, "collection closed");
        let highest = SUPPORTED_KEYBOARD_KEYS
            .iter()
            .filter_map(|&k| key_usage(k))
            .max()
            .unwrap();
        // Logical Maximum (255) as a two-byte item — the boot descriptor's 101 would clip.
        assert!(
            d.windows(3).any(|w| w == [0x26, 0xff, 0x00]),
            "key array declares logical maximum 255, needed for usage {highest:#04x}"
        );
    }
}
