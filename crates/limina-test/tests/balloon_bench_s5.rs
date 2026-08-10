// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S5 — burst under a busy control plane**
//! (`docs/design/balloon-bench.md` §6).
//!
//! The D2-starvation question: the control plane serves multiple concurrent peers (one
//! serve thread each — `crates/limina/src/control.rs`), so *does* a chatty peer stretch
//! the policy's report inter-arrival, and does detection latency follow? Two points, both
//! moderate @ 512 MiB/s (the rate Phase 1 showed the policy genuinely covers, so a
//! detection shift is visible in the outcome, not clipped by the burst finishing first):
//! - **quiet**: exactly S2's shape — the baseline.
//! - **chatty**: same, plus a second control connection spamming heartbeats at ~200/s for
//!   the whole burst window.
//!
//! Fresh boot per point (the release cooldown is supervisor state). Stock tier — the
//! harness plays both the agent and the chatty peer. The metrics are the comparison
//! (report-gap median/p95, detection latency); asserts are sanity only. Note the guest
//! agent's OWN starvation mode (reports ride only idle poll ticks, so host→agent traffic
//! defers them) is a different seam — an S8/design note, not synthesizable from here.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use limina_proto::{Heartbeat, Message};
use limina_test::bench::{
    burst_status, count_oom_since, fetch_balloon_journal, first_target_decrease_after,
    guest_epoch_secs, idle_report, join_control_as_agent, json_object, kill_burst, now_ms,
    parse_trace, real_report, sample_host, start_burst, tier, tier_config, verify_tier, BenchRun,
    BurstStatus, GuestSampler, HostSample, Tier,
};
use limina_test::Guest;

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
const INFLATE_FLOOR: u64 = 3800 * MIB;
const BURST_TOTAL: u64 = 3 * 1024 * MIB;
const BURST_RATE: u64 = 512 * MIB;
/// Chatty-peer heartbeat pacing (~200 msg/s).
const SPAM_SLEEP: Duration = Duration::from_millis(5);

fn run_point(run: &BenchRun, chatty: bool) -> String {
    let tag = if chatty { "chatty" } else { "quiet" };
    let trace_path = run.dir().join(format!("trace-{tag}.jsonl"));
    eprintln!("== S5 point: {tag} ==");

    let mut cfg = tier_config()
        .expect("tier disk (checked before the run)")
        .with_net()
        .with_memory(MIN_MIB, MAX_MIB)
        .with_balloon_control()
        .with_control_socket()
        .with_reclaim("moderate")
        .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string());
    cfg.ram_mib = MAX_MIB;

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    verify_tier(&guest, &cfg).expect("tier positive control");
    std::thread::sleep(Duration::from_secs(5));
    let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
    let mut conn =
        join_control_as_agent(&mut guest, &format!("limina-bench-s5/{tag}")).expect("agent conn");
    let mut host_samples: Vec<HostSample> = Vec::new();

    // Pre-inflate to the floor (synthetic idle reports, as S2).
    let deadline = Instant::now() + Duration::from_secs(200);
    let mut balloon_at_burst;
    loop {
        conn.send(&Message::MemPressure(idle_report(MAX_MIB as u64)))
            .expect("sending idle report");
        std::thread::sleep(Duration::from_millis(1200));
        let s = sample_host(&guest).expect("host sample");
        host_samples.push(s);
        balloon_at_burst = s.actual;
        if s.actual >= INFLATE_FLOOR {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "could not inflate to the S5 floor: actual={} MiB",
            s.actual / MIB
        );
    }
    eprintln!("  balloon at burst start: {} MiB", balloon_at_burst / MIB);

    // The chatty peer: a second control connection, heartbeats at ~200/s until stopped.
    let stop = Arc::new(AtomicBool::new(false));
    let spammer = if chatty {
        let mut spam_conn =
            join_control_as_agent(&mut guest, "limina-bench-s5/spam").expect("spam conn");
        let stop = Arc::clone(&stop);
        Some(std::thread::spawn(move || -> u64 {
            let mut sent = 0u64;
            let mut seq = 0u64;
            while !stop.load(Ordering::Relaxed) {
                seq += 1;
                if spam_conn
                    .send(&Message::Heartbeat(Heartbeat { seq }))
                    .is_err()
                {
                    break;
                }
                sent += 1;
                std::thread::sleep(SPAM_SLEEP);
            }
            sent
        }))
    } else {
        None
    };

    let journal_mark = guest_epoch_secs(&guest).expect("guest clock");
    let t0 = start_burst(&guest, BURST_TOTAL, BURST_RATE).expect("starting the burst");
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut tick = 0u32;
    let outcome = loop {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(&guest) {
            host_samples.push(s);
        }
        tick += 1;
        if tick % 5 == 0 {
            if let Some(r) = real_report(&guest) {
                let _ = conn.send(&Message::MemPressure(r));
            }
            match burst_status(&guest).unwrap_or(BurstStatus::Died) {
                BurstStatus::Complete(ts) => break Ok(ts),
                BurstStatus::Died => break Err("died"),
                BurstStatus::Running(_) => {}
            }
        }
        if Instant::now() >= deadline {
            break Err("timeout");
        }
    };
    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(&guest) {
            host_samples.push(s);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let spam_sent = spammer.map(|h| h.join().unwrap_or(0)).unwrap_or(0);
    let oom = count_oom_since(&guest, &journal_mark);
    kill_burst(&guest).ok();

    let (guest_csv, _guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write(&format!("guest-{tag}.csv"), &guest_csv).unwrap();
    run.write_host_samples_named(&format!("host-{tag}.csv"), &host_samples)
        .unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write(&format!("journal-{tag}.txt"), &journal).unwrap();
    let trace_raw = std::fs::read_to_string(&trace_path).unwrap_or_default();
    let trace = parse_trace(&trace_raw);
    assert!(
        !trace.is_empty(),
        "{tag}: the supervisor journaled no decisions — LIMINA_BALLOON_TRACE broke"
    );

    // Consumed-report inter-arrival during the burst window: the D2 observable.
    let mut gaps: Vec<u64> = trace
        .windows(2)
        .filter(|w| w[1].ts_ms >= t0)
        .map(|w| w[1].ts_ms - w[0].ts_ms)
        .collect();
    gaps.sort_unstable();
    let median_gap = gaps.get(gaps.len() / 2).copied().unwrap_or(0);
    let p95_gap = gaps
        .get((gaps.len() * 95) / 100)
        .copied()
        .unwrap_or(median_gap);
    let detection = first_target_decrease_after(&trace, t0);
    let burst_end = *outcome.as_ref().unwrap_or(&now_ms());

    json_object(&[
        ("point", format!("\"{tag}\"")),
        ("survived", (outcome.is_ok() && oom == 0).to_string()),
        (
            "outcome",
            format!("\"{}\"", outcome.map(|_| "ok").unwrap_or_else(|e| e)),
        ),
        ("oom_kills", oom.to_string()),
        ("spam_msgs_sent", spam_sent.to_string()),
        ("t0_ms", t0.to_string()),
        ("burst_wall_ms", burst_end.saturating_sub(t0).to_string()),
        ("median_report_gap_ms", median_gap.to_string()),
        ("p95_report_gap_ms", p95_gap.to_string()),
        (
            "detection_latency_ms",
            detection.map_or("null".to_string(), |(ts, _, _)| {
                ts.saturating_sub(t0).to_string()
            }),
        ),
    ])
}

#[test]
fn s5_burst_under_busy_control_plane() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s5_burst_under_busy_control_plane: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip("s5_burst_under_busy_control_plane") {
        return;
    }
    if tier() != Tier::Stock {
        // The harness must play the agent to also play the chatty peer deterministically.
        eprintln!("SKIPPED s5_burst_under_busy_control_plane: stock-tier scenario");
        return;
    }
    if let Err(e) = tier_config() {
        eprintln!("SKIPPED s5_burst_under_busy_control_plane: {e}");
        return;
    }
    let run = BenchRun::create("s5").expect("creating the bench run dir");
    eprintln!("S5 busy control plane; artifacts -> {:?}", run.dir());
    let quiet = run_point(&run, false);
    let chatty = run_point(&run, true);
    let metrics = json_object(&[
        ("scenario", "\"s5-busy-control-plane\"".to_string()),
        ("burst_rate_mib_s", (BURST_RATE / MIB).to_string()),
        ("quiet", quiet),
        ("chatty", chatty),
    ]);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S5 metrics ==\n{metrics}");
}
