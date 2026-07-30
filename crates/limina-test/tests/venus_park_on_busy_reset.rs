// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! §22 hardening guard: a seated venus session that is merely MID-FRAME at a device
//! reset must PARK (and survive the s2idle round-trip), not take the fail-closed wipe.
//!
//! The dogfood-mac 2026-07-30 incident: an aborted host-sleep attempt s2idled the guest while
//! the compositor was inside its suspend fade-out animation. At the device reset the
//! fence ledger was empty but the present-fence plumbing wasn't (`present_quiescent()`
//! false), so defer-and-classify wiped a perfectly healthy session — and the replayed
//! fence then wedged KMS (fixed separately, libkrun 0117). The wipe itself was the
//! *cause* half: the old code classified on an instant verdict taken AFTER the
//! activation was unbound, when nothing could retire anymore, so any in-flight frame
//! condemned the session.
//!
//! The fix (drain-then-classify): before unbinding, pump retired presents and let the
//! fence callbacks + guest-hold latch (500 ms ceiling) drain, bounded at 1.5 s. This
//! test drives the same shape: seated enhanced golden with `LIMINA_FENCE_PRESENT=1`
//! forced (frames park, guest flush fences are held — the plumbing the dogfood-mac reset
//! tripped over), in-guest suspend (GNOME's own suspend fade supplies present traffic
//! right at entry), SIGWINCH wake. Oracles:
//!
//!  - the worker log has NO "wiping (fail-closed)" — the session parked;
//!  - same boot_id and same gnome-shell PID across the bracket, held past the ~17 s
//!    vn_relax abort window (a wiped venus world kills the shell with that delay);
//!  - zero new gnome-shell coredumps.
//!
//! A run where the reset happens to land on an idle frame parks under old and new code
//! alike (less discriminating, still valid); the suspend fade makes the busy case the
//! common one. Same prereqs as the other seated L2s; SKIPs cleanly if missing. Gated
//! behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// How long past resume the shell must stay alive: a wiped venus world kills it
/// ~17 s after the first post-thaw submission (mesa's vn_relax dead-ring abort).
const ABORT_WINDOW: Duration = Duration::from_secs(35);

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
fn busy_seated_session_parks_across_reset() {
    if !limina_test::require_hvf_or_skip("busy_seated_session_parks_across_reset") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED busy_seated_session_parks_across_reset: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log()
            // Force the fence-present chain on: parked frames + held guest flush
            // fences are exactly the plumbing the dogfood-mac reset tripped over.
            .with_env("LIMINA_FENCE_PRESENT", "1"),
        Err(e) => {
            eprintln!("SKIPPED busy_seated_session_parks_across_reset: {e}");
            return;
        }
    };

    eprintln!("booting the seated enhanced venus desktop (fence-present forced)");
    let mut g = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = g
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    g.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated enhanced session didn't come up");
    std::thread::sleep(Duration::from_secs(10));

    let boot_id = ssh_retry(&g, "cat /proc/sys/kernel/random/boot_id");
    let shell_pid = ssh_retry(&g, "pgrep -x gnome-shell | head -1");
    let coredumps_before = ssh_retry(
        &g,
        "coredumpctl list gnome-shell --no-legend 2>/dev/null | wc -l",
    );
    let worker = g.worker_pid().expect("resolving the worker pid");
    eprintln!("pre-suspend: boot_id={boot_id} gnome-shell={shell_pid} worker={worker}");

    // Suspend from inside; GNOME's fade-out supplies present traffic right at entry.
    suspend_in_guest_and_wait_dark(&g);
    eprintln!("guest is asleep; holding a 10s gap");
    std::thread::sleep(Duration::from_secs(10));

    let ret = unsafe { libc::kill(worker, libc::SIGWINCH) };
    assert_eq!(
        ret, 0,
        "SIGWINCH to the worker failed — did the worker die?"
    );
    g.ssh_poll("true", Duration::from_secs(90))
        .expect("guest never came back on SSH after the wake pulse");

    // THE classifier oracle: the session must have parked, not wiped.
    let slog = g.supervisor_log();
    assert!(
        !slog.contains("wiping (fail-closed)"),
        "the reset classifier WIPED the session — drain-then-classify failed \
         (grep the log for the NOT-quiescent WARN to see what was still pending)"
    );
    assert!(
        slog.contains("PARKED for classification"),
        "no PARK line in the worker log — the bracket never exercised the classifier?"
    );

    // Identity across the bracket, held past the vn_relax abort window.
    let boot_id_after = ssh_retry(&g, "cat /proc/sys/kernel/random/boot_id");
    assert_eq!(boot_id_after, boot_id, "guest rebooted across the bracket");
    std::thread::sleep(ABORT_WINDOW);
    let shell_pid_after = ssh_retry(&g, "pgrep -x gnome-shell | head -1");
    assert_eq!(
        shell_pid_after, shell_pid,
        "gnome-shell restarted across the bracket — the venus session did not survive"
    );
    let coredumps_after = ssh_retry(
        &g,
        "coredumpctl list gnome-shell --no-legend 2>/dev/null | wc -l",
    );
    assert_eq!(
        coredumps_after, coredumps_before,
        "gnome-shell dumped core across the bracket"
    );

    eprintln!("session parked and survived: shell pid {shell_pid} intact past the abort window");
    g.shutdown(Duration::from_secs(60))
        .expect("guest failed to shut down cleanly");
}
