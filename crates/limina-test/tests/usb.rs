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

// ---- Emulated xHCI controller (M7∩M14, docs/design/usb-xhci.md) ------------------------
//
// These boot the L1 kernel with `--usb`, so libkrun's own `generic-xhci` controller comes up
// and the guest's stock `xhci-plat` driver binds it — no USB/IP, no real hardware. The mock
// gadget is selected with `LIMINA_USB_MOCK` (a test hook, inherited by the worker). Requires
// the USB-enabled test kernel (`CONFIG_USB_XHCI_HCD/PLATFORM`, and `CONFIG_HIDRAW` for the
// echo test) — `scripts/build-test-kernel.sh`.

/// Bare bring-up: `--usb` with no gadget → the guest sees the xHCI HCD and root hub, no errors.
#[test]
fn l1_xhci_enumerates() {
    if !limina_test::require_hvf_or_skip("l1_xhci_enumerates") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_supervisor_arg("--usb")
        .with_cmdline_token("limina.xhci_probe");
    eprintln!("booting L1 xHCI-probe guest: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("xhci_probe: begin", Duration::from_secs(15))
        .expect("guest did not start the xHCI probe");
    // The controller's own xhci-plat driver bound the generic-xhci node (the root hub is up).
    guest
        .wait_for("RESULT: xhci_hcd PRESENT", Duration::from_secs(10))
        .expect("xhci-hcd driver did not register in the guest");
    guest
        .wait_for("RESULT: xhci_bound PRESENT", Duration::from_secs(10))
        .expect("the generic-xhci controller did not bind (no root hub)");
    guest
        .wait_for("xhci_probe: done", Duration::from_secs(5))
        .expect("xHCI probe did not finish");

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

/// Stock-tier fingerprint reader over USB (M14 wave 3): `--fingerprint` cold-plugs the
/// impersonated elanmoc gadget, and the guest enumerates it with the byte-exact Elan identity
/// `04f3:0c7d`. This guards the descriptor set + cold-plug end-to-end; enumeration answers from the
/// gadget's descriptors alone (no supervisor protocol / Touch ID needed), so it is deterministic in
/// CI. `LIMINA_FP_TEST_APPROVE=1` makes the supervisor advertise the `fingerprint` capability on a
/// host without a usable Touch ID sensor, so the gadget always attaches. The full fprintd
/// enroll/verify flow (which the reused generic probe can't drive — it needs stock userspace) is
/// covered by the engine's unit tests + a golden pcapng replay and by one-time live validation
/// (docs/fingerprint-reader.md); the worker↔supervisor bulk-transport seam under a real guest
/// driver has no automated guard until the planned L2 (stock image + fprintd + test-approve).
#[test]
fn l1_xhci_fingerprint_reader() {
    if !limina_test::require_hvf_or_skip("l1_xhci_fingerprint_reader") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_supervisor_arg("--fingerprint")
        .with_env("LIMINA_FP_TEST_APPROVE", "1")
        .with_cmdline_token("limina.xhci_probe");
    eprintln!("booting L1 xHCI fingerprint-reader guest: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("xhci_probe: begin", Duration::from_secs(15))
        .expect("guest did not start the xHCI probe");
    // The impersonated elanmoc reader is 0x04f3:0x0c7d (see crates/limina-vmm/src/moc_usb.rs).
    guest
        .wait_for("RESULT: usbdev 04f3:0c7d", Duration::from_secs(20))
        .expect("the cold-plugged fingerprint gadget did not enumerate as 04f3:0c7d");
    guest
        .wait_for("xhci_probe: done", Duration::from_secs(5))
        .expect("xHCI probe did not finish");

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

/// A cold-plugged enumeration-only mock enumerates with its distinctive VID/PID.
#[test]
fn l1_xhci_mock_device() {
    if !limina_test::require_hvf_or_skip("l1_xhci_mock_device") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_supervisor_arg("--usb")
        .with_env("LIMINA_USB_MOCK", "enum")
        .with_cmdline_token("limina.xhci_probe");
    eprintln!("booting L1 xHCI mock-device guest: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("xhci_probe: begin", Duration::from_secs(15))
        .expect("guest did not start the xHCI probe");
    // The enumeration mock is 0x1d6b:0x0f10 (see devices/src/usb/mock.rs).
    guest
        .wait_for("RESULT: usbdev 1d6b:0f10", Duration::from_secs(15))
        .expect("the cold-plugged mock did not enumerate with the expected VID/PID");
    guest
        .wait_for("xhci_probe: done", Duration::from_secs(5))
        .expect("xHCI probe did not finish");

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

/// The crown-jewel B2 oracle: the HID echo gadget cold-plugged, usbhid binds it, a hidraw node
/// appears, and a 64-byte report written to `/dev/hidrawN` comes back byte-identical — proving
/// held interrupt-IN + deferred completion (fired from the OUT path) + OUT delivery end-to-end.
#[test]
fn l1_xhci_hid_echo() {
    if !limina_test::require_hvf_or_skip("l1_xhci_hid_echo") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_supervisor_arg("--usb")
        .with_env("LIMINA_USB_MOCK", "hid")
        .with_cmdline_token("limina.hid_echo");
    eprintln!("booting L1 xHCI HID-echo guest: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("hid_echo: begin", Duration::from_secs(15))
        .expect("guest did not start the HID echo probe");
    // usbhid binds the gadget and creates a hidraw node (VID 1D6B, PID 0F11).
    guest
        .wait_for("hid_echo: using /dev/hidraw", Duration::from_secs(20))
        .expect("no hidraw node appeared for the HID gadget");
    guest
        .wait_for("RESULT: hid_echo OK", Duration::from_secs(20))
        .expect("the 64-byte report did not echo back through the emulated controller");

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

/// Stock-tier FIDO over USB (M14 Stage C): `--usb` with a usable passkey store cold-plugs the
/// real FIDO authenticator gadget (no `LIMINA_USB_MOCK`, no guest agent). The guest's stock
/// usbhid binds it as a `/dev/hidrawN` with usage page 0xF1D0, and a raw CTAPHID probe drives
/// INIT + authenticatorGetInfo through it — both presence-free, so no Touch ID is needed. This
/// is the USB twin of the shipped uhid/agent path. `LIMINA_FIDO_TEST_APPROVE=1` makes the
/// supervisor advertise the `fido` capability even on a host without a usable Secure Enclave,
/// so the gadget attaches deterministically in CI; getInfo touches no enclave key.
#[test]
fn l1_xhci_fido_authenticator() {
    if !limina_test::require_hvf_or_skip("l1_xhci_fido_authenticator") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_supervisor_arg("--usb")
        .with_env("LIMINA_FIDO_TEST_APPROVE", "1")
        .with_cmdline_token("limina.fido_probe");
    eprintln!("booting L1 xHCI FIDO-authenticator guest: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for("fido_probe: begin", Duration::from_secs(15))
        .expect("guest did not start the FIDO probe");
    // usbhid binds the FIDO gadget; a hidraw node with usage page 0xF1D0 appears (fido-id-style).
    guest
        .wait_for("RESULT: fido_hidraw PRESENT", Duration::from_secs(20))
        .expect("no FIDO hidraw node (usage page 0xF1D0) appeared for the gadget");
    // CTAPHID INIT round-trips (nonce echo + a channel allocated) through the whole proxy path.
    guest
        .wait_for("RESULT: fido_init OK", Duration::from_secs(20))
        .expect("CTAPHID INIT did not complete through the FIDO gadget");
    // authenticatorGetInfo returns well-formed CBOR advertising FIDO_2_0.
    guest
        .wait_for("RESULT: fido_getinfo OK", Duration::from_secs(20))
        .expect("authenticatorGetInfo did not return FIDO_2_0 through the FIDO gadget");

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
