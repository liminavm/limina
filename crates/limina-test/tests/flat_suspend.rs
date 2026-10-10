// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Task #20: a FLAT `--disk` run arms suspend by default and never boots past a
//! pending resume.
//!
//! The invariant (user-stated): a VM with a pending resume either USES it (the
//! default) or the user explicitly DISCARDS it — a cold boot that silently ignores
//! an existing snapshot must be impossible. Flat runs derive the snapshot path from
//! the boot disk (`<disk>.limina-suspend.bin`): the pair is only valid together, so
//! they travel together, and relaunching the same disk finds the pending resume with
//! zero flags.
//!
//! Vehicle: the stock seated golden EFI-booted IN PLACE on a private CoW clone
//! (`Boot::Firmware` without net boots the configured disk directly — no harness
//! scratch clone, so the disk path, and with it the derived snapshot identity, is
//! stable across legs), in a real window. No SSH: the suspend is the bracket's own
//! button pulse (the seated GNOME session honors the suspend key), and the oracles are
//! the supervisor log, its exit, and the on-disk snapshot artifacts. Two suspend/resume
//! cycles run back to back through `limina suspend <disk>`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use limina_test::{Boot, Guest, GuestConfig};

/// A flat-shaped config for `disk`: firmware boot, in place, writable, no snapshot
/// flags at all — exactly what `limina --disk <disk>` resolves to.
fn flat_cfg(disk: &Path) -> GuestConfig {
    let mut cfg = GuestConfig::baseline_fedora_from_env().expect("baseline config");
    cfg.boot = Boot::Firmware {
        firmware: match &cfg.boot {
            Boot::Firmware { firmware, .. } => firmware.clone(),
            _ => unreachable!("baseline_fedora_from_env builds a Firmware boot"),
        },
        disk: disk.to_path_buf(),
        read_only: false, // in-place writable = the flat dev-run shape
    };
    // Windowed, as `limina --disk` runs on a desktop: the window is what decides whether a
    // suspend parks or exits.
    cfg.with_windowed_coexist_display(1280, 800)
        .with_supervisor_log()
}

#[test]
fn flat_run_default_arms_suspend_and_resumes_pending() {
    if !limina_test::require_hvf_or_skip("flat_run_default_arms_suspend_and_resumes_pending") {
        return;
    }
    let base = match GuestConfig::baseline_fedora_from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIPPED flat_run_default_arms_suspend_and_resumes_pending: {e}");
            return;
        }
    };
    let golden = match &base.boot {
        Boot::Firmware { disk, .. } => disk.clone(),
        _ => unreachable!(),
    };

    // Private clone OUTSIDE any harness scratch: its path is the stable identity the
    // derived snapshot keys on.
    let pid = std::process::id();
    let disk = std::env::temp_dir().join(format!("limina-flat-suspend-{pid}.raw"));
    limina_test::cow_clone(&golden, &disk).expect("cloning the stock golden");
    let snap = PathBuf::from(format!("{}.limina-suspend.bin", disk.display()));
    let cleanup = || {
        let _ = std::fs::remove_file(&disk);
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(snap.with_extension("bin.consumed"));
        let _ = std::fs::remove_file(snap.with_extension("splash.png"));
    };

    // --- Two back-to-back cycles: boot (cold, then resumed), suspend through the CLI ---
    // `limina suspend <disk>` finds the flat supervisor by its argv (pgrep), relays SIGTSTP, and
    // waits for the snapshot. The run is windowed, the shape `limina --disk` runs in, and the
    // supervisor must EXIT after a CLI suspend rather than park behind its play button: a parked
    // one keeps its gateway and SSH port, and the next `limina suspend` of the disk found two
    // supervisors and refused. The second cycle starts from a resumed guest, which is the path
    // that broke.
    let base_for_bin = GuestConfig::baseline_fedora_from_env().expect("baseline config");
    for cycle in 1..=2 {
        let mut g = Guest::boot(&flat_cfg(&disk)).expect("booting the flat guest");
        if cycle == 1 {
            g.wait_for_supervisor_log("suspend armed by default", Duration::from_secs(60))
                .expect("flat run did not default-arm suspend (task #20)");
            if let Err(e) = g.wait_for_launch_kind(1, false, Duration::from_secs(30)) {
                cleanup();
                panic!("the first boot of a fresh clone was not a cold boot: {e:#}");
            }
        } else {
            if let Err(e) = g.wait_for_launch_kind(1, true, Duration::from_secs(30)) {
                let log = g.supervisor_log();
                cleanup();
                panic!("the relaunch cold-booted past a pending resume: {e:#}\n{log}");
            }
            // Single-use: the canonical snapshot is consumed (renamed) the moment the resume
            // starts.
            assert!(
                !snap.exists(),
                "the consumed snapshot must leave its canonical path (single-use invariant)"
            );
        }
        // Seated GNOME needs to be up (or back) before the suspend button means anything.
        std::thread::sleep(Duration::from_secs(if cycle == 1 { 90 } else { 45 }));

        let cli = std::process::Command::new(&base_for_bin.limina_bin)
            .arg("suspend")
            .arg(&disk)
            .output()
            .expect("running limina suspend");
        if !cli.status.success() {
            let log = g.supervisor_log();
            cleanup();
            panic!(
                "cycle {cycle}: `limina suspend {}` failed: {}\nstderr: {}\nsupervisor log:\n{log}",
                disk.display(),
                cli.status,
                String::from_utf8_lossy(&cli.stderr)
            );
        }
        let outcome = match g.wait_supervisor_exit(Duration::from_secs(60)) {
            Ok(o) => o,
            Err(e) => {
                let log = g.supervisor_log();
                cleanup();
                panic!("cycle {cycle}: the supervisor outlived a CLI suspend: {e}\n{log}");
            }
        };
        // The windowed path exits 0 once it has saved its state; a headless one returns the
        // worker's 126. Either way the snapshot is what carries the session.
        if !matches!(outcome.code, Some(0) | Some(126)) {
            let log = g.supervisor_log();
            cleanup();
            panic!("cycle {cycle}: flat suspend failed (exit {outcome:?});\n{log}");
        }
        drop(g);
        assert!(
            snap.exists(),
            "cycle {cycle}: suspend must leave the snapshot at the disk-derived path {}",
            snap.display()
        );
    }

    // --- The second snapshot resumes too ---
    let mut g2 = Guest::boot(&flat_cfg(&disk)).expect("relaunching the flat guest");
    if let Err(e) = g2.wait_for_launch_kind(1, true, Duration::from_secs(30)) {
        let log = g2.supervisor_log();
        cleanup();
        panic!("the second snapshot did not resume: {e:#}\n{log}");
    }
    let _ = g2.shutdown(Duration::from_secs(30));

    // --- Leg 3: a pending resume + --discard-suspend must COLD boot and delete it ---
    std::fs::remove_file(&disk).expect("removing the leg-2 disk");
    limina_test::cow_clone(&golden, &disk).expect("re-cloning for the discard leg");
    std::fs::write(&snap, b"stale-but-present").expect("planting a pending snapshot");
    let cfg3 = flat_cfg(&disk).with_supervisor_arg("--discard-suspend");
    let mut g3 = Guest::boot(&cfg3).expect("booting with --discard-suspend");
    // The discard happens before the worker spawns, so by the first launch the snapshot must be
    // gone, and that launch must be a cold boot.
    let launch = g3.wait_for_launch(1, Duration::from_secs(60));
    let log = g3.supervisor_log();
    let cold = launch
        .as_ref()
        .is_ok_and(|l| l.get("resumed").map(String::as_str) == Some("no"));
    if !cold || snap.exists() {
        cleanup();
        panic!(
            "--discard-suspend must delete the pending snapshot and cold-boot \
             (snap.exists()={}, first launch {launch:?}, log:\n{log})",
            snap.exists(),
        );
    }
    let _ = g3.shutdown(Duration::from_secs(30));
    cleanup();
}
