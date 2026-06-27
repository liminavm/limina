// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M6 dynamic-memory: a host-set balloon **target** drives the stock guest to inflate (give up
//! memory) and deflate (take it back) — the target/`actual` loop end-to-end.
//!
//! Gates M6 Step 2 (libkrun patch 0034) + the control-socket plumbing (Step 3): the worker binds a
//! `--balloon-control-socket`; the harness writes `target <bytes>`; the device sets `num_pages` and
//! raises a config-change interrupt; the guest balloon driver inflates/deflates and writes back
//! `actual`; the worker publishes `actual` for the host to read via `stats`.
//!
//! The decisive, vehicle-independent assertion is **`actual` reaching the target** (and returning to
//! ~0 on deflate): it proves the whole loop host-side without depending on 16 KiB-host coalescing
//! (inflate on a stock 4 KiB guest hands scattered 4 KiB pages, so host `phys_footprint` reclaim is
//! best-effort there — the enhanced 16 KiB tier is what makes inflate 1:1; FRQ in `balloon.rs` is
//! what reclaims well on 4 KiB). We log the guest `MemAvailable` and worker footprint deltas as
//! secondary observations.
//!
//! SKIPs cleanly without `LIMINA_HVF_TESTS`, the GOP firmware, or the baseline disk. Heavy: a full
//! stock boot to sshd.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;
/// Target balloon size to drive the guest to.
const TARGET_BYTES: u64 = 1024 * MIB;
/// `actual` must reach at least this fraction of the target to count as inflated.
const INFLATE_FLOOR: u64 = 900 * MIB;
/// After deflate (target 0), `actual` must fall back to at most this.
const DEFLATE_CEIL: u64 = 100 * MIB;

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

/// Poll `balloon_stats().0` (actual bytes) until `pred` holds or the timeout elapses; returns the
/// last reading.
fn poll_actual(guest: &Guest, timeout: Duration, pred: impl Fn(u64) -> bool) -> u64 {
    let deadline = Instant::now() + timeout;
    loop {
        let (actual, reclaimed) = guest.balloon_stats().expect("reading balloon stats");
        eprintln!(
            "  balloon actual={} MiB (reclaimed {} MiB)",
            mib(actual),
            mib(reclaimed)
        );
        if pred(actual) || Instant::now() >= deadline {
            return actual;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn mem_available_kib(guest: &Guest) -> u64 {
    guest
        .ssh_exec("awk '/MemAvailable/{print $2}' /proc/meminfo")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[test]
fn host_target_inflates_and_deflates_the_guest_balloon() {
    if !limina_test::require_hvf_or_skip("host_target_inflates_and_deflates_the_guest_balloon") {
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = 6144;
            cfg.with_net().with_balloon_control()
        }
        Err(e) => {
            eprintln!("SKIPPED host_target_inflates_and_deflates_the_guest_balloon: {e}");
            return;
        }
    };
    eprintln!("booting stock 4 KiB F44 baseline (headless, NAT, balloon control)");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    let avail0 = mem_available_kib(&guest);
    let foot0 = guest.worker_phys_footprint().unwrap_or(0);
    let (actual0, _) = guest.balloon_stats().expect("initial balloon stats");
    eprintln!(
        "baseline: balloon actual={} MiB, guest MemAvailable={} MiB, worker footprint={} MiB",
        mib(actual0),
        avail0 / 1024,
        mib(foot0)
    );

    // INFLATE: drive the target up; the guest should inflate toward it.
    eprintln!("setting balloon target to {} MiB", mib(TARGET_BYTES));
    guest
        .set_balloon_target(TARGET_BYTES)
        .expect("setting balloon target");
    let inflated = poll_actual(&guest, Duration::from_secs(45), |a| a >= INFLATE_FLOOR);
    let avail1 = mem_available_kib(&guest);
    let foot1 = guest.worker_phys_footprint().unwrap_or(0);
    eprintln!(
        "after inflate: actual={} MiB, guest MemAvailable={} MiB (Δ {} MiB), worker footprint Δ {} MiB",
        mib(inflated),
        avail1 / 1024,
        (avail0.saturating_sub(avail1)) / 1024,
        mib(foot1.saturating_sub(foot0)) // negative deltas saturate to 0; logged best-effort
    );
    assert!(
        inflated >= INFLATE_FLOOR,
        "balloon did not inflate to the target: actual={} MiB, want >= {} MiB of the {} MiB target. \
         The host->guest target loop (num_pages + config-change -> guest inflate -> actual) is broken.",
        mib(inflated),
        mib(INFLATE_FLOOR),
        mib(TARGET_BYTES)
    );
    // The guest gave up ~the balloon's worth of available memory.
    assert!(
        avail0.saturating_sub(avail1) >= (INFLATE_FLOOR / 1024),
        "guest MemAvailable barely moved after inflating {} MiB (Δ {} MiB) — the guest didn't really \
         take the pages out of circulation",
        mib(inflated),
        (avail0.saturating_sub(avail1)) / 1024
    );

    // DEFLATE: drop the target to 0; the guest should return the pages.
    eprintln!("setting balloon target to 0 (deflate)");
    guest
        .set_balloon_target(0)
        .expect("setting balloon target 0");
    let deflated = poll_actual(&guest, Duration::from_secs(45), |a| a <= DEFLATE_CEIL);
    let avail2 = mem_available_kib(&guest);
    eprintln!(
        "after deflate: actual={} MiB, guest MemAvailable={} MiB",
        mib(deflated),
        avail2 / 1024
    );
    assert!(
        deflated <= DEFLATE_CEIL,
        "balloon did not deflate back: actual={} MiB, want <= {} MiB — the deflate path (lowered \
         num_pages -> guest leak -> actual) is broken",
        mib(deflated),
        mib(DEFLATE_CEIL)
    );
    // Memory came back to the guest: available rose by a large margin from the inflated low.
    assert!(
        avail2 >= avail1 + (400 * 1024),
        "guest MemAvailable did not recover after deflate (avail1={} -> avail2={} MiB; expected a \
         rise as the balloon returned ~{} MiB)",
        avail1 / 1024,
        avail2 / 1024,
        mib(TARGET_BYTES)
    );

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
