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

const THRASH_OUT: &str = "/tmp/limina-thrash.out";

/// Stage + start the re-read loop (one line per completed pass: `pass <ms>`).
fn start_thrash(guest: &Guest) -> u64 {
    guest
        .ssh_exec(&format!(
            "rm -f {THRASH_OUT}; setsid nohup bash -c 'while true; do cat /tmp/limina-thrash.bin > /dev/null; echo pass $(date +%s%3N) >> {THRASH_OUT}; done' </dev/null >/dev/null 2>&1 & echo spawned"
        ))
        .expect("spawning the thrash loop");
    now_ms()
}

fn stop_thrash(guest: &Guest) {
    // Bracketed patterns: a bare pattern matches the ssh shell whose own command line
    // carries it (the balloon_burst pgrep lesson, in pkill form).
    guest
        .ssh_exec(
            "pkill -f '[l]imina-thrash.bin' || true; pkill -f '[c]at /tmp/limina-thrash' || true",
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

/// One S3 point: boot with `mode`, optionally converge the policy, thrash, measure.
fn run_point(run: &BenchRun, mode: &str) -> PointResult {
    let trace_path = run.dir().join(format!("trace-{mode}.jsonl"));
    eprintln!("== S3 point: mode={mode} ==");
    let cfg = GuestConfig::baseline_fedora_from_env()
        .expect("baseline disk (checked before)")
        .with_net()
        .with_memory(MIN_MIB, MAX_MIB)
        .with_balloon_control()
        .with_control_socket()
        .with_reclaim(mode)
        .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string());

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
            "dd if=/dev/zero of=/tmp/limina-thrash.bin bs=1M count={FILE_MIB} 2>/dev/null; sync"
        ))
        .expect("creating the thrash file");

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
        if tick % 5 == 0 {
            if let Some(r) = real_report(&guest) {
                let _ = conn.send(&Message::MemPressure(r));
            }
        }
    }
    let passes = pass_times(&guest);
    stop_thrash(&guest);

    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write(&format!("guest-{mode}.csv"), &guest_csv).unwrap();
    run.write_host_samples_named(&format!("host-{mode}.csv"), &host_samples)
        .unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write(&format!("journal-{mode}.txt"), &journal).unwrap();
    let trace = parse_trace(&std::fs::read_to_string(&trace_path).unwrap_or_default());
    if mode != "disabled" {
        assert!(
            !trace.is_empty(),
            "no policy decisions journaled for {mode}"
        );
    }

    let t_end = now_ms();
    let pass_count = passes.iter().filter(|&&t| t >= t0).count();
    // Median inter-pass time (ms) — the throughput number.
    let mut deltas: Vec<u64> = passes.windows(2).map(|w| w[1] - w[0]).collect();
    deltas.sort_unstable();
    let median_pass_ms = deltas.get(deltas.len() / 2).copied().unwrap_or(0);
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
    let puff = journal
        .lines()
        .filter(|l| l.contains("Out of puff"))
        .count();

    let json = json_object(&[
        ("mode", format!("\"{mode}\"")),
        ("thrash_secs", THRASH_SECS.to_string()),
        ("passes", pass_count.to_string()),
        ("median_pass_ms", median_pass_ms.to_string()),
        ("psi_io_some_pct_s", format!("{io_integral:.2}")),
        ("psi_mem_some_pct_s", format!("{mem_integral:.2}")),
        ("min_avail_kib", min_avail.to_string()),
        ("end_target", end_stats.target.to_string()),
        ("end_actual", end_stats.actual.to_string()),
        ("released_to_zero", released.to_string()),
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
    let results: Vec<String> = ["disabled", "moderate"]
        .iter()
        .map(|m| run_point(&run, m).json)
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
