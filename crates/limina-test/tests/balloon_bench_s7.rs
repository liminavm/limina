// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S7 — the `Out of puff` retry loop in isolation**
//! (`docs/design/balloon-bench.md` §2, §6). No policy — targets go straight to the
//! worker's socket, so every observation is the *driver's* behavior.
//!
//! Four phases:
//! - **A (H1 stage-set):** fill the page cache (write + sync a file sized near RAM) so the
//!   guest reaches the H1 shape — MemFree LOW, MemAvailable HIGH.
//! - **B (slow fill):** command a 2 GiB target into that shape. `balloon_page_alloc` is
//!   `__GFP_NORETRY`: it can nibble cache via light direct reclaim but fails easily — this
//!   measures the *fill rate against cache* vs S1's ~1.8 GiB/s against free pages, and how
//!   much `Out of puff` a mere ramp produces (H2's magnitude).
//! - **C (chase):** escalate the target past whatever the driver manages to fill (adaptive
//!   — a fixed guess either OOMs guest daemons or turns out fillable via zram), so
//!   fill_balloon runs its 5 Hz retry loop against the guest's natural ceiling for the
//!   whole hold: journal line rate, kswapd/direct-reclaim cost, plateau level. This is the
//!   **journal channel's positive control** — the run FAILS if no `Out of puff` line
//!   appears, because that means the pipeline (not the driver) is broken.
//! - **D (silence):** re-command exactly the plateau `actual`. The gap closes to zero and
//!   the retry loop must stop: at most a ratelimit straggler in the window. Proves the
//!   fix direction H1 implies (close the gap → the spam stops) at the mechanism level.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF.

use std::time::Duration;

use limina_test::bench::{
    fetch_balloon_journal, guest_epoch_secs, json_object, mib_per_s, now_ms, sample_host, BenchRun,
    GuestSampler, HostSample,
};
use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;
/// 4 GiB, not 6: the baseline image's /var/tmp holds ~4.4 GiB, and the H1 shape needs
/// the cache file to approach RAM minus the working set — a smaller guest makes the
/// fill achievable within the disk budget.
const RAM_MIB: usize = 4096;
/// Phase-A cache file: sized to push MemFree low. On DISK (/var/tmp) — /tmp is a
/// RAM-backed tmpfs sized RAM/2: too small, and tmpfs pages aren't reclaimable cache, so
/// they can't produce the H1 shape at all (the S3 tmpfs incident).
const CACHE_FILE_MIB: u64 = 3072;
const CACHE_FILE: &str = "/var/tmp/limina-cachefill";
/// Phase-B target: should be fillable by nibbling cache, slowly.
const SLOW_FILL_TARGET: u64 = 2048 * MIB;
/// Phase-C escalation: start here and step up whenever the driver closes the gap, so the
/// run finds the guest's NATURAL plateau instead of guessing it (a fixed target either
/// overshoots into OOM-killing guest daemons or undershoots into a fillable target).
const CHASE_START: u64 = 3072 * MIB;
const CHASE_STEP: u64 = 256 * MIB;
const CHASE_CAP: u64 = (RAM_MIB as u64 - 512) * MIB;

/// Count `Out of puff` journal lines since a guest-epoch watermark.
fn puff_since(guest: &Guest, mark: &str) -> u64 {
    guest
        .ssh_exec(&format!(
            "sudo journalctl -k --since=@{mark} 2>/dev/null | grep -c 'Out of puff' || true"
        ))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Sample the host at ~200 ms for `secs`, appending to `samples`; returns the last sample.
fn observe(guest: &Guest, samples: &mut Vec<HostSample>, secs: u64) -> HostSample {
    let mut last = HostSample::default();
    for _ in 0..(secs * 5) {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(guest) {
            last = s;
            samples.push(s);
        }
    }
    last
}

#[test]
fn s7_out_of_puff_chase() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s7_out_of_puff_chase: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip("s7_out_of_puff_chase") {
        return;
    }
    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = RAM_MIB;
            cfg.with_net().with_balloon_control()
        }
        Err(e) => {
            eprintln!("SKIPPED s7_out_of_puff_chase: {e}");
            return;
        }
    };
    let run = BenchRun::create("s7").expect("creating the bench run dir");
    eprintln!(
        "S7 out-of-puff chase (no policy); artifacts -> {:?}",
        run.dir()
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    std::thread::sleep(Duration::from_secs(5));
    let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
    let mut host_samples: Vec<HostSample> = Vec::new();

    // ---- Phase A: the H1 stage-set (MemFree low, MemAvailable high). --------------------
    eprintln!("phase A: filling the page cache ({CACHE_FILE_MIB} MiB file on disk)");
    let df_avail_mib: u64 = guest
        .ssh_exec("df -m --output=avail /var/tmp | tail -1")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    assert!(
        df_avail_mib > CACHE_FILE_MIB + 1024,
        "guest disk too small for the cache-fill file ({df_avail_mib} MiB avail on /var/tmp)"
    );
    // Write, then read back: the write path caches the pages, the read-back re-warms
    // anything writeback pressure evicted — the point is a page-cache-full guest.
    guest
        .ssh_exec(&format!(
            "dd if=/dev/zero of={CACHE_FILE} bs=1M count={CACHE_FILE_MIB} 2>/dev/null; sync; \
             cat {CACHE_FILE} > /dev/null"
        ))
        .expect("cache-fill dd + read-back");
    std::thread::sleep(Duration::from_secs(3));
    let shape = guest
        .ssh_exec(
            "awk '/MemTotal|MemFree|MemAvailable|^Cached/{printf \"%s%s \", $1, $2}' /proc/meminfo",
        )
        .unwrap_or_default();
    eprintln!("  H1 shape: {}", shape.trim());
    let free_kib: u64 = guest
        .ssh_exec("awk '/MemFree/{print $2}' /proc/meminfo")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let avail_kib: u64 = guest
        .ssh_exec("awk '/MemAvailable/{print $2}' /proc/meminfo")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    // ---- Phase B: slow fill against cache. ----------------------------------------------
    let mark_b = guest_epoch_secs(&guest).expect("guest clock");
    let t_b = now_ms();
    guest
        .set_balloon_target(SLOW_FILL_TARGET)
        .expect("commanding the slow-fill target");
    eprintln!("phase B: 2 GiB target into the cache-full guest (60 s observation)");
    let b_last = observe(&guest, &mut host_samples, 60);
    let puff_b = puff_since(&guest, &mark_b);
    let b_fill_rate = mib_per_s(
        b_last.actual,
        Duration::from_millis(
            // Time to reach the last observed actual: use the sample timestamp.
            b_last.ts_ms.saturating_sub(t_b).max(1),
        ),
    );
    eprintln!(
        "  phase B: actual={} MiB after 60 s (~{:.0} MiB/s vs S1's ~1840), puff lines={}",
        b_last.actual / MIB,
        b_fill_rate,
        puff_b
    );

    // ---- Phase C: chase the driver past its plateau — the permanent retry loop. ---------
    // Adaptive: whenever the driver closes the gap, raise the target one step (capped),
    // so the loop runs against the guest's NATURAL allocation ceiling for the whole hold.
    let mark_c = guest_epoch_secs(&guest).expect("guest clock");
    let mut chase_target = CHASE_START;
    guest
        .set_balloon_target(chase_target)
        .expect("commanding the chase target");
    eprintln!(
        "phase C: chasing from {} MiB (step {} MiB, cap {} MiB), 90 s hold",
        CHASE_START / MIB,
        CHASE_STEP / MIB,
        CHASE_CAP / MIB
    );
    let mut c_last = HostSample::default();
    for _ in 0..(90 * 5) {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(&guest) {
            c_last = s;
            host_samples.push(s);
            if chase_target.saturating_sub(s.actual) < 64 * MIB && chase_target < CHASE_CAP {
                chase_target = (chase_target + CHASE_STEP).min(CHASE_CAP);
                guest.set_balloon_target(chase_target).ok();
                eprintln!(
                    "  gap closed — escalating target to {} MiB",
                    chase_target / MIB
                );
            }
        }
    }
    let puff_c = puff_since(&guest, &mark_c);
    eprintln!(
        "  phase C: plateau actual={} MiB (final target {} MiB), puff lines={} in 90 s",
        c_last.actual / MIB,
        chase_target / MIB,
        puff_c
    );

    // ---- Phase D: close the gap, the loop must stop. ------------------------------------
    let plateau = c_last.actual;
    let mark_d = guest_epoch_secs(&guest).expect("guest clock");
    guest
        .set_balloon_target(plateau)
        .expect("re-commanding the plateau actual");
    eprintln!(
        "phase D: target = actual ({} MiB), 30 s silence check",
        plateau / MIB
    );
    observe(&guest, &mut host_samples, 30);
    let puff_d = puff_since(&guest, &mark_d);
    eprintln!("  phase D: puff lines={puff_d} (want ≤1 ratelimit straggler)");

    // Deflate and collect.
    guest.set_balloon_target(0).ok();
    std::thread::sleep(Duration::from_secs(5));
    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write("guest.csv", &guest_csv).unwrap();
    run.write_host_samples(&host_samples).unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write("journal.txt", &journal).unwrap();

    let kswapd_delta = {
        let first = guest_samples
            .first()
            .map(|s| s.kswapd_cpu_ticks)
            .unwrap_or(0);
        let last = guest_samples
            .last()
            .map(|s| s.kswapd_cpu_ticks)
            .unwrap_or(0);
        last.saturating_sub(first)
    };
    let metrics = json_object(&[
        ("scenario", "\"s7-out-of-puff\"".to_string()),
        ("h1_free_kib", free_kib.to_string()),
        ("h1_avail_kib", avail_kib.to_string()),
        ("slow_fill_target", SLOW_FILL_TARGET.to_string()),
        ("slow_fill_actual_60s", b_last.actual.to_string()),
        ("slow_fill_mib_s", format!("{b_fill_rate:.1}")),
        ("puff_lines_b", puff_b.to_string()),
        ("chase_final_target", chase_target.to_string()),
        ("plateau_actual", plateau.to_string()),
        ("puff_lines_c_90s", puff_c.to_string()),
        ("puff_lines_d_30s", puff_d.to_string()),
        ("kswapd_cpu_ticks_delta", kswapd_delta.to_string()),
        ("guest_samples", guest_samples.len().to_string()),
    ]);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S7 metrics ==\n{metrics}");

    // The journal channel's positive control: an unfillable target MUST produce the spam.
    assert!(
        puff_c > 0,
        "phase C held an unfillable target for 90 s with ZERO 'Out of puff' journal lines — \
         either the journal pipeline is broken (grep/sudo/journalctl) or the driver model \
         in docs/design/balloon-bench.md §2 is wrong; both need investigating before any \
         journal-based conclusion is trusted"
    );
    // Closing the gap must stop the loop (ratelimit may land one straggler).
    assert!(
        puff_d <= 1,
        "phase D closed the target/actual gap but the retry loop kept logging \
         ({puff_d} lines in 30 s) — the §2 requeue model is wrong"
    );
    // The H1 stage-set must actually have been the H1 shape, or phase B measured nothing.
    assert!(
        free_kib < 1024 * 1024 && avail_kib > 5 * 512 * 1024,
        "phase A did not produce the H1 shape (free={free_kib} KiB, avail={avail_kib} KiB) — \
         phase B's fill-vs-cache number is not measuring fill-vs-cache"
    );

    let outcome = guest.shutdown(Duration::from_secs(15));
    eprintln!("teardown: {outcome:?}");
}
