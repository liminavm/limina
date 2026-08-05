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
//! stable across legs). No SSH: the suspend is the bracket's own button pulse (the
//! seated GNOME session honors the suspend key), and the oracles are the supervisor
//! log + on-disk snapshot artifacts.

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
    cfg.with_supervisor_log()
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

    // --- Leg 1: boot flagless-flat, then suspend via the production bracket ---
    let mut g1 = Guest::boot(&flat_cfg(&disk)).expect("booting the flat guest");
    // Seated GNOME needs to be up before the suspend button means anything; the boot
    // itself is the wait (EFI + autologin). Give the session time to settle.
    g1.wait_for_supervisor_log("suspend armed by default", Duration::from_secs(60))
        .expect("flat run did not default-arm suspend (task #20)");
    std::thread::sleep(Duration::from_secs(90));

    // Suspend through the FULL CLI path (task #20 part 3): `limina suspend <disk>` finds the
    // flat supervisor by its argv (pgrep), relays SIGTSTP, and waits for the snapshot. This
    // covers supervisor discovery + relay + bracket in one go (the harness's suspend_bracket
    // signals the worker directly, skipping the first two).
    let base_for_bin = GuestConfig::baseline_fedora_from_env().expect("baseline config");
    let cli = std::process::Command::new(&base_for_bin.limina_bin)
        .arg("suspend")
        .arg(&disk)
        .output()
        .expect("running limina suspend");
    if !cli.status.success() {
        let log = g1.supervisor_log();
        cleanup();
        panic!(
            "`limina suspend {}` failed: {}\nstderr: {}\nsupervisor log:\n{log}",
            disk.display(),
            cli.status,
            String::from_utf8_lossy(&cli.stderr)
        );
    }
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(120))
        .expect("supervisor did not exit after the suspend bracket");
    if outcome.code != Some(126) {
        let log = g1.supervisor_log();
        cleanup();
        panic!("flat suspend failed (exit {outcome:?});\n{log}");
    }
    drop(g1);
    assert!(
        snap.exists(),
        "suspend must leave the snapshot at the disk-derived path {}",
        snap.display()
    );

    // --- Leg 2: identical flagless relaunch must RESUME, not cold-boot ---
    let mut g2 = Guest::boot(&flat_cfg(&disk)).expect("relaunching the flat guest");
    let resumed = g2
        .wait_for_supervisor_log("restoring from snapshot", Duration::from_secs(30))
        .is_ok();
    if !resumed {
        let log = g2.supervisor_log();
        cleanup();
        panic!(
            "the relaunch cold-booted past a pending resume (no 'restoring from snapshot'):\n{log}"
        );
    }
    // Single-use: the canonical snapshot is consumed (renamed) the moment the resume starts.
    assert!(
        !snap.exists(),
        "the consumed snapshot must leave its canonical path (single-use invariant)"
    );
    let _ = g2.shutdown(Duration::from_secs(30));

    // --- Leg 3: a pending resume + --discard-suspend must COLD boot and delete it ---
    std::fs::remove_file(&disk).expect("removing the leg-2 disk");
    limina_test::cow_clone(&golden, &disk).expect("re-cloning for the discard leg");
    std::fs::write(&snap, b"stale-but-present").expect("planting a pending snapshot");
    let cfg3 = flat_cfg(&disk).with_supervisor_arg("--discard-suspend");
    let g3 = Guest::boot(&cfg3).expect("booting with --discard-suspend");
    // The discard happens before the worker spawns; the snapshot must be gone and the
    // boot must NOT try to restore.
    std::thread::sleep(Duration::from_secs(20));
    let log = g3.supervisor_log();
    let discarded = !snap.exists() && !log.contains("restoring from snapshot");
    if !discarded {
        cleanup();
        panic!(
            "--discard-suspend must delete the pending snapshot and cold-boot \
             (snap.exists()={}, log:\n{})",
            snap.exists(),
            log
        );
    }
    let _ = g3.shutdown(Duration::from_secs(30));
    cleanup();
}
