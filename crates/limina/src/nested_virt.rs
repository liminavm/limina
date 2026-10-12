// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Whether this Mac can give a guest EL2, so it can run KVM and VMs of its own.
//!
//! Hypervisor.framework answers that on M3 and later (macOS 15+), and the question needs no
//! hypervisor entitlement, so the supervisor and the control center ask it themselves rather
//! than starting a worker to find out. The worker asks libkrun the same thing before it creates
//! the VM and refuses `--nested-virt` where the answer is no.

use std::sync::OnceLock;

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_config_get_el2_supported(el2_supported: *mut bool) -> i32;
}

/// Whether this Mac's Hypervisor.framework offers a guest EL2.
pub fn host_supports() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut supported = false;
        // SAFETY: the out-pointer is a live bool for the duration of the call.
        let rc = unsafe { hv_vm_config_get_el2_supported(&mut supported) };
        rc == 0 && supported
    })
}
