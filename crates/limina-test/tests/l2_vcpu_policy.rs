// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The dynamic vCPU policy end to end: host asks, guest acts (task #35, policy half).
//!
//! `l2_vcpu_hotplug.rs` guards the MECHANISM — that a guest offlining a vCPU by hand does not
//! wedge the VMM. This guards the LOOP built on top of it: the guest agent samples its runnable
//! load and reports it, the supervisor's `VcpuPolicy` decides a target, the agent writes sysfs,
//! and the guest keeps working throughout. Every piece has unit tests; what only a real guest can
//! show is that they compose — that a real Fedora reports a load the policy reads as calm, that
//! the sysfs write lands on an SELinux-enforcing guest, and that a re-onlined vCPU takes work.
//!
//! **Tier-agnostic on purpose.** The loop has two guest-side implementations and this exercises
//! whichever the image has: `limina-agent` >= 0.5.0 pushes `CpuPressure` and writes sysfs
//! (enhanced tier), while a guest with only the stock `qemu-guest-agent` is polled through
//! `guest-get-load` and actuated with `guest-set-vcpus` (stock tier — see `crate::qga::vcpu`).
//! Against the suite's default stock image this covers the QGA path; point `LIMINA_TEST_DISK` at
//! an enhanced image to cover the agent path. The test reports which tier it saw, because a
//! failure means very different things in each.
//!
//! Measured 2026-09-02 on F44 + qemu-guest-agent 10.2.2, SELinux **Enforcing**: the QGA path
//! works — `virt_qemu_ga_t` does not block the sysfs write. Do not "simplify" this to assume it
//! is denied; it was measured, not reasoned.
//!
//! Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

/// Cap each guest-side step so a wedged guest fails fast rather than blocking on the 900 s
/// default SSH cap.
const STEP: Duration = Duration::from_secs(45);

/// The dwell handed to the supervisor. The production default is 20 s per step, which — with a
/// fresh guest's boot loadavg still decaying underneath it — would make this test many minutes
/// long for no extra coverage. What is under test is the loop, not the constant.
const DWELL_SECS: u64 = 5;

/// Poll `nproc` until `want(n)` holds, or give up. Returns the last value seen either way, so a
/// failure can say what the guest actually settled on rather than just "timed out".
fn wait_for_nproc(guest: &Guest, limit: Duration, want: impl Fn(u32) -> bool) -> (bool, u32) {
    let deadline = Instant::now() + limit;
    let mut last = 0;
    while Instant::now() < deadline {
        if let Ok(out) = guest.ssh_exec_timeout("nproc", STEP) {
            if let Ok(n) = out.trim().parse::<u32>() {
                last = n;
                if want(n) {
                    return (true, n);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    (false, last)
}

/// A 4-vCPU guest with `--cpu-reclaim aggressive` must shed vCPUs while it idles and get them all
/// back once it has work — the two halves of the policy, and the asymmetry between them.
///
/// RED before the policy exists (or against an image whose agent predates the `vcpu` capability):
/// nproc never leaves 4.
#[test]
fn an_idle_guest_sheds_vcpus_and_a_busy_one_gets_them_back() {
    if !limina_test::require_hvf_or_skip("an_idle_guest_sheds_vcpus_and_a_busy_one_gets_them_back")
    {
        return;
    }

    let cfg = match GuestConfig::enhanced_share_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_supervisor_arg("--cpu-reclaim")
            .with_supervisor_arg("aggressive")
            .with_env("LIMINA_VCPU_DWELL_SECS", &DWELL_SECS.to_string()),
        Err(e) => {
            eprintln!("SKIPPED an_idle_guest_sheds_vcpus_and_a_busy_one_gets_them_back: {e}");
            return;
        }
    };
    assert_eq!(cfg.cpus, 4, "this test expects a 4-vCPU guest");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest sshd never became reachable through gvproxy");

    // Which tier is actually under test? A failure below reads completely differently depending
    // on the answer, so establish it before asserting anything.
    let tier = guest
        .ssh_exec_timeout(
            "systemctl is-active limina-agent 2>/dev/null | grep -qx active && echo enhanced \
             || (systemctl is-active qemu-guest-agent 2>/dev/null | grep -qx active && echo qga \
             || echo none)",
            STEP,
        )
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    eprintln!("guest-side tier driving the loop: {tier}");
    assert_ne!(
        tier, "none",
        "neither limina-agent nor qemu-guest-agent is running in this guest, so nothing can act \
         on a vCPU target — this image cannot exercise the policy at all"
    );

    let boot_nproc = guest
        .ssh_exec_timeout("nproc", STEP)
        .expect("reading nproc at boot");
    assert_eq!(
        boot_nproc.trim(),
        "4",
        "the VM must BOOT with every vCPU online — the policy only ever takes them away later"
    );

    // Shrink. The guest is idle after login, but its boot loadavg decays over about a minute, and
    // the shrink gate wants the 1-minute load to fit the smaller set with a CPU to spare — so the
    // first step waits on loadavg, not on the dwell. Assert only that it gives up at least two
    // vCPUs: reaching the aggressive floor of 1 additionally needs load1 to read exactly 0.00,
    // which is a much longer and much less interesting wait.
    let (shrank, n) = wait_for_nproc(&guest, Duration::from_secs(240), |n| n <= 2);
    assert!(
        shrank,
        "an idle guest never shed vCPUs (nproc stuck at {n}, tier {tier}). On the `enhanced` tier \
         suspect a STALE IMAGE — the `vcpu` capability needs limina-agent >= 0.5.0, so check \
         `limina-agent --version` in the guest and redeliver the payload if it is older. On the \
         `qga` tier suspect the guest agent's SELinux domain refusing guest-set-vcpus, and grep \
         the worker log for `guest-set-vcpus failed`."
    );
    eprintln!("idle guest shed vCPUs: nproc 4 -> {n}");

    let online = guest
        .ssh_exec_timeout("cat /sys/devices/system/cpu/online", STEP)
        .expect("reading the online cpu mask");
    assert!(
        online.trim().starts_with('0'),
        "cpu0 must never be the one offlined; online mask is {online:?}"
    );

    // Grow. Four spinners are both more runnable tasks than online CPUs and enough burned CPU to
    // corroborate that — spinners rather than a synthetic wake storm on purpose, since an
    // uncorroborated spike is exactly what the policy now refuses to act on. No dwell on this
    // side, so it is a question of one report interval plus the sysfs writes.
    guest
        .ssh_exec_timeout(
            "rm -f /tmp/spin.pids; for i in 1 2 3 4; do nohup timeout 240 sh -c \
             'while :; do :; done' >/dev/null 2>&1 & echo $! >> /tmp/spin.pids; done; echo SPAWNED",
            STEP,
        )
        .expect("spawning the load");
    // Budget for the SLOW tier. The enhanced agent reports the spike and the utilisation behind
    // it, so it grows on the very
    // next sample (~1s); the stock tier has only loadavg, a 1-minute average, so it cannot notice
    // a burst until the average climbs — measured at ~45 s to cross a 2-vCPU machine's threshold.
    // That gap is the honest cost of the stock tier, not a flake to paper over.
    let (grew, n) = wait_for_nproc(&guest, Duration::from_secs(150), |n| n == 4);
    assert!(
        grew,
        "a busy guest did not get every vCPU back (nproc {n}, tier {tier})"
    );
    eprintln!("busy guest recovered every vCPU: nproc -> {n}");

    // A re-onlined vCPU must actually RUN work, not merely appear in the mask.
    //
    // Kill the load by recorded pid, NOT with `pkill -f <the spin command>`: that pattern also
    // matches the cmdline of the ssh invocation carrying it, so pkill kills its own shell and ssh
    // comes back 255. (Measured — it failed exactly that way first time.)
    guest
        .ssh_exec_timeout(
            "kill $(cat /tmp/spin.pids) 2>/dev/null; rm -f /tmp/spin.pids; echo STOPPED",
            STEP,
        )
        .expect("stopping the load");
    let out = guest
        .ssh_exec_timeout(
            "taskset -c 3 sh -c 'i=0; while [ $i -lt 2000000 ]; do i=$((i+1)); done; echo RAN'",
            STEP,
        )
        .expect("running a task on the re-onlined cpu3");
    assert!(
        out.contains("RAN"),
        "cpu3 did not run work after the policy re-onlined it: {out:?}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}

/// A snapshot must never capture a shrunk machine (task #41).
///
/// The guest-visible online set lives in libkrun's `VcpuList` and is NOT in the M9 snapshot
/// format, so a snapshot taken while a vCPU is offline restores it as online and the guest
/// kernel's bookkeeping diverges from the host's. #41 stays deferred rather than changing the
/// snapshot format; instead the supervisor re-onlines everything inside the suspend bracket. That
/// mitigation lives in the supervisor, so this drives suspend the way a user does — SIGTSTP to
/// the supervisor — and NOT `suspend_bracket()`, which signals the worker and would skip it.
///
/// RED without the mitigation: the restored guest comes up with fewer CPUs than it booted with.
#[test]
fn a_snapshot_taken_while_shrunk_restores_with_every_vcpu() {
    if !limina_test::require_hvf_or_skip("a_snapshot_taken_while_shrunk_restores_with_every_vcpu") {
        return;
    }

    let base = match GuestConfig::enhanced_share_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_supervisor_arg("--cpu-reclaim")
            .with_supervisor_arg("aggressive")
            .with_env("LIMINA_VCPU_DWELL_SECS", &DWELL_SECS.to_string()),
        Err(e) => {
            eprintln!("SKIPPED a_snapshot_taken_while_shrunk_restores_with_every_vcpu: {e}");
            return;
        }
    };

    let mut g1 = Guest::boot(&base.clone().with_snapshot()).expect("spawning the supervisor");
    g1.wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest sshd never became reachable");

    // Get it shrunk first, or the test proves nothing about the mitigation.
    let (shrank, n) = wait_for_nproc(&g1, Duration::from_secs(240), |n| n <= 2);
    assert!(
        shrank,
        "the guest never shed a vCPU (nproc {n}), so a snapshot here would not exercise #41 at \
         all — check the agent version in the image before reading this as a restore bug"
    );
    eprintln!("shrunk to nproc {n}; suspending through the supervisor");

    // The production suspend path. The supervisor should re-online everything BEFORE it relays
    // SIGTSTP to the worker, so the snapshot below holds a 4-vCPU machine.
    g1.suspend_via_supervisor()
        .expect("sending the suspend bracket to the supervisor");
    g1.wait_supervisor_exit(Duration::from_secs(180))
        .expect("the supervisor never finished the suspend bracket");

    let snap = g1
        .snapshot_path()
        .expect("snapshot path configured")
        .with_extension("kept");
    std::fs::copy(g1.snapshot_path().expect("snapshot path configured"), &snap)
        .expect("keeping the snapshot for the restore boot");
    drop(g1);

    let mut g2 = Guest::boot(&base.restore_from(&snap)).expect("spawning the restore supervisor");
    g2.wait_for_ssh_banner(Duration::from_secs(180))
        .expect("the restored guest never became reachable");

    // Immediately after restore the guest must have every vCPU. Read it before the policy has had
    // time to start shrinking again (dwell is DWELL_SECS, so do not dawdle) — and read the
    // GUEST's own count, since it is the side whose bookkeeping #41 is about.
    let nproc = g2
        .ssh_exec_timeout("nproc", STEP)
        .expect("reading nproc after restore");
    assert_eq!(
        nproc.trim(),
        "4",
        "the restored guest came up with {nproc:?} vCPUs, not 4 — the suspend bracket is supposed \
         to bring every vCPU back BEFORE the snapshot so the restore cannot diverge (#41)"
    );

    let outcome = g2
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the restored guest");
    eprintln!("teardown outcome: {outcome:?}");
}
