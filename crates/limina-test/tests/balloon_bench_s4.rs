// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S4 — idle-inflate convergence** (`docs/design/balloon-bench.md` §6).
//!
//! The inflate-side characterization and the regression guard against re-introducing the
//! limit cycle: a genuinely idle guest, the REAL pressure relay at the agent's cadence
//! (unlike balloon_psi's constant synthetic reports, the guest's avail actually shrinks as
//! the balloon grows — the honest closed loop), moderate mode. Measures: inflation onset
//! (I1 — how long the calm gates hold after boot), convergence time and achieved cadence
//! (I2), actual-follows-target lag (I3), reversal count over a stability window, and
//! reclaim effectiveness (I4 — reclaimed= and worker footprint against a *boot-warmed*
//! guest; the S1 caveat about never-faulted pages applies here too and is recorded, not
//! hidden). Also snapshots `Out of puff` lines — H2 says a few transients around
//! inflation steps are expected; a *persistent* stream during the stability window is H1.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF.

use std::time::{Duration, Instant};

use limina_proto::Message;
use limina_test::bench::{
    fetch_balloon_journal, json_object, now_ms, parse_trace, real_report, sample_host,
    target_reversals, tier, tier_config, verify_tier, BenchRun, GuestSampler, HostSample, Tier,
};
use limina_test::Guest;

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
/// Sanity floor: an idle 6 GiB guest under moderate must give up at least this much.
const CONVERGE_FLOOR: u64 = 2048 * MIB;
/// Keep relaying after the last target movement for this long (the oscillation window).
const STABILITY_WINDOW: Duration = Duration::from_secs(300);
/// Total relay budget.
const RUN_BUDGET: Duration = Duration::from_secs(720);

#[test]
fn s4_idle_inflate_convergence() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s4_idle_inflate_convergence: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip("s4_idle_inflate_convergence") {
        return;
    }
    let run = match BenchRun::create(&format!("s4{}", tier().suffix())) {
        Ok(r) => r,
        Err(e) => panic!("bench run dir: {e}"),
    };
    let trace_path = run.dir().join("trace.jsonl");
    let cfg = match tier_config() {
        Ok(cfg) => cfg
            .with_net()
            .with_memory(MIN_MIB, MAX_MIB)
            .with_balloon_control()
            .with_control_socket()
            .with_reclaim("moderate")
            .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string()),
        Err(e) => {
            eprintln!("SKIPPED s4_idle_inflate_convergence: {e}");
            return;
        }
    };
    eprintln!(
        "S4 convergence: moderate, {} tier; artifacts -> {:?}",
        tier().label(),
        run.dir()
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    let stamp = verify_tier(&guest, &cfg).expect("tier positive control");
    std::thread::sleep(Duration::from_secs(5));
    let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
    // Stock: relay REAL reports at the agent cadence (the honest closed loop, played by
    // the harness). Enhanced: the REAL agent is the loop — the harness only observes.
    let mut conn = match tier() {
        Tier::Stock => Some(
            limina_test::bench::join_control_as_agent(&mut guest, "limina-bench-s4/0")
                .expect("joining the control plane"),
        ),
        Tier::Enhanced => None,
    };

    // Run until the target has been quiet for the stability window (or the budget runs out).
    let t_start = now_ms();
    let mut host_samples: Vec<HostSample> = Vec::new();
    let start = Instant::now();
    let mut last_target: Option<u64> = None;
    let mut last_move = Instant::now();
    while start.elapsed() < RUN_BUDGET {
        if let Some(c) = conn.as_mut() {
            if let Some(r) = real_report(&guest) {
                let _ = c.send(&Message::MemPressure(r));
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
        if let Ok(s) = sample_host(&guest) {
            if last_target != Some(s.target) {
                last_target = Some(s.target);
                last_move = Instant::now();
            }
            host_samples.push(s);
        }
        if last_move.elapsed() > STABILITY_WINDOW {
            break;
        }
    }

    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write("guest.csv", &guest_csv).unwrap();
    run.write_host_samples(&host_samples).unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write("journal.txt", &journal).unwrap();
    let trace_raw = std::fs::read_to_string(&trace_path).unwrap_or_default();
    let trace = parse_trace(&trace_raw);

    // LIMINA_BALLOON_TRACE's positive control (with S2): the policy must have journaled.
    assert!(
        !trace.is_empty(),
        "the supervisor journaled no decisions — LIMINA_BALLOON_TRACE did not reach the policy"
    );

    let final_stats = host_samples.last().copied().unwrap_or_default();
    let sent_sets: Vec<&limina_test::bench::TraceEvent> = trace
        .iter()
        .filter(|e| e.sent && e.new_target_pages.is_some())
        .collect();
    // I1 onset: first sent target change after relay start. I2 convergence: the last one
    // (the stability window after it was quiet by construction of the loop above).
    let onset_ms = sent_sets
        .first()
        .map(|e| e.ts_ms.saturating_sub(t_start))
        .unwrap_or(0);
    let converge_ms = sent_sets
        .last()
        .map(|e| e.ts_ms.saturating_sub(t_start))
        .unwrap_or(0);
    // Report cadence at the policy (D2 observed): median inter-arrival of consumed reports.
    let mut gaps: Vec<u64> = trace.windows(2).map(|w| w[1].ts_ms - w[0].ts_ms).collect();
    gaps.sort_unstable();
    let median_gap = gaps.get(gaps.len() / 2).copied().unwrap_or(0);
    let puff_lines = journal
        .lines()
        .filter(|l| l.contains("Out of puff"))
        .count();
    // Held-gap residency: fraction of host samples with a nonzero target/actual gap
    // (> dead band, 16 MiB) — the "driver chasing an unreachable target" signature.
    let gap_samples = host_samples
        .iter()
        .filter(|s| s.target.abs_diff(s.actual) > 16 * MIB)
        .count();

    let mut entries = stamp.entries();
    entries.extend([
        ("scenario", "\"s4-idle-converge\"".to_string()),
        ("mode", "\"moderate\"".to_string()),
        ("t_start_ms", t_start.to_string()),
        ("onset_ms", onset_ms.to_string()),
        ("converge_ms", converge_ms.to_string()),
        ("sent_target_changes", sent_sets.len().to_string()),
        ("reversals", target_reversals(&trace).to_string()),
        ("median_report_gap_ms", median_gap.to_string()),
        ("final_target", final_stats.target.to_string()),
        ("final_actual", final_stats.actual.to_string()),
        ("final_reclaimed", final_stats.reclaimed.to_string()),
        (
            "final_worker_footprint",
            final_stats.worker_footprint.to_string(),
        ),
        ("out_of_puff_lines", puff_lines.to_string()),
        (
            "gap_residency_pct",
            format!(
                "{:.1}",
                100.0 * gap_samples as f64 / host_samples.len().max(1) as f64
            ),
        ),
        ("guest_samples", guest_samples.len().to_string()),
    ]);
    let metrics = json_object(&entries);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S4 metrics ==\n{metrics}");

    assert!(
        final_stats.actual >= CONVERGE_FLOOR,
        "moderate never inflated past {} MiB on an idle guest (actual={} MiB) — the \
         convergence run measured a policy that never engaged",
        CONVERGE_FLOOR / MIB,
        final_stats.actual / MIB
    );

    let outcome = guest.shutdown(Duration::from_secs(15));
    eprintln!("teardown: {outcome:?}");
}
