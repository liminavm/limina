// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A guest **reboot** must relaunch the VM, not tear it down.
//!
//! libkrun is single-shot: a guest reboot (PSCI `SYSTEM_RESET`) exits the worker process just
//! like a power-off. Our libkrun patch gives reboot a distinct exit code and the supervisor
//! relaunches the worker on it, so the limina process — and the gvproxy + control plane it owns —
//! survives a guest reboot (real-hardware behavior). Without the fix the supervisor exited on
//! the first reboot and the guest never came back.
//!
//! Oracle: SSH reaches the guest, we read its boot id, trigger a reboot, then wait for the
//! supervisor to print a second launch identity block (a fresh `launch_id`, same supervisor pid,
//! a cold boot) and for SSH to come back with a *different* boot id. The block is the host's
//! evidence that this supervisor relaunched the worker; the second SSH only succeeds if it stayed
//! alive (it owns the gvproxy NAT and the forward); the changed boot id proves it was a genuine
//! fresh boot, not the same instance.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn guest_reboot_relaunches_the_worker() {
    if !limina_test::require_hvf_or_skip("guest_reboot_relaunches_the_worker") {
        return;
    }

    // Enhanced tier (fast 16k boot) to multi-user (skip gdm — quicker, stabler SSH), with NAT
    // so we can drive the guest. The harness COW-clones the disk; the clone persists across the
    // worker relaunch (same --disk arg), so the reboot is a real reboot of the same VM.
    let cfg = match GuestConfig::enhanced_fedora_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_cmdline_extra("systemd.unit=multi-user.target"),
        Err(e) => {
            eprintln!("SKIP guest_reboot_relaunches_the_worker: {e:#}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(180))
        .expect("guest did not reach sshd");

    // The host's evidence that the worker relaunched is a second identity block with a fresh
    // launch_id; the guest's is a new boot_id. `reboot_and_wait` requires both.
    let first = guest
        .wait_for_launch_kind(1, false, Duration::from_secs(30))
        .expect("the boot printed no cold-boot identity block");
    let rebooted = guest.reboot_and_wait(Duration::from_secs(180)).expect(
        "guest never came back with a fresh boot after reboot — the supervisor did not relaunch \
         the worker (it likely exited, treating reboot as power-off)",
    );
    eprintln!(
        "boot id {} -> {} (launch {} -> {})",
        rebooted.boot_id_before,
        rebooted.boot_id_after,
        first["launch_id"],
        rebooted.launch["launch_id"]
    );
    assert_ne!(
        rebooted.boot_id_before, rebooted.boot_id_after,
        "boot id did not change across the reboot"
    );
    assert_eq!(
        rebooted.launch.get("resumed").map(String::as_str),
        Some("no"),
        "a reboot relaunch must cold-boot: {:?}",
        rebooted.launch
    );
    assert_eq!(
        rebooted.launch.get("supervisor_pid"),
        first.get("supervisor_pid"),
        "the relaunch came from another supervisor"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
