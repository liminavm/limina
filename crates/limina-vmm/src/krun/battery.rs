// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Host battery mirror for the guest's virtio-i2c SBS battery.
//!
//! The worker hands libkrun a callback ([`devices::virtio::BatteryProvider`])
//! that snapshots the Mac's battery via the IOKit power-sources API on every
//! guest register read — the guest's own UPower polling cadence drives
//! freshness, no timers or notification plumbing on our side.
//!
//! `LIMINA_BATTERY_FAKE="<percent>,<charging|discharging|ac|full>[,tte_min,ttf_min]"`
//! substitutes a fixed state, which is both the L2-test hook and a way to demo
//! the guest battery on a desktop Mac.

use std::ffi::{c_char, c_void, CStr};
use std::sync::Arc;

use devices::virtio::{BatteryProvider, BatteryState};

// Minimal CoreFoundation + IOKit power-sources FFI. The IOPS keys are plain C
// string literals (not exported constants), so we build the CFStrings ourselves.
#[allow(non_camel_case_types)]
type CFTypeRef = *const c_void;
#[allow(non_camel_case_types)]
type CFIndex = isize;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, idx: CFIndex) -> CFTypeRef;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFStringCreateWithCString(
        alloc: CFTypeRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFTypeRef;
    fn CFStringGetCStringPtr(string: CFTypeRef, encoding: u32) -> *const c_char;
    fn CFStringGetCString(
        string: CFTypeRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> bool;
    fn CFNumberGetValue(number: CFTypeRef, the_type: CFIndex, value_ptr: *mut c_void) -> bool;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> bool;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFTypeRef;
    fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: CFTypeRef) -> CFTypeRef;
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_NUMBER_SINT32_TYPE: CFIndex = 3;

/// An owned CFString made from a Rust literal.
struct CfString(CFTypeRef);
impl CfString {
    fn new(s: &str) -> Self {
        let c = std::ffi::CString::new(s).expect("no NUL in key literals");
        // SAFETY: valid NUL-terminated UTF-8 in, owned CFString out.
        Self(unsafe {
            CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        })
    }
}
impl Drop for CfString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own exactly one reference from CFStringCreateWithCString.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// `dict[key]` as i32, if present and numeric.
unsafe fn dict_i32(dict: CFTypeRef, key: &str) -> Option<i32> {
    let key = CfString::new(key);
    let value = CFDictionaryGetValue(dict, key.0);
    if value.is_null() {
        return None;
    }
    let mut out: i32 = 0;
    CFNumberGetValue(
        value,
        K_CF_NUMBER_SINT32_TYPE,
        &mut out as *mut i32 as *mut c_void,
    )
    .then_some(out)
}

/// `dict[key]` as bool, if present.
unsafe fn dict_bool(dict: CFTypeRef, key: &str) -> Option<bool> {
    let key = CfString::new(key);
    let value = CFDictionaryGetValue(dict, key.0);
    (!value.is_null()).then(|| CFBooleanGetValue(value))
}

/// `dict[key]` as an owned Rust string, if present.
unsafe fn dict_string(dict: CFTypeRef, key: &str) -> Option<String> {
    let key = CfString::new(key);
    let value = CFDictionaryGetValue(dict, key.0);
    if value.is_null() {
        return None;
    }
    let direct = CFStringGetCStringPtr(value, K_CF_STRING_ENCODING_UTF8);
    if !direct.is_null() {
        return Some(CStr::from_ptr(direct).to_string_lossy().into_owned());
    }
    let mut buf = [0 as c_char; 128];
    CFStringGetCString(
        value,
        buf.as_mut_ptr(),
        buf.len() as CFIndex,
        K_CF_STRING_ENCODING_UTF8,
    )
    .then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
}

/// Snapshot the first internal battery via IOKit. `None` = no battery (desktop).
fn read_host_battery() -> Option<BatteryState> {
    // SAFETY: straight use of the documented IOPS API; every Copy is released.
    unsafe {
        let blob = IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return None;
        }
        let list = IOPSCopyPowerSourcesList(blob);
        if list.is_null() {
            CFRelease(blob);
            return None;
        }

        let mut found = None;
        for i in 0..CFArrayGetCount(list) {
            let ps = CFArrayGetValueAtIndex(list, i);
            let desc = IOPSGetPowerSourceDescription(blob, ps);
            if desc.is_null() {
                continue;
            }
            if dict_string(desc, "Type").as_deref() != Some("InternalBattery") {
                continue;
            }
            let current = dict_i32(desc, "Current Capacity").unwrap_or(0);
            let max = dict_i32(desc, "Max Capacity").unwrap_or(100).max(1);
            let percent = (current * 100 / max).clamp(0, 100) as u8;
            let charging = dict_bool(desc, "Is Charging").unwrap_or(false);
            let ac_online = dict_string(desc, "Power Source State").as_deref() == Some("AC Power");
            // IOKit reports -1 (or omits the key) while the estimate settles.
            let minutes = |v: Option<i32>| match v {
                Some(m) if (1..=0xfffe).contains(&m) => Some(m as u16),
                _ => None,
            };
            found = Some(BatteryState {
                percent,
                charging,
                ac_online,
                time_to_empty_min: minutes(dict_i32(desc, "Time to Empty")),
                time_to_full_min: minutes(dict_i32(desc, "Time to Full Charge")),
                cycle_count: None,
            });
            break;
        }

        CFRelease(list);
        CFRelease(blob);
        found
    }
}

/// Parse `LIMINA_BATTERY_FAKE` ("85,charging" / "50,discharging,123" /
/// "100,full" / "80,ac").
fn parse_fake(spec: &str) -> Option<BatteryState> {
    let mut parts = spec.split(',').map(str::trim);
    let percent: u8 = parts.next()?.parse().ok()?;
    let state = parts.next()?;
    let (charging, ac_online) = match state {
        "charging" => (true, true),
        "discharging" => (false, false),
        "ac" | "full" => (false, true),
        _ => return None,
    };
    let tte = parts.next().and_then(|v| v.parse().ok());
    let ttf = parts.next().and_then(|v| v.parse().ok());
    Some(BatteryState {
        percent: percent.min(100),
        charging,
        ac_online,
        time_to_empty_min: tte,
        time_to_full_min: ttf,
        cycle_count: Some(42),
    })
}

/// The provider handed to libkrun, or `None` when the host has no battery (and
/// no fake is configured) — in which case the virtio-i2c device isn't attached
/// at all and the guest correctly shows no battery.
pub fn provider() -> Option<BatteryProvider> {
    if let Ok(spec) = std::env::var("LIMINA_BATTERY_FAKE") {
        let state = parse_fake(&spec)?;
        return Some(Arc::new(move || state));
    }
    // Probe once at startup to decide whether the device exists; afterwards the
    // provider re-reads on every guest poll. A transient IOKit failure mid-run
    // reads as an empty (0%, unplugged) battery for that poll — harmless and
    // self-correcting on the next one.
    read_host_battery()?;
    Some(Arc::new(|| read_host_battery().unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_spec_parses() {
        let s = parse_fake("85,charging,0,90").unwrap();
        assert_eq!(s.percent, 85);
        assert!(s.charging && s.ac_online);
        assert_eq!(s.time_to_full_min, Some(90));

        let s = parse_fake("50,discharging,123").unwrap();
        assert!(!s.charging && !s.ac_online);
        assert_eq!(s.time_to_empty_min, Some(123));

        let s = parse_fake("100,full").unwrap();
        assert!(!s.charging && s.ac_online);

        assert!(parse_fake("banana").is_none());
        assert!(parse_fake("50,levitating").is_none());
    }
}
