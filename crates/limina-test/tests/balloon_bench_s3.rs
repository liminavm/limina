// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S3 — cache starvation** (`docs/design/balloon-bench.md` §6): the
//! 2026-07-09 sticky-wedge class, reproduced as a workload. Under moderate, the policy
//! converges on an idle guest (leaving the ~1 GiB cache allowance), then the guest
//! re-reads a file working set **larger than the allowance** in a loop: every pass misses
//! cache, io-PSI climbs while memory-PSI stays quiet — the shape the old policy held
//! forever.
//!
//! What it measures: io-PSI integral and pass throughput under the squeeze, the policy's
//! trajectory (below-allowance give-back / starvation release — the wedge-stays-dead
//! check), and the same workload on `disabled` as the denominator (the file fits RAM
//! without a balloon: passes run at memory speed — the contrast IS the cache cost, the
//! Run D number reproduced in vivo).
//!
//! The third point (`warn-dug`) is the pressure give-back vehicle: since the free-elasticity
//! gate (2026-08-11) a Normal-converge no longer digs the cache, so the squeeze S3 was
//! built around only forms under host pressure. This point converges under *injected Warn*
//! (the `LIMINA_HOST_PRESSURE=@file` seam), flips the host back to Normal, then thrashes —
//! the state a real guest is left in after a host-pressure episode. Baseline (2026-08-12,
//! pre-give-back): the dug 4 GiB target held for the whole window at 5× pass cost (613 vs
//! 118 ms) with io-full peaking 2% and memory-some ~7% — the starvation attribution lands
//! on the MEMORY side on host-cached-fast storage, which is why the give-back triggers on
//! io-full OR sustained memory-some (avg60). Pre-registered for the give-back run: ~16
//! `giveback` decisions walk the 4 GiB down, tail passes converge toward the `disabled`
//! denominator, and the avg60 decay tail (~60 s half-life) keeps deflating past the
//! comfort point — an end target near 0–1 GiB is EXPECTED behavior, not over-deflation;
//! the healthy `moderate` point stays at zero give-backs (its memory PSI is flat 0).
//!
//! Escalating-step grade (2026-08-12): 6 give-backs (256M/512M/1G×3/rest), walk-down
//! 32 s → 12 s, passes 457 → 529, median 207 → 192 ms. The 192 is this vehicle's
//! post-episode FLOOR, not a convergence shortfall: the fully recovered phase (cache
//! re-warmed by ~35 s in, kswapd idle, io-some <1%, heals 0) itself runs at ~192 vs the
//! pristine 118 — a persistent ~1.6× warm-read tax after deep reclaim, cause
//! unidentified (leading hypothesis: page-cache folio-order collapse). No deflate
//! pacing can move the median below it, so the give-back convergence thread ends here.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF.

use std::time::{Duration, Instant};

use limina_proto::Message;
use limina_test::bench::{
    fetch_balloon_journal, json_object, now_ms, parse_trace, psi_integral_pct_s, real_report,
    sample_host, BenchRun, GuestSampler, HostSample,
};
use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
/// The re-read working set: 3 GiB — far above moderate's ~1 GiB cache allowance, well
/// under RAM when no balloon holds it.
const FILE_MIB: u64 = 3072;
/// How long the thrash loop runs per point.
const THRASH_SECS: u64 = 120;

// /var/tmp, NOT /tmp: Fedora's /tmp is a RAM-backed tmpfs sized RAM/2 — a 3 GiB file
// fills it, the workload's writes ENOSPC, and worse, tmpfs pages are not reclaimable
// page cache, so the scenario would not even exercise what it claims to.
const THRASH_OUT: &str = "/var/tmp/limina-thrash.out";
const THRASH_BIN: &str = "/var/tmp/limina-thrash.bin";

/// Stage + start the re-read loop (one line per completed pass: `pass <ms>`).
fn start_thrash(guest: &Guest) -> u64 {
    guest
        .ssh_exec(&format!(
            "rm -f {THRASH_OUT}; setsid nohup bash -c 'while true; do cat {THRASH_BIN} > /dev/null; echo pass $(date +%s%3N) >> {THRASH_OUT}; done' </dev/null >/dev/null 2>&1 & echo spawned"
        ))
        .expect("spawning the thrash loop");
    now_ms()
}

fn stop_thrash(guest: &Guest) {
    // Bracketed patterns: a bare pattern matches the ssh shell whose own command line
    // carries it (the balloon_burst pgrep lesson, in pkill form).
    guest
        .ssh_exec(
            "pkill -f '[l]imina-thrash.bin' || true; pkill -f '[c]at /var/tmp/limina-thrash' || true",
        )
        .ok();
}

/// Completed-pass timestamps (ms) from the thrash log.
fn pass_times(guest: &Guest) -> Vec<u64> {
    guest
        .ssh_exec(&format!("cat {THRASH_OUT} 2>/dev/null"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("pass "))
        .filter_map(|v| v.trim().parse().ok())
        .collect()
}

struct PointResult {
    json: String,
}

/// Write the injected host-level file ATOMICALLY (temp + rename) — a plain fs::write
/// truncates first, and a policy sample in that window reads empty → pinned Normal
/// (the S6 mid-staircase release incident).
fn write_level(path: &std::path::Path, level: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{level}\n")).expect("writing the host-level temp file");
    std::fs::rename(&tmp, path).expect("renaming the host-level file into place");
}

/// One S3 point: boot with reclaim `mode`, optionally converge the policy (under injected
/// Warn for `dig_under_warn`, flipping back to Normal before the workload), thrash, measure.
fn run_point(run: &BenchRun, label: &str, mode: &str, dig_under_warn: bool) -> PointResult {
    let trace_path = run.dir().join(format!("trace-{label}.jsonl"));
    eprintln!("== S3 point: {label} (mode={mode}) ==");
    let mut cfg = GuestConfig::baseline_fedora_from_env()
        .expect("baseline disk (checked before)")
        .with_net()
        .with_memory(MIN_MIB, MAX_MIB)
        .with_balloon_control()
        .with_control_socket()
        .with_reclaim(mode)
        .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string());
    let level_path = run.dir().join(format!("host-level-{label}"));
    if dig_under_warn {
        write_level(&level_path, "warn");
        cfg = cfg.with_env(
            "LIMINA_HOST_PRESSURE",
            &format!("@{}", level_path.display()),
        );
    }

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    std::thread::sleep(Duration::from_secs(5));
    let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
    let mut conn = limina_test::bench::join_control_as_agent(&mut guest, "limina-bench-s3/0")
        .expect("joining the control plane");
    let mut host_samples: Vec<HostSample> = Vec::new();

    // Create the working-set file first (it may be partially cached; the balloon squeeze
    // below evicts what the allowance can't hold).
    eprintln!("  creating the {FILE_MIB} MiB working-set file");
    guest
        .ssh_exec(&format!(
            "df -m --output=avail /var/tmp | tail -1; dd if=/dev/zero of={THRASH_BIN} bs=1M count={FILE_MIB} 2>/dev/null; sync"
        ))
        .expect("creating the thrash file");

    // Let the dd's pressure tail drain before converging — BOTH give-back arms: its
    // writeback io-full, and the reclaim memory-PSI from filling 3 GiB of cache on this
    // guest (avg60 needs ~60 s to decay, hence the generous budget). A stale reading in
    // the relayed reports would trip the give-back mid-converge (arming its long
    // re-inflation cooldown) and the point would grade an artifact of its own staging.
    for _ in 0..90 {
        match real_report(&guest) {
            Some(r) if r.io_full_avg10 < 100 && r.some_avg60 <= 200 => break,
            _ => std::thread::sleep(Duration::from_secs(2)),
        }
    }

    // Converge the policy on the (now idle) guest: relay real reports until the target is
    // quiet for 45 s. `disabled` skips straight to the workload.
    if mode != "disabled" {
        eprintln!("  converging the policy (real-report relay)");
        let budget = Instant::now() + Duration::from_secs(300);
        let mut last_target = None;
        let mut last_move = Instant::now();
        while Instant::now() < budget {
            if let Some(r) = real_report(&guest) {
                let _ = conn.send(&Message::MemPressure(r));
            }
            std::thread::sleep(Duration::from_millis(1000));
            if let Ok(s) = sample_host(&guest) {
                if last_target != Some(s.target) {
                    last_target = Some(s.target);
                    last_move = Instant::now();
                }
                host_samples.push(s);
            }
            if last_move.elapsed() > Duration::from_secs(45) {
                break;
            }
        }
        let s = host_samples.last().copied().unwrap_or_default();
        eprintln!(
            "  converged: target={} MiB actual={} MiB",
            s.target / MIB,
            s.actual / MIB
        );
    }

    // The warn-dug point thrashes at host-Normal: flip the injected level back and give the
    // policy a few ticks to sample it before the workload starts.
    if dig_under_warn {
        eprintln!("  flipping injected host level warn -> normal");
        write_level(&level_path, "normal");
        for _ in 0..5 {
            if let Some(r) = real_report(&guest) {
                let _ = conn.send(&Message::MemPressure(r));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    // THE THRASH: re-read loop + real-report relay for THRASH_SECS.
    let t0 = start_thrash(&guest);
    eprintln!("  thrashing for {THRASH_SECS} s");
    let deadline = Instant::now() + Duration::from_secs(THRASH_SECS);
    let mut tick = 0u32;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(&guest) {
            host_samples.push(s);
        }
        tick += 1;
        if tick.is_multiple_of(5) {
            if let Some(r) = real_report(&guest) {
                let _ = conn.send(&Message::MemPressure(r));
            }
        }
    }
    let passes = pass_times(&guest);
    stop_thrash(&guest);

    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write(&format!("guest-{label}.csv"), &guest_csv)
        .unwrap();
    run.write_host_samples_named(&format!("host-{label}.csv"), &host_samples)
        .unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write(&format!("journal-{label}.txt"), &journal)
        .unwrap();
    let trace = parse_trace(&std::fs::read_to_string(&trace_path).unwrap_or_default());
    if mode != "disabled" {
        assert!(
            !trace.is_empty(),
            "no policy decisions journaled for {label}"
        );
    }

    let t_end = now_ms();
    let pass_count = passes.iter().filter(|&&t| t >= t0).count();
    // Median inter-pass time (ms) — the throughput number.
    let mut deltas: Vec<u64> = passes.windows(2).map(|w| w[1] - w[0]).collect();
    deltas.sort_unstable();
    let median_pass_ms = deltas.get(deltas.len() / 2).copied().unwrap_or(0);
    // Tail median (last 45 s of passes): the give-back needs tens of seconds to act, so the
    // recovery shows here while the full-window median blurs it. Anchored to the last pass,
    // not `t_end` — the artifact fetches above run before `now_ms()`, so a host-clock anchor
    // silently shrinks the window by however long they took.
    let tail_start = passes.last().copied().unwrap_or(0).saturating_sub(45_000);
    let mut tail_deltas: Vec<u64> = passes
        .windows(2)
        .filter(|w| w[1] >= tail_start)
        .map(|w| w[1] - w[0])
        .collect();
    tail_deltas.sort_unstable();
    let tail_pass_ms = tail_deltas.get(tail_deltas.len() / 2).copied().unwrap_or(0);
    let io_integral = psi_integral_pct_s(&guest_samples, |s| s.io_some_avg10, t0, t_end);
    let mem_integral = psi_integral_pct_s(&guest_samples, |s| s.mem_some_avg10, t0, t_end);
    let min_avail = guest_samples
        .iter()
        .filter(|s| s.ts_ms >= t0)
        .map(|s| s.mem_available_kib)
        .min()
        .unwrap_or(0);
    let end_stats = host_samples.last().copied().unwrap_or_default();
    let released = trace
        .iter()
        .any(|e| e.ts_ms >= t0 && e.sent && e.new_target_pages == Some(0));
    let givebacks = trace
        .iter()
        .filter(|e| e.ts_ms >= t0 && e.decision == "giveback" && e.sent)
        .count();
    let puff = journal
        .lines()
        .filter(|l| l.contains("Out of puff"))
        .count();

    assert!(
        pass_count > 0,
        "point {label}: the thrash loop completed ZERO passes in {THRASH_SECS} s — the workload \
         never ran, the point is hollow (the S3 tmpfs incident class)"
    );
    let json = json_object(&[
        ("label", format!("\"{label}\"")),
        ("mode", format!("\"{mode}\"")),
        ("thrash_secs", THRASH_SECS.to_string()),
        ("passes", pass_count.to_string()),
        ("median_pass_ms", median_pass_ms.to_string()),
        ("tail_pass_ms", tail_pass_ms.to_string()),
        ("psi_io_some_pct_s", format!("{io_integral:.2}")),
        ("psi_mem_some_pct_s", format!("{mem_integral:.2}")),
        ("min_avail_kib", min_avail.to_string()),
        ("end_target", end_stats.target.to_string()),
        ("end_actual", end_stats.actual.to_string()),
        ("released_to_zero", released.to_string()),
        ("givebacks", givebacks.to_string()),
        ("out_of_puff_lines", puff.to_string()),
    ]);
    eprintln!("  point metrics: {json}");
    let outcome = guest.shutdown(Duration::from_secs(15));
    eprintln!("  teardown: {outcome:?}");
    PointResult { json }
}

#[test]
fn s3_cache_starvation() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s3_cache_starvation: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip("s3_cache_starvation") {
        return;
    }
    if let Err(e) = GuestConfig::baseline_fedora_from_env() {
        eprintln!("SKIPPED s3_cache_starvation: {e}");
        return;
    }
    let run = BenchRun::create("s3").expect("creating the bench run dir");
    eprintln!("S3 cache starvation; artifacts -> {:?}", run.dir());
    let results: Vec<String> = [
        ("disabled", "disabled", false),
        ("moderate", "moderate", false),
        ("warn-dug", "moderate", true),
    ]
    .iter()
    .map(|(label, mode, dug)| run_point(&run, label, mode, *dug).json)
    .collect();
    let metrics = json_object(&[
        ("scenario", "\"s3-cache-starvation\"".to_string()),
        ("guest_tier", "\"stock-4k\"".to_string()),
        ("file_mib", FILE_MIB.to_string()),
        ("points", format!("[{}]", results.join(","))),
    ]);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S3 metrics ==\n{metrics}");
}
