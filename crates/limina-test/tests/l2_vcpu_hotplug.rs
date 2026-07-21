// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guest vCPU hotplug must not wedge the VMM (task #40).
//!
//! A guest offlining a vCPU (`echo 0 > /sys/devices/system/cpu/cpuN/online`) is a normal Linux
//! admin operation. On unpatched libkrun it is DESTRUCTIVE: PSCI models CPU_ON but not CPU_OFF
//! (0x8400_0002) or AFFINITY_INFO (0xc400_0004) — both return NOT_SUPPORTED — so the dying vCPU
//! busy-spins (the de-risk probe saw the worker hit ~546% CPU) and the guest wedges; re-online is
//! also broken (the secondary boot channel is one-shot, consumed at boot). That violates the
//! two-tier "a stock guest must always stay usable" guarantee, and it is the mechanism half of the
//! deferred dynamic-vCPU-offlining feature (#35).
//!
//! This guards the fix: model CPU_OFF so the vCPU thread parks cleanly (zero host CPU), model
//! AFFINITY_INFO so the offline completes, and make CPU_ON re-deliverable at runtime. The guest
//! must stay reachable through an offline of two secondaries (nproc 4→2), and re-onlining must
//! bring them back (nproc 2→4).
//!
//! RED (pre-fix): the offline hangs / wedges the guest → the post-offline SSH assert fails.
//! GREEN (post-fix): offline + re-online round-trip cleanly.
//!
//! Uses the ≥7.1 injected-kernel enhanced path (`enhanced_share_from_env`, 4 vCPUs) — the kernel
//! carries CONFIG_HOTPLUG_CPU. SKIPs if the ≥7.1 kernel / disk is missing (build with
//! `KVER=v7.1 PAGESIZE=16k KIMAGE_NAME=Image-16k-71 PATCHES_OPTIONAL=1 scripts/build-test-kernel.sh`).
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// Cap each guest-side hotplug step so a wedged guest (the pre-fix behavior) fails the test
/// fast instead of blocking on the 900 s default SSH cap.
const STEP: Duration = Duration::from_secs(45);

#[test]
fn guest_vcpu_offline_online_does_not_wedge() {
    if !limina_test::require_hvf_or_skip("guest_vcpu_offline_online_does_not_wedge") {
        return;
    }

    // 4-vCPU ≥7.1 guest + NAT (no display — this is a scheduler/PSCI test, not venus).
    let cfg = match GuestConfig::enhanced_share_from_env() {
        Ok(cfg) => cfg.with_net(),
        Err(e) => {
            eprintln!("SKIPPED guest_vcpu_offline_online_does_not_wedge: {e}");
            return;
        }
    };
    assert_eq!(
        cfg.cpus, 4,
        "this test expects 4 vCPUs to offline two secondaries"
    );
    eprintln!("booting a 4-vCPU ≥7.1 guest to exercise runtime vCPU offline/online");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest sshd never became reachable through gvproxy");

    // Sanity: all four vCPUs are online at boot.
    let nproc0 = guest
        .ssh_exec_timeout("nproc", STEP)
        .expect("reading nproc at boot");
    assert_eq!(
        nproc0.trim(),
        "4",
        "expected 4 online vCPUs at boot, got {nproc0:?}"
    );

    // Offline two secondaries (keep cpu0, cpu1). On the pre-fix worker this hangs / wedges the
    // guest — the dying vCPUs spin on the unmodeled CPU_OFF and the reaper polls AFFINITY_INFO
    // forever — so this very command (or the nproc read after it) fails, turning the guard RED.
    for cpu in [2u32, 3] {
        guest
            .ssh_exec_timeout(
                &format!("echo 0 | sudo tee /sys/devices/system/cpu/cpu{cpu}/online >/dev/null"),
                STEP,
            )
            .unwrap_or_else(|e| {
                panic!(
                    "offlining cpu{cpu} did not complete — on the pre-fix worker CPU_OFF is \
                     unmodeled, so the offline hangs and the guest wedges: {e}"
                )
            });
    }

    // The guest must still be alive and report exactly the two remaining vCPUs.
    let nproc_off = guest
        .ssh_exec_timeout("nproc", STEP)
        .expect("guest unreachable after offlining vCPUs (the pre-fix wedge)");
    assert_eq!(
        nproc_off.trim(),
        "2",
        "expected 2 online vCPUs after offlining cpu2+cpu3, got {nproc_off:?}"
    );
    let online = guest
        .ssh_exec_timeout("cat /sys/devices/system/cpu/online", STEP)
        .expect("reading the online cpu mask");
    assert_eq!(
        online.trim(),
        "0-1",
        "unexpected online cpu mask: {online:?}"
    );

    // Re-online both. This exercises the runtime CPU_ON path (the one-shot boot channel had to
    // become durable); on the pre-fix worker there is no path back.
    for cpu in [2u32, 3] {
        guest
            .ssh_exec_timeout(
                &format!("echo 1 | sudo tee /sys/devices/system/cpu/cpu{cpu}/online >/dev/null"),
                STEP,
            )
            .unwrap_or_else(|e| panic!("re-onlining cpu{cpu} failed (runtime CPU_ON): {e}"));
    }

    let nproc_on = guest
        .ssh_exec_timeout("nproc", STEP)
        .expect("reading nproc after re-online");
    assert_eq!(
        nproc_on.trim(),
        "4",
        "re-onlined vCPUs did not come back (expected nproc 4), got {nproc_on:?}"
    );

    // The re-onlined vCPUs must actually run work, not just appear in the mask: pin a short busy
    // loop to cpu3 and require it to complete (a vCPU that resumed at a bad PC would never finish).
    guest
        .ssh_exec_timeout(
            "taskset -c 3 sh -c 'i=0; while [ $i -lt 2000000 ]; do i=$((i+1)); done; echo RAN'",
            STEP,
        )
        .map(|out| {
            assert!(
                out.contains("RAN"),
                "cpu3 did not run work after re-online: {out:?}"
            )
        })
        .expect("running a task on the re-onlined cpu3");

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
