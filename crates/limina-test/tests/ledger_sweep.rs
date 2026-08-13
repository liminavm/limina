// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The ledger settle sweep (spikes/hv-ledger-marker → libkrun `ReleasedRam::settle_sweep`):
//! xnu bills phys_footprint once per pmap, so every disk-fed guest page — written by the
//! worker through its task mapping, then read by the guest through stage-2 — bills twice,
//! and Activity Monitor shows up to 2× the VM's real memory. The sweep debits the task
//! share with a chunked `mprotect(PROT_NONE → RW)` cycle over live guest RAM; the guest
//! never notices (stage-2 is untouched, and HVF populates stage-2 in-kernel regardless of
//! the task mapping's protection).
//!
//! This test drives the whole production path: build a both-touched population with guest
//! disk reads, command `settle` on the balloon control socket, and assert
//! (a) the worker's phys_footprint actually falls by an unambiguous amount,
//! (b) the sweep counters ride the stats surface, and
//! (c) the guest comes through unharmed — proven by content integrity over cached (= swept)
//! pages while a concurrent dd keeps kernel copyio landing in guest RAM during the sweep
//! windows (the EFAULT-retry path), plus a from-disk re-read afterwards.
//!
//! RED without the mechanism: `sweeps` never advances (unknown control verb) and the
//! footprint stays at peak. SKIPs cleanly without `LIMINA_HVF_TESTS` or the baseline disk.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;

/// Size of the guest blob that builds the both-touched population. The guest writes it
/// (guest-touch) and virtio-blk write-back pwritevs it out of guest RAM (host-touch —
/// copyio populates task PTEs regardless of direction), so on a 6 GiB guest with nothing
/// evicting, this much page cache double-bills against the worker.
const BLOB_BYTES: u64 = 1536 * MIB;
/// The sweep must debit at least this much off the worker footprint (its own before/after
/// measurement). The both-touched share after boot + the blob is well above the blob size;
/// half of it is unambiguous versus noise while tolerating compressor-settled slices.
const DEBIT_MIN: u64 = 768 * MIB;
/// The externally observed footprint must fall by at least this much across the sweep.
/// Smaller than [`DEBIT_MIN`]: the concurrent dd loop re-bills its (buffer-bounded) pages
/// between the sweep's own measurement and ours.
const DROP_MIN: u64 = 500 * MIB;

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

#[test]
fn settle_sweep_debits_the_task_ledger_and_leaves_the_guest_unharmed() {
    if !limina_test::require_hvf_or_skip(
        "settle_sweep_debits_the_task_ledger_and_leaves_the_guest_unharmed",
    ) {
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = 6144; // room for the blob without cache eviction
            cfg.with_net().with_balloon_control()
        }
        Err(e) => {
            eprintln!(
                "SKIPPED settle_sweep_debits_the_task_ledger_and_leaves_the_guest_unharmed: {e}"
            );
            return;
        }
    };
    eprintln!(
        "booting stock 4 KiB F44 baseline (headless, NAT) for a {} MiB both-touch + settle sweep",
        mib(BLOB_BYTES)
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // Build the both-touched population: the guest writes a blob (guest-touch) and fsync
    // forces virtio-blk write-back, whose pwritev reads the pages out of guest RAM through
    // the worker's task mapping (host-touch) — the production double-billing shape. The
    // pages stay in the guest's page cache; the checksum doubles as the content oracle.
    let blob_mib = mib(BLOB_BYTES);
    guest
        .ssh_exec(&format!(
            "dd if=/dev/urandom of=$HOME/blob bs=1M count={blob_mib} conv=fsync 2>/dev/null \
             && echo made"
        ))
        .expect("creating the guest blob");
    let cksum_before = guest
        .ssh_exec("cksum $HOME/blob")
        .expect("checksumming the guest blob");
    let cksum_before = cksum_before.trim().to_string();
    eprintln!("blob checksum (pre-sweep): {cksum_before}");

    let f1 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");
    let s0 = guest.balloon_stats().expect("reading balloon stats");
    eprintln!(
        "worker phys_footprint at both-touched peak: {} MiB (sweeps so far: {})",
        mib(f1),
        s0.sweeps
    );

    // Concurrent IO during the sweep: a detached O_DIRECT read loop over the blob keeps
    // kernel copyio landing in guest RAM (dd's user buffer, cache bypassed — real virtio-blk
    // reads every pass) while the windows flip, exercising the EFAULT-retry path live. Only
    // dd's reused 1 MiB buffer re-bills, so it can't mask the sweep's debit.
    let dd_pid = guest
        .ssh_exec(
            "nohup sh -c 'while :; do dd if=$HOME/blob of=/dev/null bs=1M iflag=direct \
             2>/dev/null; done' >/dev/null 2>&1 </dev/null & echo $!",
        )
        .expect("launching the concurrent dd loop");
    let dd_pid = dd_pid.trim().to_string();

    guest.settle_sweep().expect("sending the settle command");
    let deadline = Instant::now() + Duration::from_secs(30);
    let stats = loop {
        std::thread::sleep(Duration::from_secs(1));
        let s = guest.balloon_stats().expect("reading balloon stats");
        if s.sweeps > s0.sweeps {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "the settle sweep never completed (sweeps still {} after 30 s) — the worker \
             either doesn't know the verb or the sweep wedged",
            s.sweeps
        );
    };
    eprintln!(
        "sweep #{} done: debited {} MiB in {} ms ({} worker touches fielded in-window)",
        stats.sweeps,
        mib(stats.sweep_debited),
        stats.sweep_ms,
        stats.sweep_faults
    );
    assert!(
        stats.sweep_debited >= DEBIT_MIN,
        "the sweep only debited {} MiB (want >= {} MiB) — the task-pmap share of the \
         both-touched population was not settled",
        mib(stats.sweep_debited),
        mib(DEBIT_MIN)
    );

    let f2 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");
    eprintln!(
        "worker phys_footprint after the sweep: {} MiB (fell {} MiB)",
        mib(f2),
        mib(f1.saturating_sub(f2))
    );
    assert!(
        f1.saturating_sub(f2) >= DROP_MIN,
        "the externally observed footprint only fell {} MiB across the sweep (want >= {} \
         MiB) — the debit didn't reach the ledger Activity Monitor reads",
        mib(f1.saturating_sub(f2)),
        mib(DROP_MIN)
    );

    // The guest must be unharmed. First: the pages the sweep flipped are exactly the cached
    // ones — re-checksum the blob from cache, with the dd loop still running.
    let cksum_cached = guest
        .ssh_exec("cksum $HOME/blob")
        .expect("the post-sweep cached checksum failed — the guest is wedged or the read errored");
    assert_eq!(
        cksum_before,
        cksum_cached.trim(),
        "cached content CHANGED across the sweep — a window zero-filled or tore a guest page"
    );

    // Stop the dd loop, then prove the disk path end-to-end: drop caches and re-read the blob
    // from disk (virtio-blk through the EFAULT-retry sites, post-sweep lazy re-population).
    guest
        .ssh_exec(&format!("kill {dd_pid} 2>/dev/null; echo stopped"))
        .expect("stopping the dd loop");
    let cksum_disk = guest
        .ssh_exec("sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null; cksum $HOME/blob")
        .expect("the from-disk re-read failed");
    assert_eq!(
        cksum_before,
        cksum_disk.trim(),
        "on-disk content CHANGED across the sweep — an EFAULT window corrupted a disk write"
    );
    eprintln!("guest content verified across the sweep (cached and from-disk)");

    // No collateral in the release machinery: sweeping live RAM must not have upset the
    // released-range bookkeeping.
    let s_end = guest.balloon_stats().expect("reading balloon stats");
    assert_eq!(
        s_end.strays, 0,
        "stray stage-2 faults appeared across the sweep — the sweep disturbed the guest's \
         stage-2 mappings"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
