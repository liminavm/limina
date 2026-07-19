// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M9.3 seamless-resume acceptance: the seated GNOME session must SURVIVE a
//! snapshot/restore round-trip — same gnome-shell process, no compositor crash.
//!
//! This is the gate test for `docs/design/venus-snapshot-replay.md`. The transport
//! layer already resumes clean (libkrun 0072 sticky queue re-arm: same boot_id, SSH
//! back, CRTC lights, rendering works), but the host restores with an EMPTY GPU
//! world while the guest's mesa still believes in its pre-suspend venus context and
//! rings. Its first real submission after thaw spins in `vn_relax` until mesa's
//! dead-ring abort threshold (~17 s) and the shell SIGABRTs — every Wayland client
//! dies and GNOME starts a FRESH session. What looks like recovery is a restart
//! (root-caused 2026-07-19 from the coredump: abort ← vn_relax ←
//! vn_ring_submit_locked ← vn_CreateImage ← cogl glyph upload; see
//! `spikes/m9-freeze-trigger/RESULTS.md` round 10).
//!
//! RED until the venus snapshot-replay phases land (P1: replay the re-creation
//! journals + ring-blob contents so the guest's rings stay serviced). The oracles
//! are deliberately about *identity*, not rendering: same boot_id (it resumed, not
//! rebooted), same gnome-shell PID across the round-trip AND past the ~17 s abort
//! window, and zero new gnome-shell coredumps. Visual fidelity is P2's gate, not
//! this test's.
//!
//! Same prereqs as `venus_fd_census`: the seated enhanced golden + KosmicKrisp;
//! SKIPs cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::path::PathBuf;
use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// How long past SSH-back we insist the shell stays alive. The observed abort fires
/// ~17 s after restore (mesa's vn_relax dead-ring threshold); 35 s covers it with
/// margin without meaningfully slowing a green run.
const ABORT_WINDOW: Duration = Duration::from_secs(35);

/// `ssh_exec` with a few retries: a loaded host (the suite runs several VMs) can
/// drop a single connection (exit 255) right after the banner poll succeeded.
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

#[test]
fn seated_gnome_session_survives_snapshot_restore() {
    if !limina_test::require_hvf_or_skip("seated_gnome_session_survives_snapshot_restore") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED seated_gnome_session_survives_snapshot_restore: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let base_cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED seated_gnome_session_survives_snapshot_restore: {e}");
            return;
        }
    };

    // The harness's injected 16 KiB test kernel (a 6.12 build) has no freeze support in
    // virtio_i2c/virtio_snd, so those two devices hold out the s2idle quiesce and the
    // suspend bracket aborts. The test needs neither battery nor audio — drop them on
    // BOTH sides of the round-trip (restore requires an identical device topology).
    //
    // The MAC is pinned on both sides: the restored guest keeps the NIC identity it read
    // at boot (it does not re-probe config space mid-resume), and production restore
    // carries the managed VM's MAC forward the same way. A fresh random MAC on the
    // restore worker orphans the guest's cached one and SSH never comes back.
    const NET_MAC: &str = "5a:94:ef:44:0f:aa";
    let devices = |cfg: GuestConfig| {
        cfg.with_supervisor_arg("--no-snd")
            .with_supervisor_arg("--no-battery")
            .with_net_mac(NET_MAC)
    };

    // --- Guest 1: seated venus desktop, snapshot-armed ---
    let cfg1 = devices(base_cfg.clone())
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .with_snapshot();
    eprintln!("booting the seated enhanced venus desktop (snapshot-armed)");
    let mut g1 = Guest::boot(&cfg1).expect("spawning the limina supervisor");
    let banner = g1
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    g1.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated enhanced session didn't come up");

    // Let the session settle: the shell's venus world (rings, glyph caches, scanouts)
    // should be steady-state, matching the real suspend-a-working-desktop scenario.
    std::thread::sleep(Duration::from_secs(10));

    // Identity baseline: the resumed guest must present the SAME kernel boot and the
    // SAME compositor process.
    let boot_id = ssh_retry(&g1, "cat /proc/sys/kernel/random/boot_id");
    let shell_pid = ssh_retry(&g1, "pgrep -x gnome-shell | head -1");
    assert!(
        !shell_pid.is_empty(),
        "no gnome-shell pid before the snapshot"
    );
    // Coredump baseline (count, not emptiness: the golden may carry old cores).
    let cores_before = ssh_retry(
        &g1,
        "sudo coredumpctl list --no-legend gnome-shell 2>/dev/null | wc -l",
    );
    eprintln!("pre-suspend: boot_id={boot_id} gnome-shell pid={shell_pid} cores={cores_before}");

    // --- Suspend: s2idle bracket -> quiesce -> snapshot -> worker exits 126 ---
    // The bracket's suspend-button pulse is swallowed depending on the desktop's
    // session state (observed: a fresh seated session ignores it and no device ever
    // leaves DRIVER_OK), so trigger the s2idle from INSIDE the guest, scheduled a
    // beat ahead and inhibitor-ignoring, then arm the bracket for quiesce+snapshot.
    ssh_retry(
        &g1,
        "sudo systemd-run --on-active=2 systemctl suspend -i >/dev/null 2>&1; echo armed",
    );
    g1.suspend_bracket().expect("sending the suspend bracket");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(120))
        .expect("supervisor did not exit after the suspend bracket");
    assert_eq!(
        outcome.code,
        Some(126),
        "the seated guest should suspend (worker exit 126); got {outcome:?}\n\
         === supervisor+worker log ===\n{}",
        g1.supervisor_log()
    );

    // Preserve the snapshot AND the disk the guest was running on — the restore must
    // resume against the exact filesystem state it suspended with, not a fresh clone
    // of the golden.
    let scratch = g1.scratch_dir().to_path_buf();
    let pid = std::process::id();
    let snap: PathBuf = std::env::temp_dir().join(format!("limina-venus-session-{pid}.snap"));
    let disk: PathBuf = std::env::temp_dir().join(format!("limina-venus-session-{pid}.raw"));
    std::fs::copy(g1.snapshot_path().expect("snapshot path configured"), &snap)
        .expect("preserving the snapshot file");
    limina_test::cow_clone(&scratch.join("disk.raw"), &disk)
        .expect("preserving the suspended guest's disk");
    drop(g1);

    let cleanup = || {
        // LIMINA_KEEP_ARTIFACTS=1 preserves the snapshot+disk pair on failure so a
        // failing restore can be re-run by hand (the pair is the whole repro).
        if std::env::var_os("LIMINA_KEEP_ARTIFACTS").is_some() {
            eprintln!(
                "keeping artifacts: snap={} disk={}",
                snap.display(),
                disk.display()
            );
            return;
        }
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(&disk);
    };

    // --- Guest 2: fresh worker restoring the snapshot against the preserved disk ---
    let mut cfg2 = devices(base_cfg)
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .restore_from(&snap);
    if let limina_test::Boot::KernelDisk { disk: d, .. } = &mut cfg2.boot {
        *d = disk.clone();
    }
    let mut g2 = Guest::boot(&cfg2).expect("spawning the restoring supervisor");
    g2.wait_for_supervisor_log("restoring from snapshot", Duration::from_secs(30))
        .unwrap_or_else(|e| {
            cleanup();
            panic!("restore worker never entered the restore path: {e}");
        });
    let banner = g2
        .wait_for_ssh_banner(Duration::from_secs(120))
        .unwrap_or_else(|e| {
            // Liveness forensics: the console tail shows whether the guest thaw
            // completed (vs a wedged resume), and a present frame proves the
            // compositor is alive even with the network down.
            let console = g2.console();
            let tail: Vec<&str> = console.lines().rev().take(30).collect();
            eprintln!("--- restored guest console tail ---");
            for line in tail.iter().rev() {
                eprintln!("{line}");
            }
            match g2.read_capture() {
                Ok(f) => eprintln!("display capture: {}x{} frame present", f.width, f.height),
                Err(e) => eprintln!("display capture: none ({e})"),
            }
            cleanup();
            panic!("restored guest never became reachable over SSH: {e}");
        });
    eprintln!("restored guest SSH up: {banner}");

    // Same kernel boot — a differing boot_id means the guest REBOOTED, which is a
    // transport regression (0072), not the session gap this test measures.
    let boot_id_after = ssh_retry(&g2, "cat /proc/sys/kernel/random/boot_id");
    assert_eq!(
        boot_id_after, boot_id,
        "boot_id changed across restore — the guest rebooted instead of resuming"
    );

    // Ride out the abort window: the pre-fix failure mode is a DELAYED crash (~17 s
    // in vn_relax), so a same-pid reading right after SSH-back proves nothing yet.
    eprintln!(
        "riding out the {}s abort window before the identity checks",
        ABORT_WINDOW.as_secs()
    );
    std::thread::sleep(ABORT_WINDOW);

    let shell_pid_after = ssh_retry(&g2, "pgrep -x gnome-shell | head -1");
    let cores_after = ssh_retry(
        &g2,
        "sudo coredumpctl list --no-legend gnome-shell 2>/dev/null | wc -l",
    );
    eprintln!("post-restore: gnome-shell pid={shell_pid_after} cores={cores_after}");
    cleanup();

    assert_eq!(
        cores_after, cores_before,
        "gnome-shell dumped core across the restore — the venus session was lost \
         (mesa vn_relax dead-ring abort; the host GPU world was not replayed)"
    );
    assert_eq!(
        shell_pid_after, shell_pid,
        "gnome-shell is a DIFFERENT process after restore — the session restarted \
         instead of being preserved (the seamless-resume goal)"
    );

    let outcome = g2
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the restored guest");
    eprintln!("teardown outcome: {outcome:?}");
}
