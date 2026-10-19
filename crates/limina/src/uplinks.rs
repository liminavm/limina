// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host interfaces a bridged NIC can join (`docs/design/multi-vm-networking.md` §3.3): the
//! ones vmnet itself offers (`vmnet_copy_shared_interface_list`, which needs no entitlement), each
//! named the way System Settings names it.

use std::ffi::{CStr, CString, c_char, c_void};

/// A host interface a NIC can be bridged onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uplink {
    /// The BSD name vm.toml stores and vmnet takes (`en0`).
    pub ifname: String,
    /// What System Settings calls it (`Wi-Fi`), when it says.
    pub kind: Option<String>,
}

impl Uplink {
    /// `Wi-Fi (en0)`, or the bare name for an interface the system does not describe.
    pub fn label(&self) -> String {
        match &self.kind {
            Some(kind) => format!("{kind} ({})", self.ifname),
            None => self.ifname.clone(),
        }
    }
}

type XpcObject = *mut c_void;
type CfRef = *const c_void;

#[link(name = "vmnet", kind = "framework")]
unsafe extern "C" {
    fn vmnet_copy_shared_interface_list() -> XpcObject;
}

unsafe extern "C" {
    fn xpc_array_get_count(array: XpcObject) -> usize;
    fn xpc_array_get_string(array: XpcObject, index: usize) -> *const c_char;
    fn xpc_release(object: XpcObject);
}

#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCNetworkInterfaceCopyAll() -> CfRef;
    fn SCNetworkInterfaceGetBSDName(interface: CfRef) -> CfRef;
    fn SCNetworkInterfaceGetLocalizedDisplayName(interface: CfRef) -> CfRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    fn CFStringGetCString(s: CfRef, buf: *mut c_char, len: isize, encoding: u32) -> bool;
    fn CFRelease(cf: CfRef);
}

const UTF8: u32 = 0x0800_0100;

fn cf_string(s: CfRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 256];
    // SAFETY: `s` is a live CFString and the buffer's length is passed with it.
    let ok = unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, UTF8) };
    // SAFETY: CFStringGetCString wrote a NUL-terminated string on success.
    ok.then(|| {
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    })
}

/// What System Settings calls each interface, by BSD name.
fn display_names() -> Vec<(String, String)> {
    let mut names = Vec::new();
    // SAFETY: plain CF calls on the array copied here, released at the end; the elements are
    // borrowed from it.
    unsafe {
        let all = SCNetworkInterfaceCopyAll();
        if all.is_null() {
            return names;
        }
        for i in 0..CFArrayGetCount(all) {
            let interface = CFArrayGetValueAtIndex(all, i);
            if let (Some(bsd), Some(shown)) = (
                cf_string(SCNetworkInterfaceGetBSDName(interface)),
                cf_string(SCNetworkInterfaceGetLocalizedDisplayName(interface)),
            ) {
                names.push((bsd, shown));
            }
        }
        CFRelease(all);
    }
    names
}

/// The interfaces vmnet would bridge onto now, in its order.
pub fn bridgeable() -> Vec<Uplink> {
    let mut ifnames = Vec::new();
    // SAFETY: the array is copied for us and released here; its strings are read before that.
    unsafe {
        let list = vmnet_copy_shared_interface_list();
        if list.is_null() {
            return Vec::new();
        }
        for i in 0..xpc_array_get_count(list) {
            let name = xpc_array_get_string(list, i);
            if !name.is_null() {
                ifnames.push(CStr::from_ptr(name).to_string_lossy().into_owned());
            }
        }
        xpc_release(list);
    }
    let names = display_names();
    ifnames
        .into_iter()
        .map(|ifname| Uplink {
            kind: names
                .iter()
                .find(|(bsd, _)| *bsd == ifname)
                .map(|(_, shown)| shown.clone()),
            ifname,
        })
        .collect()
}

/// Whether the host has an interface by this name at all, bridgeable or not. A stat-cheap check:
/// vmnet does not refuse a missing interface, its start just never answers.
pub fn exists(ifname: &str) -> bool {
    CString::new(ifname)
        // SAFETY: a NUL-terminated name, read and not kept.
        .map(|c| unsafe { libc::if_nametoindex(c.as_ptr()) } != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uplink_is_labelled_the_way_system_settings_names_it() {
        let wifi = Uplink {
            ifname: "en0".into(),
            kind: Some("Wi-Fi".into()),
        };
        assert_eq!(wifi.label(), "Wi-Fi (en0)");
        let bare = Uplink {
            ifname: "en7".into(),
            kind: None,
        };
        assert_eq!(bare.label(), "en7");
    }

    #[test]
    fn every_bridgeable_interface_exists_and_a_made_up_one_does_not() {
        for u in bridgeable() {
            assert!(exists(&u.ifname), "{u:?}");
        }
        assert!(exists("lo0"));
        assert!(!exists("en99"));
        assert!(!exists("bad\0name"));
    }
}
