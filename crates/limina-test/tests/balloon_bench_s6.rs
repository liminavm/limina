// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S6 — host-pressure staircase** (`docs/design/balloon-bench.md` §6).
//!
//! Uses the `LIMINA_HOST_PRESSURE=@file` seam: the harness rewrites the level file while
//! the supervisor runs — normal → warn → critical → normal, a dwell at each step, real
//! idle reports relayed throughout. Verifies each mode's allowance ladder engages and
//! disengages (light does nothing at normal, engages at warn; floors hold at critical;
//! release on recovery) and measures the transition latencies from the decision trace.
//!
//! Sized 2048..16384 MiB deliberately: on a small VM the moderate normal/warn allowances
//! BOTH clamp to the 1 GiB floor and the ladder is invisible — at 16 GiB max they separate
//! (2 GiB / 1 GiB / 512 MiB). The idle guest never touches most of that RAM, so the host
//! only pays for what the guest faulted.
//!
//! Modes: light (the whole point — it only acts under host pressure) and moderate.
//! Aggressive ignores host pressure by design; excluded, noted in RESULTS.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF.

use std::time::{Duration, Instant};

use limina_proto::Message;
use limina_test::bench::{
    json_object, now_ms, parse_trace, real_report, sample_host, BenchRun, GuestSampler, HostSample,
};
use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 16384;
/// Dwell per staircase step.
const STEP_SECS: u64 = 90;
const STEPS: [&str; 4] = ["normal", "warn", "critical", "normal"];

struct StepResult {
    json: String,
}

fn run_mode(run: &BenchRun, mode: &str) -> String {
    let trace_path = run.dir().join(format!("trace-{mode}.jsonl"));
    let level_path = run.dir().join(format!("host-level-{mode}"));
    std::fs::write(&level_path, "normal\n").expect("seeding the host-level file");
    eprintln!("== S6 mode={mode} ==");

    let cfg = GuestConfig::baseline_fedora_from_env()
        .expect("baseline disk (checked before)")
        .with_net()
        .with_memory(MIN_MIB, MAX_MIB)
        .with_balloon_control()
        .with_control_socket()
        .with_reclaim(mode)
        .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string())
        .with_env(
            "LIMINA_HOST_PRESSURE",
            &format!("@{}", level_path.display()),
        );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    std::thread::sleep(Duration::from_secs(5));
    let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
    let mut conn = limina_test::bench::join_control_as_agent(&mut guest, "limina-bench-s6/0")
        .expect("joining the control plane");
    let mut host_samples: Vec<HostSample> = Vec::new();

    let mut steps: Vec<StepResult> = Vec::new();
    for level in STEPS {
        std::fs::write(&level_path, format!("{level}\n")).expect("stepping the host level");
        let t_step = now_ms();
        eprintln!("  step -> {level} ({STEP_SECS} s dwell)");
        let deadline = Instant::now() + Duration::from_secs(STEP_SECS);
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
        let end = host_samples.last().copied().unwrap_or_default();
        eprintln!(
            "    step end: target={} MiB actual={} MiB",
            end.target / MIB,
            end.actual / MIB
        );
        steps.push(StepResult {
            json: json_object(&[
                ("level", format!("\"{level}\"")),
                ("t_step_ms", t_step.to_string()),
                ("end_target", end.target.to_string()),
                ("end_actual", end.actual.to_string()),
            ]),
        });
    }

    let (guest_csv, _) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write(&format!("guest-{mode}.csv"), &guest_csv).unwrap();
    run.write_host_samples_named(&format!("host-{mode}.csv"), &host_samples)
        .unwrap();
    let trace = parse_trace(&std::fs::read_to_string(&trace_path).unwrap_or_default());
    assert!(
        !trace.is_empty(),
        "no policy decisions journaled for {mode}"
    );
    // The injected level must actually reach the policy: the trace's host field follows
    // the file (the @file seam's positive control).
    for level in ["warn", "critical"] {
        assert!(
            trace.iter().any(|e| e.host == level),
            "mode {mode}: the decision trace never saw host={level} — the @file seam \
             did not reach the supervisor"
        );
    }

    let json = json_object(&[
        ("mode", format!("\"{mode}\"")),
        (
            "steps",
            format!(
                "[{}]",
                steps
                    .iter()
                    .map(|s| s.json.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        (
            "sent_changes",
            trace.iter().filter(|e| e.sent).count().to_string(),
        ),
    ]);
    let outcome = guest.shutdown(Duration::from_secs(15));
    eprintln!("  teardown: {outcome:?}");
    json
}

#[test]
fn s6_host_pressure_staircase() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s6_host_pressure_staircase: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip("s6_host_pressure_staircase") {
        return;
    }
    if let Err(e) = GuestConfig::baseline_fedora_from_env() {
        eprintln!("SKIPPED s6_host_pressure_staircase: {e}");
        return;
    }
    let run = BenchRun::create("s6").expect("creating the bench run dir");
    eprintln!("S6 staircase; artifacts -> {:?}", run.dir());
    let results: Vec<String> = ["light", "moderate"]
        .iter()
        .map(|m| run_mode(&run, m))
        .collect();
    let metrics = json_object(&[
        ("scenario", "\"s6-staircase\"".to_string()),
        ("min_mib", MIN_MIB.to_string()),
        ("max_mib", MAX_MIB.to_string()),
        ("step_secs", STEP_SECS.to_string()),
        ("modes", format!("[{}]", results.join(","))),
    ]);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S6 metrics ==\n{metrics}");
}
