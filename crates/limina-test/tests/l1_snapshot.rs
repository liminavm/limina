// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M9.1 suspend/resume acceptance test (RED until the whole M9.1 stack lands).
//!
//! A host-side VM snapshot + `--restore` must resume the guest **mid-execution**: its
//! in-guest-RAM counter keeps climbing (never resets toward 0, which would mean a fresh boot)
//! and CLOCK_MONOTONIC never leaps backwards (the restored vtimer/`CNTVOFF` keeps it going).
//! The L1 `limina.counter` guest emits heartbeats over the near-stateless PL011 console, which
//! keeps flowing across a fresh-worker restore — before virtio device-state restore (M9.2).
//!
//! The discriminator is timing-proof: we sample the FIRST heartbeat written *after* the
//! supervisor logs its restore, so a resume shows continuous `n`/`mono_ms` while any
//! reboot-instead-of-resume bug shows both reset to near-zero. See
//! `docs/design/m9-suspend-resume.md` §M9.1 and `docs/design/m9-freeze-trigger.md`.

use std::time::{Duration, Instant};

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

#[test]
fn l1_counter_survives_snapshot_and_restore() {
    if !limina_test::require_hvf_or_skip("l1_counter_survives_snapshot_and_restore") {
        return;
    }

    let mut cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .append_cmdline("limina.counter")
        .with_supervisor_log()
        .with_snapshot();
    // Multi-vCPU from day one: per-vCPU MPIDR/ICC ordering is exactly where the single-vCPU
    // spike stopped, so the acceptance test must exercise it.
    cfg.cpus = 2;

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for("LIMINA_COUNTER_READY", Duration::from_secs(20))
        .expect("counter guest did not reach userspace");

    // Let it climb for a while so the pre-snapshot counter and monotonic clock are clearly
    // non-trivial (a fresh boot would take seconds to fake these values).
    guest
        .wait_for("mono_ms=", Duration::from_secs(5))
        .expect("no counter heartbeat before snapshot");
    std::thread::sleep(Duration::from_secs(2));
    let (pre_n, pre_mono) =
        last_counter(&guest.console()).expect("parsing the pre-snapshot counter heartbeat");
    assert!(pre_n > 0, "counter should be climbing before the snapshot");
    eprintln!("pre-snapshot: n={pre_n} mono_ms={pre_mono}");

    // Trigger the host-side snapshot; the supervisor should relaunch the worker with --restore.
    guest.snapshot().expect("triggering the snapshot");
    guest
        .wait_for_supervisor_log("restoring from snapshot", Duration::from_secs(30))
        .expect("supervisor did not take the --restore path after the snapshot");

    // The snapshot file must have been written.
    let snap = guest.snapshot_path().expect("snapshot path configured");
    assert!(snap.exists(), "snapshot file {snap:?} was not written");

    // Everything from here is unambiguously post-restore: sample the FIRST heartbeat that
    // appears past the console boundary taken after the restore marker.
    let boundary = guest.console().len();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (post_n, post_mono) = loop {
        let console = guest.console();
        if console.len() > boundary {
            if let Some(hb) = console[boundary..].lines().find_map(parse_counter) {
                break hb;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no counter heartbeat after restore — guest did not resume"
        );
        std::thread::sleep(Duration::from_millis(150));
    };
    eprintln!("post-restore: n={post_n} mono_ms={post_mono}");

    // The counter continued (it did not reset to a fresh boot).
    assert!(
        post_n >= pre_n,
        "counter reset after restore (fresh boot, not a resume): pre={pre_n} post={post_n}"
    );
    // CLOCK_MONOTONIC continued forward — the restored vtimer/CNTVOFF never steps it backwards.
    assert!(
        post_mono >= pre_mono,
        "monotonic clock went backwards across restore: pre={pre_mono}ms post={post_mono}ms"
    );

    let _ = guest.shutdown(Duration::from_secs(10));
}
