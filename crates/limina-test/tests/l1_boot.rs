// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 fast boot test: our tiny direct-boot guest (kernel + virtio-fs rootfs + Rust init).
//!
//! Unlike the L2 Fedora test (which can only assert "reached the bootloader"), L1 boots
//! all the way to *our* userspace in well under a second and asserts a marker our init
//! prints, then a **clean guest-initiated power-off** (init calls `reboot(POWER_OFF)` →
//! PSCI → worker exits 0). This is the workhorse for the RED-first rule.
//!
//! Build the guest first: `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS;
//! run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The marker limina-init writes to /dev/kmsg. Keep in sync with `guest/limina-init`.
const MARKER: &str = "LIMINA_L1_USERSPACE_OK";

#[test]
fn l1_guest_boots_to_userspace_and_powers_off() {
    if !limina_test::require_hvf_or_skip("l1_guest_boots_to_userspace_and_powers_off") {
        return;
    }

    let cfg = GuestConfig::l1_from_env().expect("resolving L1 guest config");
    eprintln!("booting L1 guest via {:?}: {:?}", cfg.limina_bin, cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Reaches our userspace fast — the whole point of L1.
    guest
        .wait_for(MARKER, Duration::from_secs(15))
        .expect("L1 guest did not reach userspace");

    // The init then powers the VM off cleanly via PSCI; the worker should exit 0 and the
    // supervisor report a clean stop, with no force-kill from either layer.
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(
        !outcome.forced,
        "harness had to force teardown — guest power-off or supervisor is broken"
    );
    assert_eq!(
        outcome.code,
        Some(0),
        "expected a clean guest power-off (worker exit 0), got {outcome:?}"
    );
}
