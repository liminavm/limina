// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S0 — enhanced-tier smoke** (`docs/design/balloon-bench.md` §8 Phase 2).
//!
//! The one boot every enhanced scenario stands on, run FIRST: EFI-boots the F44 enhanced
//! golden headless (own 16k kernel, real limina-agent) and proves the assumptions the
//! rest of Phase 2 inherits:
//! - the tier positive control ([`verify_tier`]): PAGESIZE 16384 + a 7.x kernel — both
//!   the F43 enhanced golden and the stock image would otherwise pass a green run;
//! - the **real-agent positive control**: the policy trace fills with consumed reports
//!   while the harness never joins the control plane — the whole real-D2 premise;
//! - what gdm does on a headless boot (a crash-looping session manager is background
//!   memory churn that would poison "idle" scenarios — recorded, not assumed);
//! - boot-to-ssh time fits the scenarios' 300 s wait.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF + `LIMINA_BENCH_TIER=enhanced` (skips on stock —
//! there is nothing to smoke there; Phase 1 was the stock smoke).

use std::time::{Duration, Instant};

use limina_test::bench::{
    fetch_balloon_journal, json_object, now_ms, parse_trace, sample_host, tier, tier_config,
    verify_tier, BenchRun, Tier,
};
use limina_test::Guest;

const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
/// How long we give the real agent + policy to show life (first consumed report).
const AGENT_WAIT: Duration = Duration::from_secs(90);
/// Total observation after ssh (long enough for moderate to start inflating on idle).
const OBSERVE: Duration = Duration::from_secs(150);

#[test]
fn s0_enhanced_smoke() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s0_enhanced_smoke: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if tier() != Tier::Enhanced {
        eprintln!("SKIPPED s0_enhanced_smoke: enhanced-only (set LIMINA_BENCH_TIER=enhanced)");
        return;
    }
    if !limina_test::require_hvf_or_skip("s0_enhanced_smoke") {
        return;
    }
    let run = match BenchRun::create("s0enh") {
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
            eprintln!("SKIPPED s0_enhanced_smoke: {e}");
            return;
        }
    };
    eprintln!("S0 enhanced smoke; artifacts -> {:?}", run.dir());

    let t_boot = Instant::now();
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    let boot_to_ssh_ms = t_boot.elapsed().as_millis() as u64;
    let t_ssh = now_ms();

    let stamp = verify_tier(&guest, &cfg).expect("tier positive control");
    eprintln!(
        "  tier OK: pagesize={} kernel={} disk={}",
        stamp.pagesize, stamp.uname_r, stamp.disk
    );

    // The real-agent positive control: consumed reports must appear in the policy trace
    // with NO harness relay (this test never touches the control plane).
    let wait_start = Instant::now();
    let mut first_event_ms: Option<u64> = None;
    while wait_start.elapsed() < AGENT_WAIT {
        let raw = std::fs::read_to_string(&trace_path).unwrap_or_default();
        if let Some(e) = parse_trace(&raw).first() {
            first_event_ms = Some(e.ts_ms.saturating_sub(t_ssh));
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let first_event_ms = first_event_ms.expect(
        "the policy consumed no report within 90 s of ssh and no harness relay ran — \
         the REAL agent is not reporting (agent service down, or the control plane \
         never saw it); every enhanced scenario is invalid until this passes",
    );
    eprintln!("  real agent live: first consumed report {first_event_ms} ms after ssh");

    // Observe the rest of the window, then read what the closed loop did on its own.
    let remain = OBSERVE.saturating_sub(wait_start.elapsed());
    std::thread::sleep(remain);

    let trace_raw = std::fs::read_to_string(&trace_path).unwrap_or_default();
    run.write("trace.jsonl.copy", &trace_raw).unwrap();
    let trace = parse_trace(&trace_raw);
    let mut gaps: Vec<u64> = trace.windows(2).map(|w| w[1].ts_ms - w[0].ts_ms).collect();
    gaps.sort_unstable();
    let median_gap = gaps.get(gaps.len() / 2).copied().unwrap_or(0);
    let sent_sets = trace
        .iter()
        .filter(|e| e.sent && e.new_target_pages.is_some())
        .count();
    let final_stats = sample_host(&guest).unwrap_or_default();

    // gdm under a headless EFI boot: record, don't assume. NRestarts climbing = churn.
    let gdm = guest
        .ssh_exec("systemctl show gdm -p ActiveState -p SubState -p NRestarts --no-pager")
        .unwrap_or_else(|e| format!("query failed: {e}"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // The agent service itself, for the record.
    let agent_unit = guest
        .ssh_exec(
            "systemctl show limina-agent -p ActiveState -p NRestarts --no-pager 2>/dev/null \
             || true",
        )
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write("journal.txt", &journal).unwrap();
    let puff_lines = journal
        .lines()
        .filter(|l| l.contains("Out of puff"))
        .count();

    let mut entries = stamp.entries();
    entries.push(("scenario", "\"s0-enhanced-smoke\"".to_string()));
    entries.push(("boot_to_ssh_ms", boot_to_ssh_ms.to_string()));
    entries.push(("first_report_ms_after_ssh", first_event_ms.to_string()));
    entries.push(("trace_events", trace.len().to_string()));
    entries.push(("median_report_gap_ms", median_gap.to_string()));
    entries.push(("sent_target_changes", sent_sets.to_string()));
    entries.push(("final_target", final_stats.target.to_string()));
    entries.push(("final_actual", final_stats.actual.to_string()));
    entries.push(("final_reclaimed", final_stats.reclaimed.to_string()));
    entries.push(("gdm", format!("\"{gdm}\"")));
    entries.push(("limina_agent_unit", format!("\"{agent_unit}\"")));
    entries.push(("out_of_puff_lines", puff_lines.to_string()));
    let metrics = json_object(&entries);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S0 metrics ==\n{metrics}");

    let outcome = guest.shutdown(Duration::from_secs(15));
    eprintln!("teardown: {outcome:?}");
}
