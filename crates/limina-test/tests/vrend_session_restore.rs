// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Task #19 gate: the CLASSIC (vrend) GL world must survive a snapshot restore.
//!
//! Since the 2026-08-04 GL-ladder flip the compositor and every GL client run on
//! classic vrend contexts. The M9.3 venus journal never covered them, so a restored
//! seated desktop renders into the void: the guest keeps submitting (classic virgl is
//! fire-and-forget — no client ever sees an error), while host-side every
//! `CmdSubmit3d` is rejected (`ComponentError(22)`) and scanout flips die with
//! `ErrInvalidResourceId`. Process-survival oracles all stay green through this —
//! `venus_session_preserved` explicitly defers visual fidelity — which is how the
//! regression shipped. Dossier: `spikes/restore-s2idle-wake/RESULTS.md`.
//!
//! This test suspends a seated desktop through the production bracket, restores, and
//! asserts on the HOST-side classic-health signals:
//!  1. no rejected-submit storm after the restore settles (pre-fix: thousands/sec),
//!  2. zero scanout/flush rejections (the pixel killers),
//!  3. the guest IS submitting (a quiet log must not pass as healthy — a black
//!     desktop with no traffic would satisfy (1) vacuously),
//!
//! plus the usual identity floor (same boot_id, shell alive).
//!
//! Pixel fidelity (capture-based) is the P2 gate, not this one.

use std::path::PathBuf;
use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// Post-restore settle: long enough for the shell to attempt several frames (the
/// clock redraws once a second; pre-fix the storm reaches thousands of rejections
/// within this window).
const SETTLE: Duration = Duration::from_secs(12);

/// Stale-reference tolerance: replay may drop a handful of entries whose referents
/// died around the snapshot (mirrors the venus NOTED-drop class). The pre-fix storm
/// is 3–4 orders of magnitude above this within SETTLE.
const MAX_REJECTIONS: usize = 50;

/// Sum the per-tick deltas of a `[GPUTRACE]` counter across an observation window.
/// The ticks are 2s snapshots and a desktop quiesces fast (a window-open animation
/// is over well inside SETTLE), so sampling only the LAST tick reads a healthy idle
/// desktop as `submits=+0` — the run-1 EFI false negative. Totals over the window
/// are what the oracles mean. Returns (total, ticks_seen).
fn tick_total(window: &str, name: &str) -> (i64, usize) {
    let mut total = 0i64;
    let mut ticks = 0usize;
    for line in window.lines().filter(|l| l.contains("[GPUTRACE] tick=")) {
        ticks += 1;
        if let Some(v) = line
            .split_whitespace()
            .find_map(|w| w.strip_prefix(&format!("{name}=+")))
            .and_then(|v| v.parse::<i64>().ok())
        {
            total += v;
        }
    }
    (total, ticks)
}

/// `ssh_exec` with a few retries: a loaded host can drop a single connection right
/// after the banner poll succeeded.
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
fn classic_vrend_world_survives_snapshot_restore() {
    if !limina_test::require_hvf_or_skip("classic_vrend_world_survives_snapshot_restore") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED classic_vrend_world_survives_snapshot_restore: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    // EFI boot, NOT the injected-6.12 seated path: the kernel-inject session never produces
    // classic CmdSubmit3d traffic even live (proved by this test's own baseline assert), so
    // only the firmware boot — the guest's own kernel, production reality — hosts the world
    // this gate protects.
    let base_cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED classic_vrend_world_survives_snapshot_restore: {e}");
            return;
        }
    };

    // Same device trims + pinned MAC as venus_session_preserved (virtio_i2c/virtio_snd
    // hold out the s2idle quiesce; a fresh random MAC on the restore leg orphans the
    // guest's cached NIC identity).
    const NET_MAC: &str = "5a:94:ef:44:0f:ab";
    let devices = |cfg: GuestConfig| {
        cfg.with_supervisor_arg("--no-snd")
            .with_supervisor_arg("--no-battery")
            .with_net_mac(NET_MAC)
    };

    // GPUTRACE ticks are the positive-signal oracle (submits advancing) and the
    // bracket must not double-trigger the in-guest suspend.
    std::env::set_var("LIMINA_GPU_TRACE", "1");
    std::env::set_var("LIMINA_BRACKET_NO_BUTTON", "1");

    // --- Guest 1: seated desktop (compositor on classic vrend), snapshot-armed ---
    let cfg1 = devices(base_cfg.clone())
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .with_snapshot();
    eprintln!("booting the seated desktop (snapshot-armed)");
    let mut g1 = Guest::boot(&cfg1).expect("spawning the limina supervisor");
    let banner = g1
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    g1.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated session didn't come up");

    // Let the shell's classic world reach steady state (objects created, state bound).
    std::thread::sleep(Duration::from_secs(10));

    // Guard the premise: this gate is only meaningful if the seated shell actually
    // RUNS on classic vrend. A session that fell back to llvmpipe ("No virgl contexts
    // available on host") has no classic world to lose and the whole test would pass
    // vacuously forever — which is exactly how the original regression stayed hidden.
    let no_virgl = ssh_retry(
        &g1,
        "journalctl -b --no-pager 2>/dev/null | grep -c 'No virgl contexts available' || true",
    );
    let gbm = ssh_retry(
        &g1,
        "journalctl -b --no-pager 2>/dev/null | grep -m1 'Created gbm renderer' || echo none",
    );
    eprintln!("pre-suspend renderer probe: no-virgl-hits={no_virgl} gbm={gbm:?}");
    assert_eq!(
        no_virgl.trim(),
        "0",
        "the seated session fell back from virgl (\"No virgl contexts available on host\" \
         in the guest journal) — there is no classic vrend world to gate; fix the L2 \
         environment before trusting this test"
    );

    // Baseline the oracle on the KNOWN-live world: the same stimulus we use
    // post-restore must produce classic submits here, or the oracle itself is
    // wrong for this environment and the post-restore reading means nothing.
    let pre_mark = g1.supervisor_log().len();
    ssh_retry(
        &g1,
        "export XDG_RUNTIME_DIR=/run/user/1000 \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; \
         systemd-run --user --collect --unit vrend-stim-pre \
         env WAYLAND_DISPLAY=wayland-0 gnome-calculator >/dev/null 2>&1; echo stim",
    );
    std::thread::sleep(SETTLE);
    let pre_log = g1.supervisor_log();
    let pre_window = &pre_log[pre_mark.min(pre_log.len())..];
    let (pre_submits, pre_ticks) = tick_total(pre_window, "submits");
    let pre_calc = ssh_retry(&g1, "pgrep -fc gnome-calculator || true");
    eprintln!(
        "pre-suspend baseline: calc-procs={pre_calc} submits=+{pre_submits} \
         over {pre_ticks} ticks"
    );
    assert!(
        pre_submits > 0,
        "the LIVE seated session produced no classic submits under the stimulus \
         (submits=+{pre_submits} over {pre_ticks} ticks, calc-procs={pre_calc}) — \
         the oracle does not measure this environment; fix the stimulus/oracle \
         before trusting the gate"
    );

    let boot_id = ssh_retry(&g1, "cat /proc/sys/kernel/random/boot_id");
    let shell_pid = ssh_retry(&g1, "pgrep -x gnome-shell | head -1");
    assert!(
        !shell_pid.is_empty(),
        "no gnome-shell pid before the snapshot"
    );
    eprintln!("pre-suspend: boot_id={boot_id} gnome-shell pid={shell_pid}");

    // --- Suspend through the production bracket (in-guest trigger, no button) ---
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

    let scratch = g1.scratch_dir().to_path_buf();
    let pid = std::process::id();
    let snap: PathBuf = std::env::temp_dir().join(format!("limina-vrend-session-{pid}.snap"));
    let disk: PathBuf = std::env::temp_dir().join(format!("limina-vrend-session-{pid}.raw"));
    std::fs::copy(g1.snapshot_path().expect("snapshot path configured"), &snap)
        .expect("preserving the snapshot file");
    limina_test::cow_clone(&scratch.join("disk.raw"), &disk)
        .expect("preserving the suspended guest's disk");
    drop(g1);

    let snap_consumed = snap.with_extension("bin.consumed");
    let cleanup = || {
        if std::env::var_os("LIMINA_KEEP_ARTIFACTS").is_some() {
            eprintln!(
                "keeping artifacts: snap={} (or {}) disk={}",
                snap.display(),
                snap_consumed.display(),
                disk.display()
            );
            return;
        }
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(&snap_consumed);
        let _ = std::fs::remove_file(&disk);
    };

    // --- Guest 2: fresh worker restoring against the preserved disk ---
    let mut cfg2 = devices(base_cfg.clone())
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .restore_from(&snap);
    if let limina_test::Boot::Firmware { disk: d, .. } = &mut cfg2.boot {
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
            eprintln!("--- restore supervisor log tail ---");
            let slog = g2.supervisor_log();
            for line in slog.lines().rev().take(25).collect::<Vec<_>>().iter().rev() {
                eprintln!("{line}");
            }
            cleanup();
            panic!("restored guest never became reachable over SSH: {e}");
        });
    eprintln!("restored guest SSH up: {banner}");

    // Identity floor.
    let boot_id_after = ssh_retry(&g2, "cat /proc/sys/kernel/random/boot_id");
    assert_eq!(
        boot_id_after, boot_id,
        "boot_id changed across restore — the guest rebooted instead of resuming"
    );

    // A healthy idle desktop is GPU-SILENT (submits=+0 proves nothing either way), so
    // stimulate real compositing through the restored world: launch a fresh app in the
    // seated session — its window-open animation forces the shell to draw frames using
    // the replayed framebuffers/objects, and the new client exercises fresh context
    // creation alongside them.
    let log_mark = g2.supervisor_log().len();
    // The in-guest `systemctl suspend` DPMS-blanked the display and the headless L2 has
    // no real user input to un-blank it — a blanked compositor issues no frame callbacks
    // and clients go idle (the rounds-5/6 "CRTC stays off" shape, benign here). Simulate
    // user activity first, then open the window.
    ssh_retry(
        &g2,
        "export XDG_RUNTIME_DIR=/run/user/1000 \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; \
         busctl --user call org.gnome.ScreenSaver /org/gnome/ScreenSaver \
         org.gnome.ScreenSaver SimulateUserActivity >/dev/null 2>&1; \
         systemd-run --user --collect --unit vrend-stim \
         env WAYLAND_DISPLAY=wayland-0 gnome-calculator >/dev/null 2>&1; echo stim",
    );
    std::thread::sleep(SETTLE);

    let log = g2.supervisor_log();
    let window = &log[log_mark.min(log.len())..];

    // (2) The pixel killers must be GONE outright: a single failing scanout flip
    // means the compositor's framebuffer resource did not survive the restore.
    let scanout_rejects = window
        .matches("SetScanout) -> ErrInvalidResourceId")
        .count()
        + window
            .matches("ResourceFlush) -> ErrInvalidResourceId")
            .count();

    // (1) No rejected-submit storm.
    let submit_rejects = window.matches("CmdSubmit3d) -> ErrRutabaga").count();

    // (3) Positive signal: the guest is actually submitting — total the GPUTRACE
    // deltas across the whole settle window (the last tick alone reads an
    // already-idle healthy desktop as +0) and require submits with no
    // unknown-ctx/res. Guards against a vacuous pass from a silent guest.
    let (submits, ticks) = tick_total(window, "submits");
    let (unknown_ctx, _) = tick_total(window, "unknown_ctx");
    let (unknown_res, _) = tick_total(window, "unknown_res");
    // The RED signature on this leg was errs=+20k/settle with ZERO text-matching
    // rejections — the device error counter is the load-bearing storm oracle here.
    let (errs, _) = tick_total(window, "errs");

    let shell_pid_after = ssh_retry(&g2, "pgrep -x gnome-shell | head -1");

    let verdict_ok = scanout_rejects == 0
        && submit_rejects <= MAX_REJECTIONS
        && ticks > 0
        && submits > 0
        && unknown_ctx == 0
        && unknown_res == 0
        && (0..=MAX_REJECTIONS as i64).contains(&errs)
        && shell_pid_after == shell_pid;
    if !verdict_ok {
        // Guest-side display forensics before teardown: distinguishes "world dead"
        // from "display never re-lit" (both read as submits=+0 host-side).
        let drm = g2
            .ssh_exec(
                "for f in /sys/class/drm/card*-*/dpms /sys/class/drm/card*-*/enabled; do \
                 echo \"$f: $(cat $f 2>/dev/null)\"; done; \
                 echo calc-procs=$(pgrep -fc gnome-calculator || true)",
            )
            .unwrap_or_else(|e| format!("(drm probe failed: {e})"));
        let replay_lines: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("restore:") || l.contains("replay") || l.contains("journal"))
            .collect();
        eprintln!("--- restore-leg replay/journal lines ---");
        for l in replay_lines
            .iter()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            eprintln!("{}", &l[..l.len().min(190)]);
        }
        cleanup();
        panic!(
            "the classic vrend world did not survive the restore:\n\
             scanout/flush rejections: {scanout_rejects} (must be 0)\n\
             submit rejections:        {submit_rejects} (max {MAX_REJECTIONS})\n\
             window totals ({ticks} ticks): submits=+{submits} unknown_ctx=+{unknown_ctx} \
             unknown_res=+{unknown_res} errs=+{errs}\n\
             (submits must advance, zero unknowns, errs within tolerance)\n\
             gnome-shell pid:          {shell_pid} -> {shell_pid_after}\n\
             guest display state:\n{drm}"
        );
    }

    eprintln!(
        "classic world survived: scanout_rejects=0 submit_rejects={submit_rejects} \
         submits=+{submits} errs=+{errs} over {ticks} ticks"
    );
    cleanup();
}
