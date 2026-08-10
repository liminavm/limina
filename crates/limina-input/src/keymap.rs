// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! macOS virtual keycode (`NSEvent.keyCode`, `kVK_*` from `<HIToolbox/Events.h>`) →
//! Linux evdev `KEY_*`. macOS keycodes are positional (US ANSI layout positions), so this
//! is a layout-position map — exactly what evdev wants (the guest applies its own layout).
//!
//! Modifiers keep their left/right distinction (macOS has separate keycodes for each).
//! Command → META and Option → ALT in the raw map; the Command/Option *swap* and custom
//! remaps are a policy layer ([`KeyRemap`]) that sits on top of this raw positional map —
//! call [`macos_keycode_to_linux_remapped`] from the input path, never the raw map directly.

use crate::constants::*;

/// Keyboard remap **policy**, applied on top of the raw positional map. Host-side and
/// per-config (a libkrun-free limina feature). Extensible; for now it carries the headline
/// macOS-ergonomics knob, the Command/Option swap. `Default` = identity (no remap).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyRemap {
    /// Swap the Command and Option keys: Command then acts as Alt and Option acts as
    /// Meta/Super (both left and right). The common ask from users coming off a PC layout —
    /// it puts Alt under the thumb where Command sits. The guest still owns the keyboard
    /// *layout* (dead keys/IME), so this only swaps the two modifiers' evdev identities.
    pub swap_cmd_opt: bool,
}

impl KeyRemap {
    /// Apply the remap policy to a raw Linux `KEY_*` code.
    fn apply(&self, code: u16) -> u16 {
        if self.swap_cmd_opt {
            // Swap on the *output* modifier identity, so Command↔Alt and Option↔Meta hold
            // regardless of which physical key produced META/ALT.
            return match code {
                KEY_LEFTMETA => KEY_LEFTALT,
                KEY_LEFTALT => KEY_LEFTMETA,
                KEY_RIGHTMETA => KEY_RIGHTALT,
                KEY_RIGHTALT => KEY_RIGHTMETA,
                other => other,
            };
        }
        code
    }
}

/// Map a macOS keycode to a Linux `KEY_*`, applying the [`KeyRemap`] policy. **This is the
/// function the input path calls**; [`macos_keycode_to_linux`] is the raw layer underneath.
/// `modifier_is_down` stays keyed on the *physical* keycode (the swap changes only which
/// evdev code we emit, not how we read the macOS modifier state).
pub fn macos_keycode_to_linux_remapped(keycode: u16, remap: &KeyRemap) -> Option<u16> {
    macos_keycode_to_linux(keycode).map(|code| remap.apply(code))
}

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

/// The macOS virtual keycodes of every **held** modifier key — the left/right pairs of
/// Command, Shift, Option and Control. Caps Lock is deliberately absent: it is a *lock* key
/// owned by [`CapsLockSync`], not a hold.
pub const MODIFIER_KEYCODES: [u16; 8] = [0x37, 0x36, 0x38, 0x3C, 0x3A, 0x3D, 0x3B, 0x3E];

/// Every held-modifier edge needed to make the guest's believed state match the host bitmask,
/// as `(macOS keycode, is_down)` pairs — **releases first, then presses**.
///
/// This is the held-modifier twin of [`CapsLockSync`], and it exists for the same reason: a
/// `flagsChanged` says which modifier *changed*, never what the whole keyboard is doing, so a
/// modifier that goes down while we aren't looking is invisible to us. macOS sends no
/// reconciling edge when focus or the Space comes back, and there is nothing to "catch up" on
/// later — the key never moves again until it is released. Feeding the full bitmask from every
/// event closes that gap the same way the caps sync does.
///
/// Found by the 2026-08-09 repro (`spikes/modifier-drift/`): Ctrl held across a Space switch was
/// released in the guest on the way out and never restored on the way back, so Super arrived
/// bare and GNOME opened the overview. The Ctrl bit was in every bitmask we looked at the whole
/// time — [`modifier_emit`] just never asks about any key but the one the event named.
///
/// `except` is the keycode the *caller* is about to emit itself (the event's own modifier).
/// Excluding it is load-bearing, not an optimization: this function walks
/// [`MODIFIER_KEYCODES`] in a fixed order in which Option precedes Control, so reconciling the
/// event's own key here would emit Super *before* the Ctrl it is supposed to be modified by —
/// reproducing the very bug one step further downstream.
///
/// Releases come before presses so a modifier that changed hands out of view (left Ctrl up,
/// right Ctrl down) is never briefly down on both sides.
pub fn reconcile_modifiers(
    flags: u64,
    believed: &std::collections::HashSet<u16>,
    except: Option<u16>,
) -> Vec<(u16, bool)> {
    // Whether this bitmask carries device-dependent (left/right-specific) bits at all.
    //
    // Presses are gated on it, and that gate is the difference between healing and corrupting.
    // `modifier_is_down` falls back to the device-INDEPENDENT class bit when the low word is
    // clear, which is exactly right when you are asking about one named key (the caller already
    // knows which side moved) and exactly wrong here: asked about all eight, a lone CONTROL class
    // bit answers "down" for BOTH Controls, and this function would press two keys the user is
    // not holding. Releases need no such gate — a clear class bit means both sides are genuinely
    // up, and a set one simply yields no edge.
    let precise = flags & 0x0000_ffff != 0;
    let mut releases = Vec::new();
    let mut presses = Vec::new();
    for kc in MODIFIER_KEYCODES {
        if Some(kc) == except {
            continue;
        }
        let Some(down) = modifier_is_down(kc, flags) else {
            continue;
        };
        if down == believed.contains(&kc) {
            continue;
        }
        if down {
            if precise {
                presses.push((kc, true));
            }
        } else {
            releases.push((kc, false));
        }
    }
    releases.extend(presses);
    releases
}

/// The macOS virtual keycode for Caps Lock (`kVK_CapsLock`).
pub const MACOS_KC_CAPSLOCK: u16 = 0x39;

/// `NSEventModifierFlagCapsLock` — the caps-lock LED (lock) bit in a `modifierFlags` bitmask.
const CAPSLOCK_FLAG: u64 = 1 << 16;

/// The host caps-lock LED (lock) state carried by a `modifierFlags` bitmask.
pub fn capslock_on(flags: u64) -> bool {
    flags & CAPSLOCK_FLAG != 0
}

/// What a `flagsChanged` should emit for a **held** modifier (Shift/Ctrl/Cmd/Opt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModEmit {
    /// State unchanged for this key — emit nothing (dedup), keep the pressed-state as is.
    None,
    /// The modifier crossed an edge: press (`true`) or release (`false`). The bool is also the
    /// key's new pressed-state.
    Edge(bool),
}

/// Decide what a `flagsChanged` on `keycode` should emit for a **held** modifier, given the raw
/// flag bitmask and our current belief about whether the key is held. `None` if `keycode` isn't
/// a held modifier we track — including **Caps Lock**, a *lock* key whose macOS flag is an LED
/// state (not a hold) and which is handled by [`CapsLockSync`], not here. Pure (keyed on the
/// *physical* keycode, like [`modifier_is_down`]) so the input path stays a thin wrapper.
pub fn modifier_emit(keycode: u16, flags: u64, was_pressed: bool) -> Option<ModEmit> {
    if keycode == MACOS_KC_CAPSLOCK {
        return None; // Caps Lock is a lock key — handled by CapsLockSync, not as a held modifier.
    }
    let down = modifier_is_down(keycode, flags)?;
    Some(if down == was_pressed {
        ModEmit::None
    } else {
        ModEmit::Edge(down)
    })
}

/// Keeps the guest's caps-lock aligned with the host's caps LED. Caps Lock is a *lock* key: the
/// macOS flag reports the LED state, and the guest toggles its own lock on each key *press*, so
/// they stay aligned only if each host toggle becomes exactly one guest press+release tap. A
/// blind tap-per-event breaks the moment the host LED is toggled while the VM is unfocused — the
/// event monitor sees nothing and macOS sends no reconciling `flagsChanged` on refocus, so the
/// guest ends up one toggle out of phase (stuck/inverted).
///
/// Instead we track the believed guest state and tap only when the host LED actually differs.
/// Feeding the LED bit from *every* event's modifier flags (key, pointer, flagsChanged) makes it
/// self-healing: the first interaction after refocus re-syncs the guest.
#[derive(Clone, Copy, Debug, Default)]
pub struct CapsLockSync {
    guest_on: bool,
}

impl CapsLockSync {
    /// A fresh sync — the guest boots with caps off.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the current host caps LED state. Returns `true` if the guest needs a toggle tap
    /// (and records the new state); `false` if already in sync.
    pub fn observe(&mut self, led_on: bool) -> bool {
        if led_on == self.guest_on {
            false
        } else {
            self.guest_on = led_on;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // macOS keycodes for the four modifier keys involved in the swap.
    const L_CMD: u16 = 0x37;
    const R_CMD: u16 = 0x36;
    const L_OPT: u16 = 0x3A;
    const R_OPT: u16 = 0x3D;
    const L_CTRL: u16 = 0x3B;
    const R_CTRL: u16 = 0x3E;

    /// Flag bitmasks lifted verbatim from the 2026-08-09 `LIMINA_INPUT_TRACE` repro
    /// (`spikes/modifier-drift/trace-2026-08-09.log`) — real events, not hand-built constants,
    /// so a decode mistake here would have shown up as a wrong diagnosis there first.
    /// Left Control held alone, and left Control + left Option held together.
    const F_LCTRL: u64 = 0x40101;
    const F_LCTRL_LOPT: u64 = 0xc0121;

    fn believed(keys: &[u16]) -> std::collections::HashSet<u16> {
        keys.iter().copied().collect()
    }

    #[test]
    fn a_modifier_held_across_a_focus_change_is_re_announced() {
        // THE REPORTED BUG. Ctrl went down while our window was on another Space, so the guest
        // was told to release it and no `flagsChanged` ever arrives to say it came back. The
        // host bitmask says it is down; our believed set says it is not; the reconcile is the
        // only thing that can close that gap.
        assert_eq!(
            reconcile_modifiers(F_LCTRL, &believed(&[]), None),
            vec![(L_CTRL, true)],
        );
    }

    #[test]
    fn the_events_own_key_is_excluded_so_its_edge_stays_last() {
        // Ctrl (held, unseen) + the Option press that is about to be forwarded by the caller.
        // `L_OPT` must NOT come back from the reconcile: the caller emits it right after, and
        // emitting it here would put Super *before* Ctrl in the guest's event stream — which is
        // the bare-Super overview all over again, just one step further along.
        assert_eq!(
            reconcile_modifiers(F_LCTRL_LOPT, &believed(&[]), Some(L_OPT)),
            vec![(L_CTRL, true)],
        );
    }

    #[test]
    fn a_modifier_released_out_of_view_is_taken_back() {
        // The mirror image: we believe Ctrl is down, the host says it is not. Left stuck, a
        // wedged modifier makes the guest compositor eat every later key.
        assert_eq!(
            reconcile_modifiers(0x100, &believed(&[L_CTRL]), None),
            vec![(L_CTRL, false)],
        );
    }

    #[test]
    fn an_agreeing_state_emits_nothing() {
        // The overwhelmingly common case — this runs on every event, so it must be silent.
        assert!(reconcile_modifiers(F_LCTRL, &believed(&[L_CTRL]), None).is_empty());
        assert!(reconcile_modifiers(0x100, &believed(&[]), None).is_empty());
    }

    #[test]
    fn releases_precede_presses_so_a_side_switch_never_doubles_up() {
        // Ctrl swapped hands out of view. Emitting the press first would leave both Controls
        // down in the guest for one event.
        let out = reconcile_modifiers(0x42000, &believed(&[L_CTRL]), None);
        assert_eq!(out, vec![(L_CTRL, false), (R_CTRL, true)]);
    }

    #[test]
    fn a_class_only_bitmask_never_presses_but_still_releases() {
        // No device-dependent bits: CONTROL is down but the mask cannot say which one. Pressing
        // on this evidence would put BOTH Controls down in the guest — worse than the drift it
        // is trying to heal.
        const CONTROL_CLASS: u64 = 1 << 18;
        assert!(reconcile_modifiers(CONTROL_CLASS, &believed(&[]), None).is_empty());
        // But a class bit that is *clear* is unambiguous — both sides are up, so a stale
        // believed-press is still taken back.
        assert_eq!(
            reconcile_modifiers(0, &believed(&[L_CTRL]), None),
            vec![(L_CTRL, false)],
        );
    }

    #[test]
    fn caps_lock_is_never_reconciled_here() {
        // Caps Lock is a *lock* key: its flag is an LED, and CapsLockSync owns it. Treating the
        // LED as a hold would emit a press that the guest latches — the exact bug that sync
        // exists to avoid.
        let caps_on = 1 << 16;
        assert!(reconcile_modifiers(caps_on, &believed(&[]), None).is_empty());
    }

    #[test]
    fn identity_remap_matches_the_raw_map() {
        let id = KeyRemap::default();
        assert!(!id.swap_cmd_opt);
        for kc in 0u16..=0x7f {
            assert_eq!(
                macos_keycode_to_linux_remapped(kc, &id),
                macos_keycode_to_linux(kc),
                "default remap must be identity for keycode {kc:#x}"
            );
        }
    }

    #[test]
    fn swap_cmd_opt_exchanges_meta_and_alt_both_sides() {
        let swap = KeyRemap { swap_cmd_opt: true };
        // Command keys now act as Alt.
        assert_eq!(
            macos_keycode_to_linux_remapped(L_CMD, &swap),
            Some(KEY_LEFTALT)
        );
        assert_eq!(
            macos_keycode_to_linux_remapped(R_CMD, &swap),
            Some(KEY_RIGHTALT)
        );
        // Option keys now act as Meta/Super.
        assert_eq!(
            macos_keycode_to_linux_remapped(L_OPT, &swap),
            Some(KEY_LEFTMETA)
        );
        assert_eq!(
            macos_keycode_to_linux_remapped(R_OPT, &swap),
            Some(KEY_RIGHTMETA)
        );
    }

    #[test]
    fn swap_leaves_non_cmd_opt_keys_untouched() {
        let swap = KeyRemap { swap_cmd_opt: true };
        // A letter, a digit, Shift, and Control are unaffected by the Command/Option swap.
        for kc in [
            0x00u16, /*A*/
            0x12,    /*1*/
            0x38,    /*L Shift*/
            0x3B,    /*L Ctrl*/
        ] {
            assert_eq!(
                macos_keycode_to_linux_remapped(kc, &swap),
                macos_keycode_to_linux(kc),
                "swap must not touch keycode {kc:#x}"
            );
        }
    }

    #[test]
    fn swap_is_an_involution() {
        // Swapping the swapped output returns the original — sanity on the mapping pairs.
        let swap = KeyRemap { swap_cmd_opt: true };
        for code in [KEY_LEFTMETA, KEY_RIGHTMETA, KEY_LEFTALT, KEY_RIGHTALT] {
            assert_eq!(swap.apply(swap.apply(code)), code);
        }
    }

    // --- modifier_emit (held modifiers) + Caps Lock sync ---
    const KC_CAPSLOCK: u16 = 0x39;
    const KC_LSHIFT: u16 = 0x38;
    const F_CAPSLOCK: u64 = 1 << 16; // NSEventModifierFlagCapsLock (the LED lock state)
    const F_LSHIFT: u64 = (1 << 17) | 0x0000_0002; // class SHIFT + device-dependent left-shift

    #[test]
    fn capslock_on_reads_the_led_bit() {
        assert!(capslock_on(F_CAPSLOCK));
        assert!(!capslock_on(0));
        assert!(capslock_on(F_CAPSLOCK | F_LSHIFT)); // caps + shift held
        assert!(!capslock_on(F_LSHIFT)); // shift only
    }

    #[test]
    fn capslock_sync_taps_only_on_a_real_change_and_dedups() {
        let mut s = CapsLockSync::new(); // guest boots caps-off
        assert!(s.observe(true)); // host caps turned on -> tap (guest on)
        assert!(!s.observe(true)); // still on (e.g. a keystroke while on) -> no tap
        assert!(s.observe(false)); // turned off -> tap (guest off)
        assert!(!s.observe(false)); // still off -> no tap
    }

    #[test]
    fn capslock_sync_heals_drift_from_an_unfocused_toggle() {
        // The focus-desync: caps on while focused, then toggled OFF while the VM is unfocused —
        // the monitor sees nothing and macOS sends no reconciling event. The first event after
        // refocus carries led=false while we still believe on, so one heal tap re-syncs.
        let mut s = CapsLockSync::new();
        assert!(s.observe(true)); // caps on (guest on)
                                  // ... unfocused: host toggled caps OFF; the VM saw no event, so the belief stays on ...
        assert!(s.observe(false)); // first post-refocus event carries led=off -> heal tap
        assert!(!s.observe(false)); // now back in sync
    }

    #[test]
    fn modifier_emit_ignores_capslock() {
        // Caps Lock is handled by CapsLockSync, not the held-modifier path.
        assert_eq!(modifier_emit(KC_CAPSLOCK, F_CAPSLOCK, false), None);
        assert_eq!(modifier_emit(KC_CAPSLOCK, 0, true), None);
    }

    #[test]
    fn held_modifier_emits_one_edge_per_change_and_dedups() {
        // A real held modifier (left Shift) presses when its bit appears, releases when it
        // clears, and dedups against the believed state (a re-sent same-state flagsChanged
        // emits nothing).
        assert_eq!(
            modifier_emit(KC_LSHIFT, F_LSHIFT, false),
            Some(ModEmit::Edge(true))
        );
        assert_eq!(
            modifier_emit(KC_LSHIFT, F_LSHIFT, true),
            Some(ModEmit::None)
        );
        assert_eq!(
            modifier_emit(KC_LSHIFT, 0, true),
            Some(ModEmit::Edge(false))
        );
        assert_eq!(modifier_emit(KC_LSHIFT, 0, false), Some(ModEmit::None));
    }

    #[test]
    fn modifier_emit_ignores_untracked_keys() {
        // A letter key is not a modifier — the flagsChanged path must not emit for it.
        assert_eq!(modifier_emit(0x00 /* A */, 0, false), None);
    }
}
