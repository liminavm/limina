// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S8 — the desktop-shaped steady state** (`docs/design/balloon-bench.md`
//! §6 S7-variant, §8 Phase 2): the organic H1 hunt.
//!
//! Phase 1 proved the `Out of puff` mechanism (S7: spam iff a target/actual gap is held)
//! but never caught the *policy* holding one organically — H1 predicts that happens in
//! exactly the shape the dogfood guest lives in: a seated desktop with a warm page cache
//! (MemFree low, MemAvailable high), where MemAvailable-sized targets ask the
//! `__GFP_NORETRY` driver for pages it cannot get. This run reproduces that shape on the
//! enhanced tier with the REAL agent and *no synthetic help*, then just watches:
//! - seated EFI F44 enhanced boot, coexist venus display (the production desktop);
//! - cache warmed by real file reads (the stand-in for weeks of desktop uptime);
//! - ≥30 min under `moderate` (`LIMINA_BENCH_S8_MINUTES` overrides), recording gap
//!   residency + episodes, `Out of puff` lines, trace hold reasons, and reclaim work.
//!
//! The verdict is the data either way: episodes > 0 confirms H1 organically (lever #2's
//! justification); a clean 30 min bounds how much of the dogfood complaint this shape
//! explains. Asserts are sanity only.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF + `LIMINA_BENCH_TIER=enhanced` + KosmicKrisp.

use std::time::{Duration, Instant};

use limina_test::bench::{
    counter_delta, fetch_balloon_journal, json_object, now_ms, parse_trace, sample_host, tier,
    tier_config, verify_tier, BenchRun, GuestSampler, HostSample, Tier,
};
use limina_test::Guest;

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
/// A held gap smaller than the policy dead band is not an episode.
const GAP_FLOOR: u64 = 16 * MIB;
/// Consecutive gap time for a run to count as an episode (the 5 Hz retry loop is
/// "persistent", not a step transient — S4 showed steps close in well under this).
const EPISODE_MIN: Duration = Duration::from_secs(5);

fn observe_minutes() -> u64 {
    std::env::var("LIMINA_BENCH_S8_MINUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(35)
}

#[test]
fn s8_desktop_steady_state() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s8_desktop_steady_state: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if tier() != Tier::Enhanced {
        eprintln!("SKIPPED s8_desktop_steady_state: enhanced-only (LIMINA_BENCH_TIER=enhanced)");
        return;
    }
    if !limina_test::require_hvf_or_skip("s8_desktop_steady_state") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED s8_desktop_steady_state: no KosmicKrisp ICD under /Volumes/mesa-cs \
             (run scripts/ensure-mesa-cs.sh) — the desktop shape needs the real seated session"
        );
        return;
    }
    let run = BenchRun::create("s8enh").expect("creating the bench run dir");
    let trace_path = run.dir().join("trace.jsonl");
    let cfg = match tier_config() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_memory(MIN_MIB, MAX_MIB)
            .with_balloon_control()
            .with_control_socket()
            .with_reclaim("moderate")
            .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string()),
        Err(e) => {
            eprintln!("SKIPPED s8_desktop_steady_state: {e}");
            return;
        }
    };
    let minutes = observe_minutes();
    eprintln!(
        "S8 desktop steady state: moderate, {minutes} min observation; artifacts -> {:?}",
        run.dir()
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    let stamp = verify_tier(&guest, &cfg).expect("tier positive control");
    // Let gdm autologin seat the session before shaping memory.
    std::thread::sleep(Duration::from_secs(30));
    let session = guest
        .ssh_exec("loginctl list-sessions --no-legend 2>/dev/null | tr '\\n' ';'")
        .unwrap_or_default()
        .trim()
        .to_string();
    eprintln!("  sessions: {session}");

    let sampler = GuestSampler::start(&guest, 500).expect("starting the guest sampler");

    // Warm the cache with REAL file reads — the desktop equivalent of uptime. Reads more
    // than fits so the cache ends genuinely full; bounded so a slow disk can't eat the run.
    eprintln!("  warming the page cache (tar-read of /usr + /var/lib)");
    guest
        .ssh_exec(
            "timeout 300 tar cf /dev/null /usr 2>/dev/null; \
             timeout 60 tar cf /dev/null /var/lib 2>/dev/null; true",
        )
        .expect("cache warm-up read");
    let shape = guest
        .ssh_exec(
            "awk '/MemTotal|MemFree|MemAvailable|^Cached/{printf \"%s%s \", $1, $2}' /proc/meminfo",
        )
        .unwrap_or_default();
    eprintln!("  post-warm shape: {}", shape.trim());

    // The steady state: nothing synthetic. Sample the host at 1 Hz and let the real
    // closed loop (agent -> policy -> driver) do whatever it does.
    let t_start = now_ms();
    let t_wall = Instant::now();
    let budget = Duration::from_secs(minutes * 60);
    let mut host_samples: Vec<HostSample> = Vec::new();
    while t_wall.elapsed() < budget {
        std::thread::sleep(Duration::from_millis(1000));
        if let Ok(s) = sample_host(&guest) {
            host_samples.push(s);
        }
    }

    // Collect.
    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write("guest.csv", &guest_csv).unwrap();
    run.write_host_samples(&host_samples).unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write("journal.txt", &journal).unwrap();
    let trace_raw = std::fs::read_to_string(&trace_path).unwrap_or_default();
    let trace = parse_trace(&trace_raw);
    assert!(
        !trace.is_empty(),
        "the supervisor journaled no decisions across {minutes} min — the closed loop \
         was not running, this measured an empty room"
    );

    // Gap episodes: maximal runs of consecutive host samples with target-actual above the
    // dead band, EPISODE_MIN or longer — the "policy parks an unfillable target" signature.
    let mut episodes: Vec<(u64, u64)> = Vec::new(); // (start_ms, len_ms)
    let mut cur: Option<(u64, u64)> = None;
    for s in &host_samples {
        if s.target.saturating_sub(s.actual) > GAP_FLOOR {
            cur = Some(match cur {
                Some((start, _)) => (start, s.ts_ms.saturating_sub(start)),
                None => (s.ts_ms, 0),
            });
        } else if let Some((start, len)) = cur.take() {
            if len >= EPISODE_MIN.as_millis() as u64 {
                episodes.push((start, len));
            }
        }
    }
    if let Some((start, len)) = cur {
        if len >= EPISODE_MIN.as_millis() as u64 {
            episodes.push((start, len));
        }
    }
    let gap_ms: u64 = episodes.iter().map(|(_, len)| len).sum();
    let episodes_json: Vec<String> = episodes
        .iter()
        .map(|(start, len)| {
            json_object(&[("start_ms", start.to_string()), ("len_ms", len.to_string())])
        })
        .collect();

    // Hold-reason histogram: what gate the policy sat behind, per consumed report.
    let mut holds: Vec<(String, u64)> = Vec::new();
    for e in &trace {
        match holds.iter_mut().find(|(d, _)| *d == e.decision) {
            Some((_, n)) => *n += 1,
            None => holds.push((e.decision.clone(), 1)),
        }
    }
    holds.sort_by_key(|h| std::cmp::Reverse(h.1));
    let holds_json: Vec<String> = holds
        .iter()
        .map(|(d, n)| format!("{{\"{d}\":{n}}}"))
        .collect();

    let puff_lines = journal
        .lines()
        .filter(|l| l.contains("Out of puff"))
        .count();
    let t_end = now_ms();
    let final_stats = host_samples.last().copied().unwrap_or_default();
    let sent_sets = trace
        .iter()
        .filter(|e| e.sent && e.new_target_pages.is_some())
        .count();

    let mut entries = stamp.entries();
    entries.extend([
        ("scenario", "\"s8-desktop-steady-state\"".to_string()),
        ("minutes", minutes.to_string()),
        ("sessions", format!("\"{session}\"")),
        ("t_start_ms", t_start.to_string()),
        ("gap_episodes", episodes.len().to_string()),
        ("gap_episode_list", format!("[{}]", episodes_json.join(","))),
        ("gap_total_ms", gap_ms.to_string()),
        (
            "gap_residency_pct",
            format!(
                "{:.2}",
                100.0 * gap_ms as f64 / (t_end.saturating_sub(t_start)).max(1) as f64
            ),
        ),
        ("out_of_puff_lines", puff_lines.to_string()),
        ("sent_target_changes", sent_sets.to_string()),
        ("hold_histogram", format!("[{}]", holds_json.join(","))),
        ("final_target", final_stats.target.to_string()),
        ("final_actual", final_stats.actual.to_string()),
        ("final_reclaimed", final_stats.reclaimed.to_string()),
        (
            "kswapd_cpu_ticks_delta",
            counter_delta(&guest_samples, |s| s.kswapd_cpu_ticks, t_start, t_end).to_string(),
        ),
        (
            "pgsteal_direct",
            counter_delta(&guest_samples, |s| s.pgsteal_direct, t_start, t_end).to_string(),
        ),
        ("guest_samples", guest_samples.len().to_string()),
        ("host_samples", host_samples.len().to_string()),
    ]);
    let metrics = json_object(&entries);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S8 metrics ==\n{metrics}");

    let outcome = guest.shutdown(Duration::from_secs(20));
    eprintln!("teardown: {outcome:?}");
}
