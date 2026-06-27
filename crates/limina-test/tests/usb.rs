// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M7 USB passthrough — L1 guest-side prerequisite test.
//!
//! Boots the custom L1 kernel with `limina.usb_probe` and asserts the guest carries the
//! upstream USB/IP stack our kernel config (`build-test-kernel.sh` FRAG) turns on: the
//! `vhci_hcd` VIRTUAL host controller (the thing `usbip attach` binds a remote device to —
//! no real EHCI/XHCI needed), the usbip vhci driver, the usb bus, and uinput. This is the
//! guest half of the passthrough story; the host half (the `limina-usbip` server) is unit-tested
//! in that crate, and the real-device end-to-end is the hardware-gated follow-on.
//!
//! Requires the USB-enabled kernel: `scripts/build-test-kernel.sh` (the FRAG now carries the USB
//! symbols) + `scripts/build-test-guest.sh`. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn usb_ip_stack_is_present_in_the_guest_kernel() {
    if !limina_test::require_hvf_or_skip("usb_ip_stack_is_present_in_the_guest_kernel") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_cmdline_token("limina.usb_probe");
    eprintln!("booting L1 USB-probe guest: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("usb_probe: begin", Duration::from_secs(15))
        .expect("guest did not start the USB probe");

    // Each facility must report PRESENT. If the kernel lost a CONFIG_USB* symbol (e.g. a
    // defconfig/olddefconfig drift), the corresponding marker would read MISSING and the
    // matching wait_for would instead hit the "done" line first and time out — a clear RED.
    for facility in ["vhci_hcd", "usbip_vhci_driver", "usb_bus", "uinput"] {
        let marker = format!("RESULT: {facility} PRESENT");
        guest
            .wait_for(&marker, Duration::from_secs(10))
            .unwrap_or_else(|e| panic!("USB facility {facility} not present in guest: {e}"));
        eprintln!("  ✓ {facility}");
    }

    guest
        .wait_for("usb_probe: done", Duration::from_secs(5))
        .expect("USB probe did not finish");

    // The init powers off cleanly after the probe (PSCI → worker exit 0).
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(
        !outcome.forced,
        "harness had to force teardown: {outcome:?}"
    );
    assert_eq!(
        outcome.code,
        Some(0),
        "expected clean power-off: {outcome:?}"
    );
}

/// The crown-jewel end-to-end: a USB device actually ENUMERATES in the guest with NO physical
/// hardware. The host serves the hardware-free CDC-ACM mock over vsock; the guest imports it and
/// hands the fd to `vhci_hcd`, which runs URB traffic against the mock until `cdc-acm` binds it as
/// `/dev/ttyACM0`. Proves the whole USB/IP passthrough pipeline (protocol + transport + kernel).
#[test]
fn mock_cdc_acm_device_enumerates_in_guest_via_usbip() {
    if !limina_test::require_hvf_or_skip("mock_cdc_acm_device_enumerates_in_guest_via_usbip") {
        return;
    }

    const USBIP_VSOCK_PORT: u32 = 9320;
    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_usbip_vsock(USBIP_VSOCK_PORT);
    eprintln!("booting L1 USB-attach guest: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Accept the guest's USB/IP connection and serve the mock in the background. The guest
    // connects early (it retries), so accept right after boot.
    guest
        .accept_usbip_mock(Duration::from_secs(20))
        .expect("guest USB/IP client did not connect");

    guest
        .wait_for("usb_attach: attached", Duration::from_secs(15))
        .expect("guest did not complete the USB/IP import handshake");

    // The kernel enumerates the mock against the host server; cdc-acm binds it → /dev/ttyACM0.
    // (Verified live in the guest console: `usb 1-1: new full-speed USB device number 2 using
    // vhci_hcd` → `cdc_acm 1-1:1.0: ttyACM0: USB ACM device`.)
    guest
        .wait_for("RESULT: ttyACM0 PRESENT", Duration::from_secs(20))
        .expect("mock CDC-ACM device did not enumerate as /dev/ttyACM0 in the guest");

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    assert!(
        !outcome.forced,
        "harness had to force teardown: {outcome:?}"
    );
    assert_eq!(
        outcome.code,
        Some(0),
        "expected clean power-off: {outcome:?}"
    );
}
