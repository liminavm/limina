// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 stock-baseline boot test: the unmodified Fedora image must boot through limina.
//!
//! This drives the real `limina` supervisor → `limina-vmm` worker → libkrun/HVF chain, with
//! the disk opened **read-only** so the shared image is never mutated.
//!
//! What it asserts: the guest reaches its **bootloader** (EDK2 firmware banner + GRUB).
//! On a *pristine* Fedora image the kernel has no `console=` on its cmdline, so it goes
//! silent on serial after GRUB hands off — reaching GRUB proves the whole user-facing
//! chain works: limina boots the firmware, the firmware reads the virtio-blk disk, finds
//! the ESP, and launches the distro bootloader. Asserting on *userspace* without
//! mutating the image is what the L1 tiny-kernel guest (our own init prints a marker)
//! will add next — see docs/roadmap.md.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{assert_console_has, GuestConfig, Guest};

#[test]
fn fedora_stock_image_boots_to_bootloader() {
    if !limina_test::require_hvf_or_skip("fedora_stock_image_boots_to_bootloader") {
        return;
    }

    let cfg = GuestConfig::fedora_from_env().expect("resolving guest config");
    eprintln!(
        "booting {:?} (ro={}) via {:?}",
        cfg.disk, cfg.read_only, cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Firmware comes up first — this alone proves limina created the VM and the guest is
    // executing on HVF with our serial wired as the EDK2 console.
    guest
        .wait_for("UEFI firmware", Duration::from_secs(30))
        .expect("guest did not reach EFI firmware");

    // GRUB proves the firmware read the disk, found the ESP, and ran the bootloader.
    guest
        .wait_for("GRUB", Duration::from_secs(60))
        .expect("guest did not reach the GRUB bootloader");

    // Sanity-check both markers are present together in the final capture.
    assert_console_has(&guest.console(), &["UEFI firmware", "GRUB"])
        .expect("expected boot markers");

    // Clean teardown. Stock Fedora ignores the GPIO power button, so the supervisor will
    // force-kill the worker after its (short, test-configured) grace — that's expected
    // and still a clean stop. We just require the supervisor itself exits.
    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(
        !outcome.forced,
        "harness had to force the supervisor down — supervisor teardown is broken"
    );
}
