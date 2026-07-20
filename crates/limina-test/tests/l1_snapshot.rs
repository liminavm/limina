// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M9.1 suspend/resume acceptance tests.
//!
//! M9.1 delivers the host-side snapshot *mechanism*: a running guest is quiesced, its per-vCPU
//! architectural state + the in-kernel GICv3 + guest RAM are captured to one file, and a **fresh**
//! worker with `--restore` reloads all of it and resumes the guest **executing at the exact PC it
//! was snapshotted at**, behind the restored GIC. That is what these tests assert.
//!
//! What M9.1 deliberately does NOT do is restore **device** state (virtio-mmio queues, virtiofs,
//! the block/console backends) — that is M9.2. So a restored guest resumes its vCPUs but wedges the
//! moment it next touches a device (its virtiofs rootfs is dead). The oracle is therefore weakened
//! accordingly: we prove the resume *mechanism* (fresh worker consumes the snapshot; every vCPU
//! comes back live at its saved PC), not end-to-end heartbeat *continuity* — the latter needs M9.2
//! and gets its own test then. See `docs/design/m9-suspend-resume.md` §M9.1.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// Parse `(n, mono_ms)` from a `LIMINA_COUNTER n=<N> mono_ms=<T>` heartbeat line.
fn parse_counter(line: &str) -> Option<(u64, u64)> {
    let field = |key: &str| -> Option<u64> {
        line.split(key)
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    Some((field("n=")?, field("mono_ms=")?))
}

/// The most recent heartbeat in `console`, if any.
fn last_counter(console: &str) -> Option<(u64, u64)> {
    console.lines().rev().find_map(parse_counter)
}

/// M9.1 **save-side** isolation guard (GREEN without restore): a SIGUSR1 suspend trigger must
/// quiesce the running guest, write the `--snapshot-file`, and exit the worker "snapshotted" (126)
/// — the supervisor reports the VM suspended and, unlike a reboot, does NOT relaunch. This proves
/// the capture + exit-disposition half in isolation; the full resume round-trip is
/// [`l1_counter_survives_snapshot_and_restore`].
#[test]
fn l1_snapshot_save_writes_file_and_exits_126() {
    if !limina_test::require_hvf_or_skip("l1_snapshot_save_writes_file_and_exits_126") {
        return;
    }

    let mut cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .append_cmdline("limina.counter")
        .with_supervisor_log()
        .with_snapshot();
    cfg.cpus = 2;
    // A small guest keeps the RAM snapshot (dumped + CRC'd + written) fast; the counter guest
    // needs almost nothing.
    cfg.ram_mib = 512;

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("LIMINA_COUNTER_READY", Duration::from_secs(20))
        .expect("counter guest did not reach userspace");
    guest
        .wait_for("mono_ms=", Duration::from_secs(5))
        .expect("no counter heartbeat before snapshot");

    let snap = guest
        .snapshot_path()
        .expect("snapshot path configured")
        .to_path_buf();
    guest.snapshot().expect("triggering the snapshot");

    // The worker writes the snapshot then exits 126; the supervisor reports the suspend and stops
    // WITHOUT relaunching, so the supervisor process itself propagates exit 126. Wait by-ref so the
    // scratch dir (holding the snapshot file) survives for the assertions below.
    let outcome = match guest.wait_supervisor_exit(Duration::from_secs(30)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("=== supervisor+worker log ===\n{}", guest.supervisor_log());
            panic!("supervisor did not exit after the snapshot trigger: {e}");
        }
    };
    assert_eq!(
        outcome.code,
        Some(126),
        "supervisor should propagate the worker's snapshotted exit (126); got {outcome:?}"
    );

    // The snapshot file was written and is non-trivial (per-vCPU state + GIC blob + guest RAM + CRC
    // — far more than 4 KiB even for the tiny L1 guest).
    let meta = std::fs::metadata(&snap)
        .unwrap_or_else(|e| panic!("snapshot file {snap:?} was not written: {e}"));
    assert!(
        meta.len() > 4096,
        "snapshot file {snap:?} is implausibly small ({} bytes)",
        meta.len()
    );
    // The supervisor recorded the suspend disposition (not a crash / plain stop).
    assert!(
        guest.supervisor_log().contains("suspended"),
        "supervisor log should note the VM was suspended; got:\n{}",
        guest.supervisor_log()
    );
}

/// Parse the resumed PC from a `vCPU <id> resumed from snapshot at pc=0x<hex>` worker log line.
fn parse_resumed_pc(line: &str) -> Option<u64> {
    let hex = line.split("resumed from snapshot at pc=0x").nth(1)?;
    let hex = hex.split_whitespace().next()?;
    u64::from_str_radix(hex, 16).ok()
}

/// M9.1 resume-mechanism acceptance: a snapshot taken from a live, multi-vCPU guest must be
/// reloadable by a **fresh, separate** worker that brings **every** vCPU back live at the exact
/// PC it was snapshotted at, behind the restored in-kernel GIC. This is the weakened oracle: it
/// proves the resume *mechanism* (not device-state continuity, which is M9.2 — see the module
/// doc). Multi-vCPU from day one: per-vCPU MPIDR/ICC ordering behind the GIC is exactly where the
/// single-vCPU spike stopped, so the acceptance test must exercise it.
#[test]
fn l1_guest_resumes_in_fresh_worker_from_snapshot() {
    if !limina_test::require_hvf_or_skip("l1_guest_resumes_in_fresh_worker_from_snapshot") {
        return;
    }

    // Suspend = teardown + resume-on-a-separate-start (NOT an in-process relaunch). So we boot
    // Guest 1, snapshot it (worker exits 126, supervisor stops), then boot a SECOND Guest that
    // restores from that file — modelling the real split and keeping the restore decision external
    // (the harness plays the role production's persisted Suspended{snapshot} status plays).
    let base = || {
        GuestConfig::l1_from_env()
            .expect("resolving L1 guest config")
            .append_cmdline("limina.counter")
    };
    const CPUS: u8 = 2;
    let suspend_cfg = {
        let mut c = base().with_supervisor_log().with_snapshot();
        c.cpus = CPUS;
        c.ram_mib = 512; // small RAM keeps the RAM dump + CRC + write fast
        c
    };

    // --- Guest 1: boot, let the counter climb, snapshot, confirm it suspended (exit 126) ---
    let mut g1 = Guest::boot(&suspend_cfg).expect("spawning the first limina supervisor");
    g1.wait_for("LIMINA_COUNTER_READY", Duration::from_secs(20))
        .expect("counter guest did not reach userspace");
    g1.wait_for("mono_ms=", Duration::from_secs(5))
        .expect("no counter heartbeat before snapshot");
    // Let it climb so the snapshot is taken from a genuinely-running guest (a non-trivial PC), not
    // one still in early boot.
    std::thread::sleep(Duration::from_secs(1));
    let (pre_n, pre_mono) =
        last_counter(&g1.console()).expect("parsing the pre-snapshot counter heartbeat");
    assert!(pre_n > 0, "counter should be climbing before the snapshot");
    eprintln!("pre-snapshot: n={pre_n} mono_ms={pre_mono}");

    g1.snapshot().expect("triggering the snapshot");
    let outcome = g1
        .wait_supervisor_exit(Duration::from_secs(30))
        .expect("supervisor did not exit after the snapshot trigger");
    assert_eq!(
        outcome.code,
        Some(126),
        "first VM should suspend (worker exit 126); got {outcome:?}"
    );

    // Copy the snapshot out of Guest 1's scratch so it survives Guest 1 being dropped, then drop it
    // (suspend teardown is complete).
    let snap_src = g1
        .snapshot_path()
        .expect("snapshot path configured")
        .to_path_buf();
    let snap = std::env::temp_dir().join(format!("limina-m9-roundtrip-{}.bin", std::process::id()));
    std::fs::copy(&snap_src, &snap).expect("copying snapshot out of the suspended VM's scratch");
    drop(g1);
    // Auto-resume consumes the snapshot by renaming it (M9.4 single-use); clean up whichever
    // name survives the test.
    let cleanup = {
        let consumed = snap.with_extension("bin.consumed");
        let snap = snap.clone();
        move || {
            let _ = std::fs::remove_file(&snap);
            let _ = std::fs::remove_file(&consumed);
        }
    };

    // --- Guest 2: a fresh, separate worker that RESTORES from the snapshot ---
    let restore_cfg = {
        let mut c = base().with_supervisor_log().restore_from(&snap);
        c.cpus = CPUS;
        c.ram_mib = 512;
        c
    };
    let mut g2 = Guest::boot(&restore_cfg).expect("spawning the restoring limina supervisor");

    // The fresh worker announces it is taking the restore path (not a fresh boot)...
    g2.wait_for_supervisor_log("restoring from snapshot", Duration::from_secs(20))
        .unwrap_or_else(|e| {
            cleanup();
            panic!("restore worker never entered the restore path: {e}");
        });
    // ...and then every vCPU comes back live at its saved PC behind the restored GIC. Wait for the
    // first "resumed" line (proves ≥1 vCPU restored + the worker is alive), then confirm all CPUS
    // of them landed — the per-vCPU restore the single-vCPU spike couldn't reach.
    g2.wait_for_supervisor_log("resumed from snapshot at pc=", Duration::from_secs(30))
        .unwrap_or_else(|e| {
            cleanup();
            panic!("restored guest never resumed a vCPU: {e}");
        });
    // Both vCPUs' "resumed" lines may not be flushed at the same instant; poll briefly for all of
    // them. (The guest wedges on its dead virtio rootfs right after, so the worker stays alive.)
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let pcs = loop {
        let log = g2.supervisor_log();
        let pcs: Vec<u64> = log.lines().filter_map(parse_resumed_pc).collect();
        if pcs.len() >= CPUS as usize || std::time::Instant::now() >= deadline {
            break pcs;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    cleanup();
    eprintln!("resumed PCs: {pcs:?}");

    // Every vCPU resumed (multi-vCPU per-vCPU + GIC restore worked)...
    assert_eq!(
        pcs.len(),
        CPUS as usize,
        "expected all {CPUS} vCPUs to resume from the snapshot; got {}: {pcs:?}",
        pcs.len()
    );
    // ...at a real saved PC, not a null/zeroed fresh state (which would mean the register file
    // never loaded).
    assert!(
        pcs.iter().all(|&pc| pc != 0),
        "a vCPU resumed at pc=0 — the saved register file did not load: {pcs:?}"
    );

    // Device state is not restored (M9.1 scope), so the rootfs/virtio are dead — accept any outcome.
    let _ = g2.shutdown(Duration::from_secs(10));
}

/// M9.2 **suspend-bracket abort** guard: a guest that CANNOT s2idle-quiesce must never be
/// snapshotted, and must never be left wedged. The L1 counter guest has a **virtiofs rootfs**, whose
/// `virtio_fs_freeze` returns `-EOPNOTSUPP`, so s2idle aborts *inside the guest* — the bracket's
/// quiesce poll (`Vmm::is_quiesced`) never succeeds. This asserts the fail-safe: after the quiesce
/// timeout the bracket wakes the guest and the worker KEEPS RUNNING (never exits 126), and the guest
/// survives (its counter keeps advancing). This is the untested half of the bracket — the abort path.
#[test]
fn l2_suspend_bracket_aborts_when_guest_cannot_quiesce() {
    if !limina_test::require_hvf_or_skip("l2_suspend_bracket_aborts_when_guest_cannot_quiesce") {
        return;
    }

    let mut cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .append_cmdline("limina.counter")
        .with_supervisor_log()
        .with_snapshot(); // arms the SIGTSTP bracket thread (and the raw SIGUSR1 path)
    cfg.cpus = 2;
    cfg.ram_mib = 512;

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("LIMINA_COUNTER_READY", Duration::from_secs(20))
        .expect("counter guest did not reach userspace");
    guest
        .wait_for("mono_ms=", Duration::from_secs(5))
        .expect("no counter heartbeat before the bracket");
    let before = last_counter(&guest.console()).expect("a counter heartbeat before the bracket");

    // Fire the suspend bracket. The virtiofs rootfs makes the guest unable to s2idle-quiesce, so the
    // bracket must poll, time out (~20s in the worker), wake the guest, and keep the worker alive.
    guest
        .suspend_bracket()
        .expect("sending the SIGTSTP suspend bracket");

    // Wait past the worker's 20s quiesce timeout, with margin.
    std::thread::sleep(Duration::from_secs(28));

    // The worker must still be alive — an aborted suspend never exits 126.
    let worker = guest.worker_pid();
    assert!(
        worker.is_ok(),
        "worker should still be running after an aborted suspend (it must NOT exit 126): {worker:?}\n\
         === supervisor+worker log ===\n{}",
        guest.supervisor_log()
    );

    // The worker log must show the bracket aborted (guest didn't quiesce), NOT a completed snapshot.
    let log = guest.supervisor_log();
    assert!(
        log.contains("did not quiesce"),
        "expected the bracket to log an aborted (non-quiesced) suspend; got:\n{log}"
    );
    assert!(
        !log.contains("exiting 126 (suspended)"),
        "the bracket must NOT have snapshotted a non-quiesced guest; got:\n{log}"
    );

    // The guest survived: its counter kept advancing past the pre-bracket baseline.
    guest
        .wait_for("mono_ms=", Duration::from_secs(15))
        .expect("no counter heartbeat after the aborted suspend");
    let after = last_counter(&guest.console()).expect("a counter heartbeat after the bracket");
    assert!(
        after.0 > before.0,
        "the counter must keep advancing after an aborted suspend (before n={}, after n={})",
        before.0,
        after.0
    );

    // RE-ARM guard: an aborted suspend must not disable the bracket for the rest of the worker's
    // life (a one-shot trigger thread silently swallows every later SIGTSTP, so a VM whose first
    // suspend fails can never be suspended again). Fire the bracket a second time — it must run a
    // full second cycle, which aborts the same way since this guest still can't quiesce.
    guest
        .suspend_bracket()
        .expect("sending the second SIGTSTP suspend bracket");
    std::thread::sleep(Duration::from_secs(28));
    let log = guest.supervisor_log();
    assert_eq!(
        log.matches("did not quiesce").count(),
        2,
        "expected a second bracket abort (one 'did not quiesce' per SIGTSTP) — an aborted \
         bracket must re-arm, not die; got:\n{log}"
    );
    assert!(
        guest.worker_pid().is_ok(),
        "worker should still be running after the second aborted suspend"
    );

    let _ = guest.shutdown(Duration::from_secs(10));
}
