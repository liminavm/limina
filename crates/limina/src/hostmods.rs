// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What macOS itself does to the modifier row (System Settings ▸ Keyboard ▸ Modifier Keys).
//!
//! limina's normalization is *positional* — the key in the Option position becomes Super, the
//! one in the Command position becomes Alt — so it has to be applied to the physical key. But
//! macOS applies its own remapping down in the HID layer, **before any application sees the
//! event**: measured 2026-08-24 with `spikes/modifier-mapping/probe.swift` against a live
//! Control↔Command swap, pressing physical Control delivered `keyCode=0x37 kVK_Command` with
//! the Command flag set. The physical key is therefore not observable from an `NSEvent`; the
//! only way back to it is to read the configuration and invert it, which is what this does.
//!
//! The settings live in the **ByHost** global domain under one key per keyboard —
//! `com.apple.keyboard.modifiermapping.<vendor>-<product>-<n>` — holding an array of
//! `{HIDKeyboardModifierMappingSrc, HIDKeyboardModifierMappingDst}` dictionaries. The values
//! are HID usages with a `0x7_0000_0000` usage-page prefix. `NSUserDefaults` cannot see ByHost,
//! hence the raw `CFPreferences` calls.

use limina_input::keymap::HostModifierMap;

/// The usage-page prefix macOS stores alongside each usage (`kHIDPage_KeyboardOrKeypad`).
const USAGE_PAGE_PREFIX: i64 = 0x7_0000_0000;

/// The ByHost key prefix, one entry per keyboard that has ever been customised.
const KEY_PREFIX: &str = "com.apple.keyboard.modifiermapping.";

/// Reduce the per-keyboard configurations to the one map limina can act on.
///
/// **An `NSEvent` does not say which keyboard produced it**, so limina cannot apply a different
/// inversion per device. When the configured keyboards disagree there is no answer that is
/// right for both, and the safe choice is the identity: normalization then behaves exactly as
/// it did before this existed, rather than un-remapping the wrong keyboard's keys.
///
/// Pure, so the policy is testable without a Mac's preferences underneath it.
fn merge_devices(devices: &[(String, Vec<(u32, u32)>)]) -> HostModifierMap {
    let mut chosen: Option<(&str, HostModifierMap)> = None;
    for (device, pairs) in devices {
        let map = HostModifierMap::from_pairs(pairs.iter().copied());
        if map.is_identity() {
            continue;
        }
        match chosen {
            None => chosen = Some((device, map)),
            Some((first, existing)) if existing != map => {
                log::warn!(
                    "keyboard: {device} and {first} remap the modifier row differently, and an \
                     NSEvent does not say which keyboard it came from — leaving the row as macOS \
                     reports it. Modifier normalization will treat Command as Command."
                );
                return HostModifierMap::default();
            }
            Some(_) => {}
        }
    }
    chosen.map(|(_, m)| m).unwrap_or_default()
}

/// Read macOS's configuration. Identity when nothing is configured, which is the common case.
///
/// Read **once at startup**, deliberately: re-reading live would move the mapping under held
/// keys, and the guest would keep holding whatever the old map emitted. (The menu toggle has
/// the same hazard and solves it by releasing through the map that pressed.)
pub fn read() -> HostModifierMap {
    let devices = read_devices();
    if devices.is_empty() {
        return HostModifierMap::default();
    }
    let map = merge_devices(&devices);
    if !map.is_identity() {
        log::info!(
            "keyboard: macOS remaps the modifier row; normalization will read past it to the \
             physical key ({} keyboard(s) configured)",
            devices.len()
        );
    }
    map
}

#[cfg(not(target_os = "macos"))]
fn read_devices() -> Vec<(String, Vec<(u32, u32)>)> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn read_devices() -> Vec<(String, Vec<(u32, u32)>)> {
    use objc2_core_foundation::{
        kCFPreferencesAnyApplication, kCFPreferencesCurrentHost, kCFPreferencesCurrentUser,
        CFArray, CFDictionary, CFPreferencesCopyKeyList, CFPreferencesCopyValue, CFString,
    };

    // SAFETY: the three domain constants are static CFStrings owned by CoreFoundation, and the
    // call is documented to return a +1 reference (or NULL) which `CFRetained` then owns.
    let keys = unsafe {
        CFPreferencesCopyKeyList(
            kCFPreferencesAnyApplication,
            kCFPreferencesCurrentUser,
            kCFPreferencesCurrentHost,
        )
    };
    let Some(keys) = keys else {
        return Vec::new();
    };

    let mut devices = Vec::new();
    for i in 0..keys.count() {
        // The list is CFStrings; anything else in it is not ours to interpret.
        let Some(key) = (unsafe { borrow::<CFString>(keys.value_at_index(i)) }) else {
            continue;
        };
        let name = key.to_string();
        let Some(device) = name.strip_prefix(KEY_PREFIX) else {
            continue;
        };
        // SAFETY: same domain constants; `key` is a live CFString borrowed from the list.
        let value = unsafe {
            CFPreferencesCopyValue(
                key,
                kCFPreferencesAnyApplication,
                kCFPreferencesCurrentUser,
                kCFPreferencesCurrentHost,
            )
        };
        let Some(entries) = value.as_deref().and_then(|v| v.downcast_ref::<CFArray>()) else {
            continue;
        };
        let mut pairs = Vec::new();
        for j in 0..entries.count() {
            let Some(entry) = (unsafe { borrow::<CFDictionary>(entries.value_at_index(j)) }) else {
                continue;
            };
            let (Some(src), Some(dst)) = (
                usage(entry, "HIDKeyboardModifierMappingSrc"),
                usage(entry, "HIDKeyboardModifierMappingDst"),
            ) else {
                continue;
            };
            pairs.push((src, dst));
        }
        if !pairs.is_empty() {
            devices.push((device.to_string(), pairs));
        }
    }
    devices
}

/// Borrow a CF getter's non-owning `*const c_void` as `&T`, checking the dynamic type.
///
/// The Get-rule pointers CFArray/CFDictionary hand back are borrowed from their container, so
/// the lifetime is the caller's to keep honest — every call site here uses the result before
/// the container goes out of scope.
///
/// # Safety
/// `ptr` must be null or a valid CFType pointer that outlives `'a`.
#[cfg(target_os = "macos")]
unsafe fn borrow<'a, T: objc2_core_foundation::ConcreteType + objc2_core_foundation::Type>(
    ptr: *const std::ffi::c_void,
) -> Option<&'a T> {
    let ptr = std::ptr::NonNull::new(ptr.cast_mut())?;
    let any: &objc2_core_foundation::CFType = unsafe { ptr.cast().as_ref() };
    any.downcast_ref::<T>()
}

/// One `Src`/`Dst` field, page prefix stripped. `None` for anything unexpected — a missing
/// field, a non-number, or the `-1` macOS writes for "No Action" (a key that produces no event
/// at all, so there is nothing for us to invert).
#[cfg(target_os = "macos")]
fn usage(entry: &objc2_core_foundation::CFDictionary, field: &str) -> Option<u32> {
    use objc2_core_foundation::{CFNumber, CFRetained, CFString};
    let key = CFString::from_str(field);
    // SAFETY: `key` is a live CFString and the dictionary is keyed by CFStrings.
    let raw = unsafe { entry.value(CFRetained::as_ptr(&key).as_ptr().cast()) };
    let number = unsafe { borrow::<CFNumber>(raw) }?;
    let usage = number.as_i64()?.checked_sub(USAGE_PAGE_PREFIX)?;
    u32::try_from(usage).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use limina_input::keymap::{
        HID_LEFT_COMMAND, HID_LEFT_CONTROL, HID_LEFT_OPTION, HID_RIGHT_COMMAND, HID_RIGHT_CONTROL,
    };

    fn ctrl_cmd() -> Vec<(u32, u32)> {
        vec![
            (HID_LEFT_COMMAND, HID_LEFT_CONTROL),
            (HID_LEFT_CONTROL, HID_LEFT_COMMAND),
            (HID_RIGHT_COMMAND, HID_RIGHT_CONTROL),
            (HID_RIGHT_CONTROL, HID_RIGHT_COMMAND),
        ]
    }

    /// What THIS Mac has configured, for when a keyboard bug needs the ground truth.
    /// `cargo test -p limina --bins -- --ignored --nocapture dump_this_macs_modifier_map`
    #[test]
    #[ignore = "environment-dependent: reports this Mac's settings rather than asserting"]
    fn dump_this_macs_modifier_map() {
        let devices = read_devices();
        if devices.is_empty() {
            println!("no keyboard has a custom modifier mapping");
        }
        for (device, pairs) in &devices {
            println!("keyboard {device}:");
            for (src, dst) in pairs {
                println!("  usage {src:#04X} -> {dst:#04X}");
            }
        }
        let merged = merge_devices(&devices);
        println!("merged: {merged:?} (identity={})", merged.is_identity());
    }

    #[test]
    fn no_configured_keyboards_is_identity() {
        assert!(merge_devices(&[]).is_identity());
    }

    #[test]
    fn one_configured_keyboard_wins() {
        let map = merge_devices(&[("1452-834-0".into(), ctrl_cmd())]);
        assert_eq!(map, HostModifierMap::from_pairs(ctrl_cmd()));
    }

    #[test]
    fn keyboards_that_agree_are_not_a_conflict() {
        // The same physical customisation applied to two keyboards is one answer, not two.
        let map = merge_devices(&[
            ("1452-834-0".into(), ctrl_cmd()),
            ("1118-2100-0".into(), ctrl_cmd()),
        ]);
        assert_eq!(map, HostModifierMap::from_pairs(ctrl_cmd()));
    }

    #[test]
    fn an_untouched_keyboard_does_not_veto_a_configured_one() {
        // macOS writes identity entries for keyboards you opened the pane on and left alone;
        // those carry no opinion and must not read as disagreement.
        let identity = vec![(HID_LEFT_COMMAND, HID_LEFT_COMMAND)];
        let map = merge_devices(&[
            ("1452-834-0".into(), ctrl_cmd()),
            ("1118-2100-0".into(), identity),
        ]);
        assert_eq!(map, HostModifierMap::from_pairs(ctrl_cmd()));
    }

    #[test]
    fn genuinely_disagreeing_keyboards_fall_back_to_identity() {
        // No inversion is right for both, and an NSEvent will not say which one typed. Doing
        // nothing leaves normalization exactly as it behaved before any of this existed.
        let other = vec![(HID_LEFT_OPTION, HID_LEFT_COMMAND)];
        let map = merge_devices(&[
            ("1452-834-0".into(), ctrl_cmd()),
            ("1118-2100-0".into(), other),
        ]);
        assert!(map.is_identity());
    }
}
