// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guest RAM built from owned, ledger-tagged memory objects (docs/design/guest-ram-objects.md):
//! a page both the worker and the guest touch is billed to the worker's `phys_footprint` once.
//!
//! On anonymous guest RAM xnu bills it once per pmap, so a disk-fed page (the guest writes it,
//! virtio-blk's pwritev reads it out of guest RAM through the worker's task mapping) bills twice
//! (spikes/hv-ledger-marker). This test builds that population on a stock guest, which sends no
//! pressure reports and so never gets a settle sweep, and asserts the worker's footprint grew by
//! about what the guest itself now uses, not by twice that.
//!
//! RED on anonymous RAM: the footprint grows by about twice the blob.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;

/// Guest data written and fsynced, so it is both guest- and worker-touched.
const BLOB_BYTES: u64 = 1536 * MIB;
/// How far the worker's footprint growth may exceed the guest's own growth in used memory. Two
/// bills of the blob would exceed it by about the blob; one bill by worker noise.
const EXCESS_MAX: u64 = BLOB_BYTES / 4;

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

/// The guest's used memory (MemTotal - MemFree), in bytes.
fn guest_used(guest: &Guest) -> u64 {
    let out = guest
        .ssh_exec(
            "awk '/^MemTotal:/ {t=$2} /^MemFree:/ {f=$2} END {print (t-f)*1024}' /proc/meminfo",
        )
        .expect("reading the guest's meminfo");
    out.trim().parse().expect("meminfo gave a number")
}

#[test]
fn a_page_both_worker_and_guest_touch_bills_once() {
    if !limina_test::require_hvf_or_skip("a_page_both_worker_and_guest_touch_bills_once") {
        return;
    }
    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = 6144; // room for the blob without cache eviction
            cfg.with_net().with_env("LIMINA_GUEST_RAM", "objects")
        }
        Err(e) => {
            eprintln!("SKIPPED a_page_both_worker_and_guest_touch_bills_once: {e}");
            return;
        }
    };
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through the forward");

    let fp0 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");
    let used0 = guest_used(&guest);
    guest
        .ssh_exec(&format!(
            "dd if=/dev/urandom of=$HOME/blob bs=1M count={} conv=fsync 2>/dev/null && echo made",
            mib(BLOB_BYTES)
        ))
        .expect("creating the guest blob");
    let fp1 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");
    let used1 = guest_used(&guest);

    let grew = fp1.saturating_sub(fp0);
    let guest_grew = used1.saturating_sub(used0);
    let excess = grew.saturating_sub(guest_grew);
    eprintln!(
        "worker footprint grew {} MiB, guest used grew {} MiB: {} MiB billed beyond the guest's \
         own use (allowed {} MiB)",
        mib(grew),
        mib(guest_grew),
        mib(excess),
        mib(EXCESS_MAX)
    );
    assert!(
        guest_grew >= BLOB_BYTES / 2,
        "the guest's used memory grew only {} MiB for a {} MiB blob; the page cache did not keep it",
        mib(guest_grew),
        mib(BLOB_BYTES)
    );
    assert!(
        excess <= EXCESS_MAX,
        "the worker billed {} MiB beyond what the guest uses: pages both sides touched are \
         billed twice",
        mib(excess)
    );

    guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
}
