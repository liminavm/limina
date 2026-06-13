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
//! Oracle: SSH reaches the guest, we read its boot id, trigger a reboot, then wait for SSH to
//! come back with a *different* boot id. That second SSH only succeeds if the supervisor stayed
//! alive and relaunched the worker (it owns the gvproxy NAT + the `127.0.0.1:2222` forward; had
//! it exited, the forward would be gone and SSH could never return). The changed boot id proves
//! it was a genuine fresh boot, not the same instance.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

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
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest did not reach sshd");

    let boot_id_1 = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading boot id")
        .trim()
        .to_string();
    assert!(!boot_id_1.is_empty(), "empty boot id");
    eprintln!("boot id before reboot: {boot_id_1}");

    // Schedule the reboot 1s out via a transient timer, so the SSH command returns cleanly (0)
    // before the connection drops, instead of being killed mid-command.
    guest
        .ssh_exec("sudo systemd-run --on-active=1 systemctl reboot")
        .expect("scheduling guest reboot");
    eprintln!("reboot scheduled; waiting for the VM to come back with a fresh boot id");

    // Wait for the guest to return with a *different* boot id. SSH will fail during the reboot
    // window (tolerated) and may briefly still reach the pre-reboot instance (same id, ignored).
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut boot_id_2 = None;
    while Instant::now() < deadline {
        if let Ok(out) = guest.ssh_exec("cat /proc/sys/kernel/random/boot_id") {
            let id = out.trim().to_string();
            if !id.is_empty() && id != boot_id_1 {
                boot_id_2 = Some(id);
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    let boot_id_2 = boot_id_2.expect(
        "guest never came back with a fresh boot after reboot — the supervisor did not relaunch \
         the worker (it likely exited, treating reboot as power-off)",
    );
    eprintln!("boot id after reboot:  {boot_id_2} (relaunch confirmed)");
    assert_ne!(
        boot_id_1, boot_id_2,
        "boot id did not change across the reboot"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
