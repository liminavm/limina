// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S2 — allocation-rate sweep** (`docs/design/balloon-bench.md` §6).
//!
//! The headline scenario for "the balloon does not deflate quickly enough": against a
//! balloon pre-inflated to the policy cap, touch 3 GiB of anonymous memory at a
//! *controlled* rate R while relaying the guest's REAL pressure reports at the agent's
//! cadence — the production release loop, played faithfully — and record whether the
//! burst survives, how fast the policy detected (trace: first sent target decrease after
//! burst start), and what the squeeze cost (PSI integrals, swap, reclaim work).
//!
//! Fresh boot per (mode, rate) point: the 5-minute post-release cooldown is supervisor
//! state, and a within-boot sweep would either wait it out or measure a polluted policy.
//! `disabled` runs unthrottled only — it is the slowdown denominator (no balloon to fight).
//! `light` at host-normal holds no balloon by design (documented, not measured here; the
//! S6 staircase exercises light under injected host pressure).
//!
//! This is also `LIMINA_BALLOON_TRACE`'s in-vivo positive control: enforcing-mode points
//! assert the supervisor actually journaled consumed reports.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF. Point list overridable via
//! `LIMINA_BENCH_S2_MODES` / `LIMINA_BENCH_S2_RATES` (MiB/s, 0 = unthrottled).

use std::time::{Duration, Instant};

use limina_proto::Message;
use limina_test::bench::{
    burst_status, count_oom_since, counter_delta, fetch_balloon_journal,
    first_target_decrease_after, guest_epoch_secs, idle_report, join_control_as_agent, json_object,
    kill_burst, now_ms, parse_trace, psi_integral_pct_s, real_report, sample_host, start_burst,
    tier, tier_config, verify_tier, BenchRun, BurstStatus, GuestSampler, HostSample, Tier,
};
use limina_test::Guest;

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
/// Pre-burst inflation floor: near the policy cap (room = 4096 MiB), so the burst cannot
/// fit without the release path (the balloon_burst sizing lesson).
const INFLATE_FLOOR: u64 = 3800 * MIB;
/// The burst: 3 GiB of touched anonymous memory.
const BURST_TOTAL: u64 = 3 * 1024 * MIB;

struct Point {
    mode: &'static str,
    rate_mib_s: u64,
}

fn points() -> Vec<Point> {
    // Phase 2's axis is the TIER, not the mode ladder (Phase 1 characterized the modes):
    // enhanced defaults to moderate + the disabled denominator only.
    let default_modes = match tier() {
        Tier::Stock => "disabled,moderate,aggressive",
        Tier::Enhanced => "disabled,moderate",
    };
    let modes: Vec<String> = std::env::var("LIMINA_BENCH_S2_MODES")
        .unwrap_or_else(|_| default_modes.into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let rates: Vec<u64> = std::env::var("LIMINA_BENCH_S2_RATES")
        .unwrap_or_else(|_| "512,1024,2048,0".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let mut out = Vec::new();
    for mode in &modes {
        let mode: &'static str = match mode.as_str() {
            "disabled" => "disabled",
            "light" => "light",
            "moderate" => "moderate",
            "aggressive" => "aggressive",
            other => panic!("unknown reclaim mode {other:?} in LIMINA_BENCH_S2_MODES"),
        };
        if mode == "disabled" {
            // The denominator: no balloon to fight, one unthrottled point.
            out.push(Point {
                mode,
                rate_mib_s: 0,
            });
        } else {
            for &r in &rates {
                out.push(Point {
                    mode,
                    rate_mib_s: r,
                });
            }
        }
    }
    out
}

/// Everything one (mode, rate) point measured, as a metrics JSON object.
struct PointOutcome {
    json: String,
    survived: bool,
}

fn run_point(run: &BenchRun, p: &Point) -> PointOutcome {
    let tag = format!("{}-{}", p.mode, p.rate_mib_s);
    let trace_path = run.dir().join(format!("trace-{tag}.jsonl"));
    eprintln!(
        "== S2 point: mode={} rate={} MiB/s ==",
        p.mode, p.rate_mib_s
    );

    let mut cfg = tier_config()
        .expect("tier disk (checked before the sweep)")
        .with_net()
        .with_memory(MIN_MIB, MAX_MIB)
        .with_balloon_control()
        .with_control_socket()
        .with_reclaim(p.mode)
        .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string());
    cfg.ram_mib = MAX_MIB;

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    let stamp = verify_tier(&guest, &cfg).expect("tier positive control");
    std::thread::sleep(Duration::from_secs(5));
    let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
    // Stock: the harness plays the agent. Enhanced: the REAL agent reports — relaying on
    // top of it would double-feed the policy and un-measure the real D2.
    let mut conn = match tier() {
        Tier::Stock => Some(
            join_control_as_agent(&mut guest, &format!("limina-bench-s2/{tag}"))
                .expect("joining the control plane"),
        ),
        Tier::Enhanced => None,
    };
    let mut host_samples: Vec<HostSample> = Vec::new();

    // Pre-inflate to the floor. Stock: synthetic idle reports (constant avail ratchets the
    // target to room). Enhanced: the real closed loop does it on its own (S0 measured full
    // convergence in ~30 s) — we only watch.
    let mut balloon_at_burst = 0u64;
    if p.mode != "disabled" {
        eprintln!("  inflating to the policy cap ({})", tier().label());
        let deadline = Instant::now() + Duration::from_secs(200);
        loop {
            if let Some(c) = conn.as_mut() {
                c.send(&Message::MemPressure(idle_report(MAX_MIB as u64)))
                    .expect("sending idle report");
            }
            std::thread::sleep(Duration::from_millis(1200));
            let s = sample_host(&guest).expect("host sample");
            host_samples.push(s);
            balloon_at_burst = s.actual;
            if s.actual >= INFLATE_FLOOR {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "mode {} could not inflate to the S2 floor: actual={} MiB — the burst \
                 would fit in free RAM and measure nothing",
                p.mode,
                s.actual / MIB
            );
        }
        eprintln!("  balloon at burst start: {} MiB", balloon_at_burst / MIB);
    }

    let journal_mark = guest_epoch_secs(&guest).expect("guest clock");
    let pre_burst = now_ms();

    // THE BURST, with the real-pressure relay interleaved (host samples at ~200 ms, a
    // relay+status tick every ~1 s — the agent's cadence).
    let rate_bytes = p.rate_mib_s * MIB;
    let t0 = start_burst(&guest, BURST_TOTAL, rate_bytes).expect("starting the burst");
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut tick = 0u32;
    let outcome = loop {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(&guest) {
            host_samples.push(s);
        }
        tick += 1;
        if tick % 5 == 0 {
            if let Some(c) = conn.as_mut() {
                if let Some(r) = real_report(&guest) {
                    let _ = c.send(&Message::MemPressure(r));
                }
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

    // Post-burst observation window (the burst HOLDS its allocation): relief under
    // sustained occupancy.
    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(&guest) {
            host_samples.push(s);
        }
    }
    let oom = count_oom_since(&guest, &journal_mark);
    kill_burst(&guest).ok();

    // Collect.
    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write(&format!("guest-{tag}.csv"), &guest_csv).unwrap();
    run.write_host_samples_named(&format!("host-{tag}.csv"), &host_samples)
        .unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write(&format!("journal-{tag}.txt"), &journal).unwrap();
    let trace_raw = std::fs::read_to_string(&trace_path).unwrap_or_default();
    let trace = parse_trace(&trace_raw);
    if p.mode != "disabled" {
        // LIMINA_BALLOON_TRACE's in-vivo positive control.
        assert!(
            !trace.is_empty(),
            "mode {}: the supervisor journaled no decisions — LIMINA_BALLOON_TRACE did not \
             reach the policy (env plumbing broke)",
            p.mode
        );
    }

    let survived = outcome.is_ok() && oom == 0;
    let burst_end = *outcome.as_ref().unwrap_or(&now_ms());
    let detection = first_target_decrease_after(&trace, t0);
    let min_avail = guest_samples
        .iter()
        .filter(|s| s.ts_ms >= t0)
        .map(|s| s.mem_available_kib)
        .min()
        .unwrap_or(0);
    let dj = |d: Option<(u64, u64, u64)>| {
        d.map_or("null".to_string(), |(ts, from, to)| {
            json_object(&[
                ("ts_ms", ts.to_string()),
                ("latency_ms", ts.saturating_sub(t0).to_string()),
                ("from_pages", from.to_string()),
                ("to_pages", to.to_string()),
            ])
        })
    };
    let cd = |f: fn(&limina_test::bench::GuestSample) -> u64| {
        counter_delta(&guest_samples, f, t0, burst_end + 5000)
    };
    let mut point_entries = stamp.entries();
    point_entries.extend([
        ("mode", format!("\"{}\"", p.mode)),
        ("rate_mib_s", p.rate_mib_s.to_string()),
        ("survived", survived.to_string()),
        (
            "outcome",
            format!("\"{}\"", outcome.map(|_| "ok").unwrap_or_else(|e| e)),
        ),
        ("oom_kills", oom.to_string()),
        ("balloon_at_burst_bytes", balloon_at_burst.to_string()),
        ("t0_ms", t0.to_string()),
        ("burst_wall_ms", burst_end.saturating_sub(t0).to_string()),
        ("detection", dj(detection)),
        ("min_avail_kib", min_avail.to_string()),
        (
            "psi_mem_some_pct_s",
            format!(
                "{:.2}",
                psi_integral_pct_s(&guest_samples, |s| s.mem_some_avg10, t0, burst_end + 5000)
            ),
        ),
        (
            "psi_io_some_pct_s",
            format!(
                "{:.2}",
                psi_integral_pct_s(&guest_samples, |s| s.io_some_avg10, t0, burst_end + 5000)
            ),
        ),
        ("pgmajfault", cd(|s| s.pgmajfault).to_string()),
        ("pswpout", cd(|s| s.pswpout).to_string()),
        ("pgsteal_kswapd", cd(|s| s.pgsteal_kswapd).to_string()),
        ("pgsteal_direct", cd(|s| s.pgsteal_direct).to_string()),
        ("kswapd_cpu_ticks", cd(|s| s.kswapd_cpu_ticks).to_string()),
        (
            "out_of_puff_lines",
            journal
                .lines()
                .filter(|l| l.contains("Out of puff"))
                .count()
                .to_string(),
        ),
        ("pre_burst_ms", pre_burst.to_string()),
    ]);
    let json = json_object(&point_entries);
    eprintln!("  point metrics: {json}");

    let outcome_teardown = guest.shutdown(Duration::from_secs(15));
    eprintln!("  teardown: {outcome_teardown:?}");
    PointOutcome { json, survived }
}

#[test]
fn s2_allocation_rate_sweep() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s2_allocation_rate_sweep: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip("s2_allocation_rate_sweep") {
        return;
    }
    if let Err(e) = tier_config() {
        eprintln!("SKIPPED s2_allocation_rate_sweep: {e}");
        return;
    }

    let run =
        BenchRun::create(&format!("s2{}", tier().suffix())).expect("creating the bench run dir");
    eprintln!(
        "S2 sweep ({} tier); artifacts -> {:?}",
        tier().label(),
        run.dir()
    );
    let mut results = Vec::new();
    let mut any_survived = false;
    for p in points() {
        let out = run_point(&run, &p);
        any_survived |= out.survived;
        results.push(out.json);
    }
    let metrics = json_object(&[
        ("scenario", "\"s2-rate-sweep\"".to_string()),
        ("tier", format!("\"{}\"", tier().label())),
        ("min_mib", MIN_MIB.to_string()),
        ("max_mib", MAX_MIB.to_string()),
        ("burst_total_bytes", BURST_TOTAL.to_string()),
        ("points", format!("[{}]", results.join(","))),
    ]);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S2 sweep metrics ==\n{metrics}");
    // Sanity only: a sweep where nothing survives (including the disabled denominator)
    // means the vehicle, not the policy, is broken.
    assert!(
        any_survived,
        "no S2 point survived — the burst vehicle itself is broken"
    );
}
