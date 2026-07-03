// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 battery-mirror test: the host battery shows up in the guest as a native power_supply.
//!
//! Drives the real `limina` supervisor on the STOCK Fedora image (EFI/BLS boot, `--net` for
//! SSH) with `LIMINA_BATTERY_FAKE` pinning a known state, so the assertion is exact and the
//! test runs identically on battery-less hosts. The chain under test is the whole shipped
//! stack: the worker's battery provider → libkrun's virtio-i2c device + emulated SBS slave →
//! the DT child node → the guest's stock `i2c-virtio` + `sbs-battery` modules (autoloaded,
//! zero guest-side limina components) → `/sys/class/power_supply/sbs-0-000b`.
//!
//! RED without any link of that chain: no FDT node → the modules never load; no SBS register
//! file → sbs-battery's probe read of BatteryStatus fails; wrong register encoding → the
//! values below don't match. Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The sysfs node: SBS battery at the standard address 0x0b on the (sole) virtio-i2c bus.
const PSY: &str = "/sys/class/power_supply/sbs-0-000b";

#[test]
fn host_battery_mirrors_into_the_guest_as_a_power_supply() {
    if !limina_test::require_hvf_or_skip("host_battery_mirrors_into_the_guest_as_a_power_supply") {
        return;
    }

    // 73% discharging with 142 minutes left — distinctive values no real host would
    // coincidentally report through a broken path.
    let cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net()
        .with_env("LIMINA_BATTERY_FAKE", "73,discharging,142");
    eprintln!("booting stock Fedora (EFI/BLS) with a fake 73%/discharging host battery");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest did not reach sshd");

    // i2c-virtio + sbs-battery are modules; udev autoload may settle after sshd, so poll.
    guest
        .ssh_poll(
            &format!("test -d {PSY} && echo ok"),
            Duration::from_secs(60),
        )
        .expect(
            "the SBS battery power_supply never appeared — virtio-i2c/sbs-battery chain broken",
        );

    let read = |file: &str| {
        guest
            .ssh_exec(&format!("cat {PSY}/{file}"))
            .unwrap_or_else(|e| panic!("reading {PSY}/{file}: {e}"))
            .trim()
            .to_string()
    };

    // The exact fake state, end to end through the SBS register file.
    assert_eq!(read("capacity"), "73", "RelativeStateOfCharge (0x0d)");
    assert_eq!(
        read("status"),
        "Discharging",
        "BatteryStatus (0x16) + Current sign (0x0a)"
    );
    // 142 min = 8520 s (sbs-battery reports AverageTimeToEmpty in seconds).
    assert_eq!(
        read("time_to_empty_avg"),
        "8520",
        "AverageTimeToEmpty (0x12)"
    );
    // The string registers exercise the SMBus block-read fallback path (i2c-virtio has no
    // native block reads) — and are what UPower surfaces as vendor/model.
    assert_eq!(read("manufacturer"), "Limina", "ManufacturerName (0x20)");
    assert_eq!(read("model_name"), "Host Battery", "DeviceName (0x21)");
    assert_eq!(read("technology"), "Li-ion", "DeviceChemistry (0x22)");
    eprintln!("guest power_supply mirrors the fake host battery exactly ✓");

    let outcome = guest
        .shutdown(Duration::from_secs(60))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
