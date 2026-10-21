// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 CPPC test: little vCPUs reach a stock guest on the ACPI machine as CPPC, and the guest builds
//! an energy model from them.
//!
//! Drives the real `limina` supervisor on the STOCK Fedora image (EFI/BLS boot on the default ACPI
//! machine, `--net` for SSH) with `--cpufreq --little-vcpus 2`. The chain under test: libkrun's
//! `_CPC` per processor device, the MADT efficiency classes and `\_SB._OSC` → the guest's stock
//! `cppc_cpufreq` → `cpu_capacity`, a cpufreq policy per vCPU and an energy-model perf domain,
//! which is what Energy Aware Scheduling needs. Zero guest-side limina components.
//!
//! RED without any link: no `_OSC` grant → arm64 rejects the SystemMemory registers and no
//! cpufreq driver binds; no efficiency classes → no energy model; equal `_CPC` levels → equal
//! capacities; a register wider than it says → UBSAN in `cpc_read`. Gated behind
//! LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn little_vcpus_reach_an_acpi_guest_as_cppc_with_an_energy_model() {
    if !limina_test::require_hvf_or_skip(
        "little_vcpus_reach_an_acpi_guest_as_cppc_with_an_energy_model",
    ) {
        return;
    }

    let mut cfg = GuestConfig::fedora_from_env()
        .expect("resolving guest config")
        .with_net()
        .with_supervisor_arg("--cpufreq")
        .with_supervisor_arg("--little-vcpus=2");
    cfg.cpus = 4;
    eprintln!("booting stock Fedora on ACPI with 2 of 4 vCPUs little");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(180))
        .expect("guest did not reach sshd");

    // cppc_cpufreq is a module; udev may load it after sshd.
    guest
        .ssh_poll(
            "test \"$(cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_driver | sort -u)\" = \
             cppc_cpufreq && echo ok",
            Duration::from_secs(60),
        )
        .expect("cppc_cpufreq never drove every vCPU");

    let run = |cmd: &str| {
        guest
            .ssh_exec(cmd)
            .unwrap_or_else(|e| panic!("running `{cmd}`: {e}"))
            .trim()
            .to_string()
    };

    let capacities: Vec<u32> = run("cat /sys/devices/system/cpu/cpu[0-3]/cpu_capacity")
        .split_whitespace()
        .map(|c| c.parse().expect("a numeric cpu_capacity"))
        .collect();
    assert_eq!(capacities.len(), 4, "capacities: {capacities:?}");
    assert_eq!(&capacities[..2], &[1024, 1024], "big vCPUs: {capacities:?}");
    assert!(
        capacities[2] == capacities[3] && capacities[2] < 1024,
        "the last two vCPUs must be little and alike: {capacities:?}"
    );

    // One policy per vCPU (no _PSD), and an energy-model perf domain for each.
    assert_eq!(
        run("ls -d /sys/devices/system/cpu/cpufreq/policy* | wc -l"),
        "4"
    );
    let dmesg = run("sudo dmesg");
    assert_eq!(
        dmesg.matches("EM: created perf domain").count(),
        4,
        "an energy model per vCPU, from the MADT efficiency classes"
    );
    assert!(
        !dmesg
            .lines()
            .any(|l| l.contains("UBSAN") && l.contains("cppc_acpi")),
        "a CPPC register read tripped UBSAN"
    );
}
