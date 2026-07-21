// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! In-place s2idle: the guest suspends and wakes **without the worker ever dying** —
//! no snapshot, no restore, the same VMM process throughout. This is the substrate for
//! host-sleep integration (`docs/design/host-sleep-s2idle.md`): on host `willSleep` the
//! worker will pulse the sleep button, and on `didWake` the wake key; both legs must
//! already work for a stock guest with zero limina components.
//!
//! What this pins:
//! - the s2idle round-trip itself on the same worker (libkrun 0072 sticky queue re-arm:
//!   the thaw's bus-fallback re-negotiation lands on re-armed rings, same boot_id, SSH
//!   returns through the same gvproxy);
//! - the **stock wallclock benefit that motivates host-sleep s2idle**: the kernel's thaw
//!   re-reads the (libkrun 0088 host-anchored) RTC and injects the slept duration, so the
//!   guest comes back with a correct CLOCK_REALTIME after a real gap — chronyd stopped,
//!   no guest agent (same guard shape as `managed_vm_suspends_and_resumes`, but with no
//!   snapshot/restore in the loop).
//!
//! The wake uses the worker's `SIGWINCH` test seam (`limina-vmm/src/wake.rs`) — the same
//! `KEY_WAKEUP` GPIO pulse the host-sleep bracket will use, minus IOKit.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// Real wallclock gap held while the guest sleeps: if the thaw failed to advance
/// CLOCK_REALTIME the guest would come back ~this far behind — far outside the
/// ±5 s tolerance.
const SLEEP_GAP: Duration = Duration::from_secs(15);

/// How long past SSH-back the seated test insists the shell stays alive: the pre-fix
/// failure is DELAYED (~17 s in mesa's vn_relax dead-ring abort after the wipe), so a
/// same-pid reading right after wake proves nothing yet (same window as the snapshot
/// gate test).
const ABORT_WINDOW: Duration = Duration::from_secs(35);

/// `ssh_exec` with a few retries: a loaded host can drop a single connection right
/// after the banner poll succeeded (same helper as `venus_session_preserved`).
fn ssh_retry(guest: &Guest, cmd: &str) -> String {
    let mut last_err = String::new();
    for _ in 0..4 {
        match guest.ssh_exec(cmd) {
            Ok(out) => return out.trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("ssh `{cmd}` kept failing: {last_err}");
}

/// Drive the guest into s2idle from inside (scheduled a beat ahead), then confirm entry
/// by SSH going dark (two consecutive failures so one dropped connection can't fake it).
fn suspend_in_guest_and_wait_dark(guest: &Guest) {
    guest
        .ssh_exec("sudo systemd-run --on-active=2 systemctl suspend -i >/dev/null 2>&1; echo armed")
        .expect("arming the in-guest suspend");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut consecutive_failures = 0;
    loop {
        match guest.ssh_exec_timeout("true", Duration::from_secs(8)) {
            Err(_) => {
                consecutive_failures += 1;
                if consecutive_failures >= 2 {
                    return;
                }
            }
            Ok(_) => consecutive_failures = 0,
        }
        assert!(
            std::time::Instant::now() < deadline,
            "guest never entered s2idle (SSH kept answering for 60s after systemctl suspend)"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[test]
fn stock_guest_survives_inplace_s2idle_with_correct_clock() {
    if !limina_test::require_hvf_or_skip("stock_guest_survives_inplace_s2idle_with_correct_clock") {
        return;
    }

    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg.with_net().with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED stock_guest_survives_inplace_s2idle_with_correct_clock: {e}");
            return;
        }
    };

    eprintln!("booting the stock Fedora image via EFI (in-place s2idle vehicle)");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable");
    eprintln!("guest SSH up: {banner}");

    // The clock assertion below must hold via the RTC path alone (PL031 → rtc-efi →
    // kernel sleeptime injection), not because chrony fixed it after the fact.
    let _ = guest.ssh_exec("sudo systemctl stop chronyd || true");

    let boot_id = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading the pre-suspend boot_id")
        .trim()
        .to_string();
    assert!(!boot_id.is_empty(), "empty pre-suspend boot_id");
    let worker = guest.worker_pid().expect("resolving the worker pid");
    eprintln!("pre-suspend: boot_id={boot_id} worker={worker}");

    // Suspend from inside the guest and confirm s2idle entry (SSH goes dark once
    // virtio-net freezes).
    suspend_in_guest_and_wait_dark(&guest);
    eprintln!("guest is asleep (SSH unreachable); holding a {SLEEP_GAP:?} wallclock gap");
    std::thread::sleep(SLEEP_GAP);

    // Wake via the worker's SIGWINCH seam — the KEY_WAKEUP GPIO pulse, exactly what the
    // host-sleep bracket fires on didWake. The worker must still be the SAME process.
    let ret = unsafe { libc::kill(worker, libc::SIGWINCH) };
    assert_eq!(
        ret, 0,
        "SIGWINCH to the worker failed — did the worker die?"
    );
    eprintln!("wake pulsed (SIGWINCH → worker {worker})");

    guest
        .ssh_poll("true", Duration::from_secs(90))
        .expect("guest never came back on SSH after the wake pulse");

    // Same boot (resumed, not rebooted), same worker (in-place, no relaunch).
    let boot_id_after = guest
        .ssh_exec("cat /proc/sys/kernel/random/boot_id")
        .expect("reading the post-wake boot_id")
        .trim()
        .to_string();
    assert_eq!(
        boot_id_after, boot_id,
        "boot_id changed across the in-place s2idle — the guest rebooted"
    );
    let worker_after = guest
        .worker_pid()
        .expect("resolving the post-wake worker pid");
    assert_eq!(
        worker_after, worker,
        "worker pid changed — the VMM was relaunched; this test must be in-place"
    );

    // Wallclock guard: the kernel's thaw must have stepped CLOCK_REALTIME across the
    // gap via the RTC (0088). A broken path leaves the guest ~SLEEP_GAP behind.
    let guest_now: f64 = guest
        .ssh_exec("date +%s.%N")
        .expect("reading the guest wallclock")
        .trim()
        .parse()
        .expect("parsing the guest wallclock");
    let host_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("host wallclock")
        .as_secs_f64();
    let skew = guest_now - host_now;
    eprintln!("post-wake: boot_id preserved, worker preserved, clock skew {skew:+.3}s");
    assert!(
        skew.abs() <= 5.0,
        "guest wallclock skewed {skew:+.1}s from the host after the in-place wake — \
         the RTC sleeptime-injection path is broken"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("shutting down the woken guest");
    eprintln!("teardown outcome: {outcome:?}");
}

/// The seated GNOME session must survive an **in-place** s2idle round-trip — same worker,
/// no snapshot, no replay. The transport survives (0072 sticky re-arm, proven above), and
/// the host GPU world never went anywhere — the only thing that destroys it is the
/// worker's own unconditional `reset_session()` on the thaw's device reset
/// (`worker.rs:228`). The defer-and-classify fix (docs/design/host-sleep-s2idle.md §3)
/// parks the session across the reset and adopts it when the re-arm signature identifies
/// the activation as a thaw.
///
/// RED before that fix: the wipe leaves the guest's mesa believing in contexts/rings the
/// host no longer has; the shell's first submission spins in vn_relax (~17 s) and
/// SIGABRTs — a fresh session replaces it (new pid, +1 coredump). Oracles are identity,
/// not rendering: same boot_id, same gnome-shell pid past the abort window, no new cores.
#[test]
fn venus_session_survives_inplace_s2idle() {
    if !limina_test::require_hvf_or_skip("venus_session_survives_inplace_s2idle") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED venus_session_survives_inplace_s2idle: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let base_cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED venus_session_survives_inplace_s2idle: {e}");
            return;
        }
    };

    // The injected 16 KiB test kernel (6.12) has no freeze support in virtio_i2c/virtio_snd,
    // so those devices would abort the guest's s2idle entry — drop them (same as the
    // snapshot gate test). No MAC pinning needed: the worker and gvproxy never die here.
    let cfg = base_cfg
        .with_supervisor_arg("--no-snd")
        .with_supervisor_arg("--no-battery")
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log();

    eprintln!("booting the seated enhanced venus desktop (in-place s2idle vehicle)");
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable");
    eprintln!("guest SSH up: {banner}");
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated enhanced session didn't come up");

    // Let the session settle so the shell's venus world (rings, glyph caches, scanouts)
    // is steady-state — the real lid-close scenario.
    std::thread::sleep(Duration::from_secs(10));

    let boot_id = ssh_retry(&guest, "cat /proc/sys/kernel/random/boot_id");
    let shell_pid = ssh_retry(&guest, "pgrep -x gnome-shell | head -1");
    assert!(
        !shell_pid.is_empty(),
        "no gnome-shell pid before the suspend"
    );
    let cores_before = ssh_retry(
        &guest,
        "sudo coredumpctl list --no-legend gnome-shell 2>/dev/null | wc -l",
    );
    let worker = guest.worker_pid().expect("resolving the worker pid");
    eprintln!(
        "pre-suspend: boot_id={boot_id} gnome-shell pid={shell_pid} cores={cores_before} \
         worker={worker}"
    );

    suspend_in_guest_and_wait_dark(&guest);
    eprintln!("guest is asleep; waking in-place via the SIGWINCH seam");
    std::thread::sleep(Duration::from_secs(3));

    let ret = unsafe { libc::kill(worker, libc::SIGWINCH) };
    assert_eq!(
        ret, 0,
        "SIGWINCH to the worker failed — did the worker die?"
    );

    guest
        .ssh_poll("true", Duration::from_secs(90))
        .expect("guest never came back on SSH after the wake pulse");
    let boot_id_after = ssh_retry(&guest, "cat /proc/sys/kernel/random/boot_id");
    assert_eq!(
        boot_id_after, boot_id,
        "boot_id changed across the in-place s2idle — the guest rebooted"
    );

    // Ride out the abort window: the pre-fix failure is a DELAYED crash (~17 s in
    // vn_relax), so a same-pid reading right after SSH-back proves nothing yet.
    eprintln!(
        "riding out the {}s abort window before the identity checks",
        ABORT_WINDOW.as_secs()
    );
    std::thread::sleep(ABORT_WINDOW);

    let shell_pid_after = ssh_retry(&guest, "pgrep -x gnome-shell | head -1");
    let cores_after = ssh_retry(
        &guest,
        "sudo coredumpctl list --no-legend gnome-shell 2>/dev/null | wc -l",
    );
    eprintln!("post-wake: gnome-shell pid={shell_pid_after} cores={cores_after}");
    assert_eq!(
        cores_after, cores_before,
        "gnome-shell dumped core across the in-place s2idle — the venus session was \
         wiped by the thaw reset (defer-and-classify regression)"
    );
    assert_eq!(
        shell_pid_after, shell_pid,
        "gnome-shell is a DIFFERENT process after the in-place wake — the session \
         restarted instead of being preserved"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(20))
        .expect("shutting down the woken guest");
    eprintln!("teardown outcome: {outcome:?}");
}
