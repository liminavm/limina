// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Dynamic-memory stress on a **windowed, seated desktop**: the shape users run, with nothing
//! played by the harness. On the enhanced tier the guest's own limina-agent reports pressure; on
//! the stock tier the host reads the same files through the stock qemu-guest-agent, without PSI
//! on an Enforcing Fedora guest. Either way the policy acts on what it is told
//! (`docs/design/balloon-bench.md`, `docs/design/guest-ram-objects.md`).
//!
//! Each point is one fresh boot of `MIN_MIB..MAX_MIB` with one reclaim mode, on one backend: the
//! balloon over all of RAM, or the virtio-mem split (`LIMINA_VIRTIO_MEM=split`). Scenarios:
//! - **burst**: with memory reclaimed, allocate `BURST_MIB` at a fixed rate. Does the policy give
//!   memory back in time, or does the guest OOM-kill?
//! - **cache**: let reclaim settle on an idle desktop, then re-read a cold file larger than what
//!   that leaves the guest. Does the policy give memory back, or keep the guest thrashing (thrash
//!   is io pressure; memory pressure stays quiet)?
//! - **stream**: let reclaim settle, then read more than the whole guest once, as a backup does.
//!   Nothing is re-read, so the cache is not too small for anything — the shape a give-back
//!   trigger must not mistake for thrash.
//! - **idle**: an idle desktop for `LIMINA_STRESS_IDLE_MIN` minutes (default 20). Does reclaim
//!   settle, or keep reversing?
//! - **host**: the host's level goes normal, warn, critical, normal (`LIMINA_HOST_PRESSURE=@file`).
//!   Does reclaim follow it?
//!
//! These are measurement runs: each point writes its series and a metrics object under the run
//! directory, and asserts only that it measured what it claims to.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF. `LIMINA_BENCH_TIER` (stock|enhanced),
//! `LIMINA_STRESS_BACKENDS` (default `balloon,split`), `LIMINA_STRESS_MODES` (default `moderate`),
//! `LIMINA_STRESS_SCENARIOS` (default `burst,cache,idle,host`; `stream` on request), `LIMINA_STRESS_RATES` (burst
//! MiB/s, 0 = unthrottled; default `512,2048,0`). A window opens: run it alone.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use limina_test::Guest;
use limina_test::bench::{
    BenchRun, BurstStatus, GuestSample, GuestSampler, HostSample, TraceEvent, burst_status,
    count_oom_since, counter_delta, fetch_balloon_journal, first_target_decrease_after,
    guest_epoch_secs, json_object, kill_burst, now_ms, parse_trace, psi_integral_pct_s,
    sample_host, start_burst, target_reversals, tier, tier_config, verify_tier,
};

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 8192;
/// The burst: most of what the policy may reclaim, so it cannot fit without a release.
const BURST_MIB: u64 = 4096;
/// The re-read working set: more than a settled guest has available (~1.3 GiB at 2..8 GiB),
/// and well under what a guest that gives memory back has.
const FILE_MIB: u64 = 2048;
/// Free disk the working set must leave the guest: a nearly full btrfs is its own io pressure.
const DISK_SLACK_MIB: u64 = 1024;
/// Reclaim has settled when what is held has not moved by more than this for [`SETTLE_QUIET`].
const SETTLE_SLOP: u64 = 64 * MIB;
const SETTLE_QUIET: Duration = Duration::from_secs(90);
/// Idle reclaim on a 2..8 GiB desktop measured ~5 min from first step to last.
const SETTLE_WITHIN: Duration = Duration::from_secs(600);
const THRASH_SECS: u64 = 180;
/// The one-pass read: more than `MAX_MIB`, so nothing it reads can still be cached when it ends.
const STREAM_MIB: u64 = 10240;
/// Refault rates are judged over this window, the policy's PSI avg10 horizon.
const REFAULT_WINDOW_MS: u64 = 10_000;
/// How much the policy must have reclaimed before a burst starts.
const RECLAIMED_FLOOR: u64 = 2048 * MIB;
/// /var/tmp, not /tmp: Fedora's /tmp is tmpfs, whose pages are not reclaimable page cache.
const THRASH_BIN: &str = "/var/tmp/limina-thrash.bin";
const THRASH_OUT: &str = "/var/tmp/limina-thrash.out";

fn list(var: &str, default: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

struct Point {
    backend: String,
    mode: String,
    scenario: String,
    rate_mib_s: u64,
}

impl Point {
    fn tag(&self) -> String {
        match self.scenario.as_str() {
            "burst" => format!("{}-{}-burst{}", self.backend, self.mode, self.rate_mib_s),
            s => format!("{}-{}-{s}", self.backend, self.mode),
        }
    }
}

fn points() -> Vec<Point> {
    let mut out = Vec::new();
    for backend in list("LIMINA_STRESS_BACKENDS", "balloon,split") {
        assert!(
            backend == "balloon" || backend == "split",
            "unknown backend {backend:?}"
        );
        for mode in list("LIMINA_STRESS_MODES", "moderate") {
            for scenario in list("LIMINA_STRESS_SCENARIOS", "burst,cache,idle,host") {
                let rates: Vec<u64> = match scenario.as_str() {
                    "burst" => list("LIMINA_STRESS_RATES", "512,2048,0")
                        .iter()
                        .map(|r| r.parse().expect("a rate in MiB/s"))
                        .collect(),
                    "cache" | "stream" | "idle" | "host" => vec![0],
                    other => panic!("unknown scenario {other:?}"),
                };
                for rate_mib_s in rates {
                    out.push(Point {
                        backend: backend.clone(),
                        mode: mode.clone(),
                        scenario: scenario.clone(),
                        rate_mib_s,
                    });
                }
            }
        }
    }
    out
}

/// Write the injected host level atomically: a truncated file reads as Normal.
fn write_level(path: &Path, level: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{level}\n")).expect("writing the host level");
    std::fs::rename(&tmp, path).expect("placing the host level");
}

struct Run<'a> {
    guest: Guest,
    sampler: GuestSampler,
    host: Vec<HostSample>,
    trace_path: PathBuf,
    level_path: PathBuf,
    stamp: Vec<(&'static str, String)>,
    bench: &'a BenchRun,
    tag: String,
}

impl Run<'_> {
    fn boot<'a>(bench: &'a BenchRun, p: &Point) -> Run<'a> {
        let tag = p.tag();
        eprintln!("== {tag} ({}) ==", tier().label());
        let trace_path = bench.dir().join(format!("trace-{tag}.jsonl"));
        let level_path = bench.dir().join(format!("host-level-{tag}"));
        write_level(&level_path, "normal");
        let mut cfg = tier_config()
            .expect("tier disk (checked before the run)")
            .with_net()
            .with_memory(MIN_MIB, MAX_MIB)
            .with_balloon_control()
            .with_reclaim(&p.mode)
            .with_windowed_coexist_display(1280, 800)
            .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string())
            .with_env(
                "LIMINA_HOST_PRESSURE",
                &format!("@{}", level_path.display()),
            );
        if p.backend == "split" {
            cfg = cfg.with_env("LIMINA_VIRTIO_MEM", "split");
        }
        cfg.ram_mib = MAX_MIB;
        let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
        guest
            .wait_for_ssh(Duration::from_secs(300))
            .expect("guest sshd never became reachable");
        let stamp = verify_tier(&guest, &cfg)
            .expect("tier positive control")
            .entries();
        let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
        let mut run = Run {
            guest,
            sampler,
            host: Vec::new(),
            trace_path,
            level_path,
            stamp,
            bench,
            tag,
        };
        if p.backend == "split" {
            // The guest plugs its region as it comes up; until it has, a reading of reclaim is
            // a reading of the plug.
            let plugged = run.watch_until(Duration::from_secs(120), |s| {
                s.mem_region > 0 && s.mem_plugged >= s.mem_requested
            });
            let last = run.host.last().copied().unwrap_or_default();
            assert!(
                plugged,
                "{}: the split guest did not plug its region (region {} MiB, plugged {} MiB)",
                run.tag,
                last.mem_region / MIB,
                last.mem_plugged / MIB
            );
        }
        run
    }

    fn sample(&mut self) -> HostSample {
        let s = sample_host(&self.guest).unwrap_or_default();
        self.host.push(s);
        s
    }

    /// Sample the host every second until `done` or `within` passes; returns whether it held.
    fn watch_until(&mut self, within: Duration, done: impl Fn(&HostSample) -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            let s = self.sample();
            if done(&s) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn watch_for(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.sample();
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Wait for the policy to reclaim [`RECLAIMED_FLOOR`]; returns what is held when the wait
    /// ends.
    fn reclaim_first(&mut self) -> u64 {
        self.watch_until(Duration::from_secs(300), |s| s.held() >= RECLAIMED_FLOOR);
        self.host.last().map(HostSample::held).unwrap_or(0)
    }

    /// Watch until what is held stops moving ([`SETTLE_QUIET`] within [`SETTLE_SLOP`]) or
    /// [`SETTLE_WITHIN`] passes; returns what is held and whether it settled.
    fn settle(&mut self) -> (u64, bool) {
        let deadline = Instant::now() + SETTLE_WITHIN;
        let (mut anchor, mut since) = (self.sample().held(), Instant::now());
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let held = self.sample().held();
            if held.abs_diff(anchor) > SETTLE_SLOP {
                (anchor, since) = (held, Instant::now());
            } else if since.elapsed() >= SETTLE_QUIET {
                return (held, true);
            }
            if Instant::now() >= deadline {
                return (held, false);
            }
        }
    }

    /// Collect the series, write them, and return the point's metrics with what `extra` makes
    /// of the guest samples and the decision trace appended.
    fn finish(
        mut self,
        extra: impl FnOnce(&[GuestSample], &[TraceEvent]) -> Vec<(&'static str, String)>,
    ) -> String {
        let (csv, samples) = self
            .sampler
            .stop_and_fetch(&self.guest)
            .expect("fetching guest CSV");
        let tag = self.tag.clone();
        self.bench.write(&format!("guest-{tag}.csv"), &csv).unwrap();
        self.bench
            .write_host_samples_named(&format!("host-{tag}.csv"), &self.host)
            .unwrap();
        let journal = fetch_balloon_journal(&self.guest).unwrap_or_default();
        self.bench
            .write(&format!("journal-{tag}.txt"), &journal)
            .unwrap();
        let trace = parse_trace(&std::fs::read_to_string(&self.trace_path).unwrap_or_default());
        let held_max = self.host.iter().map(HostSample::held).max().unwrap_or(0);
        let footprint_min = self
            .host
            .iter()
            .map(|s| s.worker_footprint)
            .filter(|&f| f > 0)
            .min()
            .unwrap_or(0);
        let mut entries = std::mem::take(&mut self.stamp);
        entries.extend([
            ("point", format!("\"{tag}\"")),
            ("decisions", trace.len().to_string()),
            ("sent", trace.iter().filter(|e| e.sent).count().to_string()),
            ("reversals", target_reversals(&trace).to_string()),
            ("held_max_mib", (held_max / MIB).to_string()),
            ("footprint_min_mib", (footprint_min / MIB).to_string()),
        ]);
        entries.extend(extra(&samples, &trace));
        let json = json_object(&entries);
        eprintln!("  {json}");
        let _ = self.guest.shutdown(Duration::from_secs(15));
        json
    }
}

fn psi(samples: &[GuestSample], from: u64, to: u64) -> Vec<(&'static str, String)> {
    vec![
        (
            "psi_mem_some_pct_s",
            format!(
                "{:.1}",
                psi_integral_pct_s(samples, |s| s.mem_some_avg10, from, to)
            ),
        ),
        (
            "psi_io_some_pct_s",
            format!(
                "{:.1}",
                psi_integral_pct_s(samples, |s| s.io_some_avg10, from, to)
            ),
        ),
        (
            "pgmajfault",
            counter_delta(samples, |s| s.pgmajfault, from, to).to_string(),
        ),
        (
            "pgsteal_direct",
            counter_delta(samples, |s| s.pgsteal_direct, from, to).to_string(),
        ),
        (
            "refault_file",
            counter_delta(samples, |s| s.workingset_refault_file, from, to).to_string(),
        ),
        (
            "refault_file_peak_per_s",
            peak_rate(samples, |s| s.workingset_refault_file, from, to).to_string(),
        ),
        (
            "refault_anon",
            counter_delta(samples, |s| s.workingset_refault_anon, from, to).to_string(),
        ),
    ]
}

/// The highest per-second rate of a counter over any [`REFAULT_WINDOW_MS`] span in `from..to`.
fn peak_rate(samples: &[GuestSample], f: impl Fn(&GuestSample) -> u64, from: u64, to: u64) -> u64 {
    let span: Vec<_> = samples
        .iter()
        .filter(|s| s.ts_ms >= from && s.ts_ms <= to)
        .collect();
    let mut peak = 0;
    let mut j = 0;
    for (i, s) in span.iter().enumerate() {
        while span[j].ts_ms + REFAULT_WINDOW_MS < s.ts_ms {
            j += 1;
        }
        let ms = s.ts_ms - span[j].ts_ms;
        if i > j && ms >= REFAULT_WINDOW_MS / 2 {
            peak = peak.max(f(s).saturating_sub(f(span[j])) * 1000 / ms);
        }
    }
    peak
}

fn burst(bench: &BenchRun, p: &Point) -> String {
    let mut run = Run::boot(bench, p);
    let held_at_start = run.reclaim_first();
    let mark = guest_epoch_secs(&run.guest).expect("guest clock");
    let t0 = start_burst(&run.guest, BURST_MIB * MIB, p.rate_mib_s * MIB).expect("the burst");
    let deadline = Instant::now() + Duration::from_secs(300);
    let outcome = loop {
        std::thread::sleep(Duration::from_secs(1));
        run.sample();
        match burst_status(&run.guest).unwrap_or(BurstStatus::Died) {
            BurstStatus::Complete(_) => break "ok",
            BurstStatus::Died => break "died",
            BurstStatus::Running(_) => {}
        }
        if Instant::now() >= deadline {
            break "timeout";
        }
    };
    // The burst holds its memory: watch the policy under the occupancy.
    run.watch_for(Duration::from_secs(20));
    let end = now_ms();
    let oom = count_oom_since(&run.guest, &mark);
    kill_burst(&run.guest).ok();
    let rate = p.rate_mib_s;
    run.finish(|samples, trace| {
        let detection = first_target_decrease_after(trace, t0)
            .map_or("null".into(), |(ts, _, _)| {
                ts.saturating_sub(t0).to_string()
            });
        let min_avail = samples
            .iter()
            .filter(|s| s.ts_ms >= t0)
            .map(|s| s.mem_available_kib)
            .min()
            .unwrap_or(0);
        let mut extra = vec![
            ("rate_mib_s", rate.to_string()),
            ("held_at_start_mib", (held_at_start / MIB).to_string()),
            ("outcome", format!("\"{outcome}\"")),
            ("oom_kills", oom.to_string()),
            ("detection_ms", detection),
            ("min_avail_mib", (min_avail / 1024).to_string()),
        ];
        extra.extend(psi(samples, t0, end));
        extra
    })
}

fn cache(bench: &BenchRun, p: &Point) -> String {
    let mut run = Run::boot(bench, p);
    let avail_mib: u64 = run
        .guest
        .ssh_exec("df --output=avail -m /var/tmp | tail -n1")
        .expect("reading free disk")
        .trim()
        .parse()
        .expect("df's avail column");
    assert!(
        avail_mib >= FILE_MIB + DISK_SLACK_MIB,
        "{}: the guest disk has {avail_mib} MiB free, short of {FILE_MIB} MiB + {DISK_SLACK_MIB} MiB slack",
        run.tag
    );
    // Written, then dropped from the cache: the policy settles on an idle desktop, not on the
    // writer's dirty pages, and the loop starts cold.
    run.guest
        .ssh_exec(&format!(
            "dd if=/dev/urandom of={THRASH_BIN} bs=1M count={FILE_MIB} status=none && sync && \
             echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null && echo ok"
        ))
        .expect("writing the working set");
    let (held_at_start, settled) = run.settle();
    run.guest
        .ssh_exec(&format!(
            "rm -f {THRASH_OUT}; setsid nohup bash -c 'while true; do cat {THRASH_BIN} > /dev/null; \
             echo pass $(date +%s%3N) >> {THRASH_OUT}; done' </dev/null >/dev/null 2>&1 & echo spawned"
        ))
        .expect("starting the re-read loop");
    let t0 = now_ms();
    run.watch_for(Duration::from_secs(THRASH_SECS));
    let end = now_ms();
    run.guest
        .ssh_exec("pkill -f '[c]at /var/tmp/limina-thrash' ; pkill -f '[l]imina-thrash.out' ; true")
        .ok();
    let passes: Vec<u64> = run
        .guest
        .ssh_exec(&format!("cat {THRASH_OUT}"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("pass ")?.trim().parse().ok())
        .collect();
    // Pass times in order; the last quarter shows where the policy left the guest.
    let gaps: Vec<u64> = passes.windows(2).map(|w| w[1] - w[0]).collect();
    let median = |g: &[u64]| {
        let mut g = g.to_vec();
        g.sort_unstable();
        g.get(g.len() / 2).copied().unwrap_or(0)
    };
    let (all, tail) = (median(&gaps), median(&gaps[gaps.len() * 3 / 4..]));
    let held_end = run.host.last().map(HostSample::held).unwrap_or(0);
    let count = passes.len();
    run.finish(|samples, trace| {
        let givebacks = trace
            .iter()
            .filter(|e| e.ts_ms >= t0 && e.decision == "giveback")
            .count();
        let mut extra = vec![
            ("held_at_start_mib", (held_at_start / MIB).to_string()),
            ("settled", settled.to_string()),
            ("held_end_mib", (held_end / MIB).to_string()),
            ("passes", count.to_string()),
            ("pass_median_ms", all.to_string()),
            ("pass_tail_median_ms", tail.to_string()),
            ("givebacks", givebacks.to_string()),
        ];
        extra.extend(psi(samples, t0, end));
        extra
    })
}

fn stream(bench: &BenchRun, p: &Point) -> String {
    let mut run = Run::boot(bench, p);
    let (held_at_start, settled) = run.settle();
    // Detached, so the host side keeps being sampled while it runs.
    run.guest
        .ssh_exec(&format!(
            "rm -f {THRASH_OUT}; setsid nohup bash -c 'sudo dd if=/dev/vda of=/dev/null bs=1M \
             count={STREAM_MIB} status=none; echo done > {THRASH_OUT}' </dev/null >/dev/null 2>&1 & \
             echo spawned"
        ))
        .expect("starting the one-pass read");
    let t0 = now_ms();
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        run.watch_for(Duration::from_secs(5));
        let done = run
            .guest
            .ssh_exec(&format!("cat {THRASH_OUT} 2>/dev/null"))
            .unwrap_or_default();
        if done.trim() == "done" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{}: the one-pass read never finished",
            run.tag
        );
    }
    let end = now_ms();
    let held_end = run.host.last().map(HostSample::held).unwrap_or(0);
    run.finish(|samples, trace| {
        let givebacks = trace
            .iter()
            .filter(|e| e.ts_ms >= t0 && e.decision == "giveback")
            .count();
        let mut extra = vec![
            ("held_at_start_mib", (held_at_start / MIB).to_string()),
            ("settled", settled.to_string()),
            ("held_end_mib", (held_end / MIB).to_string()),
            ("stream_ms", (end - t0).to_string()),
            ("givebacks", givebacks.to_string()),
        ];
        extra.extend(psi(samples, t0, end));
        extra
    })
}

fn idle(bench: &BenchRun, p: &Point) -> String {
    let minutes: u64 = std::env::var("LIMINA_STRESS_IDLE_MIN")
        .ok()
        .and_then(|m| m.parse().ok())
        .unwrap_or(20);
    let mut run = Run::boot(bench, p);
    let t0 = now_ms();
    run.watch_for(Duration::from_secs(minutes * 60));
    // Settling is judged on the second half: the first is the climb.
    let half = t0 + minutes * 30_000;
    let held: Vec<u64> = run
        .host
        .iter()
        .filter(|s| s.ts_ms >= half)
        .map(HostSample::held)
        .collect();
    let (lo, hi) = (
        held.iter().min().copied().unwrap_or(0),
        held.iter().max().copied().unwrap_or(0),
    );
    let end = now_ms();
    run.finish(|samples, trace| {
        let late: Vec<_> = trace.iter().filter(|e| e.ts_ms >= half).cloned().collect();
        let mut extra = vec![
            ("idle_minutes", minutes.to_string()),
            (
                "late_sent",
                late.iter().filter(|e| e.sent).count().to_string(),
            ),
            ("late_reversals", target_reversals(&late).to_string()),
            ("late_held_min_mib", (lo / MIB).to_string()),
            ("late_held_max_mib", (hi / MIB).to_string()),
        ];
        extra.extend(psi(samples, half, end));
        extra
    })
}

fn host(bench: &BenchRun, p: &Point) -> String {
    let mut run = Run::boot(bench, p);
    let mut extra = Vec::new();
    for (level, secs, key) in [
        ("normal", 120, "held_normal_mib"),
        ("warn", 120, "held_warn_mib"),
        ("critical", 120, "held_critical_mib"),
        ("normal", 180, "held_recovered_mib"),
    ] {
        write_level(&run.level_path, level);
        run.watch_for(Duration::from_secs(secs));
        let held = run.host.last().map(HostSample::held).unwrap_or(0);
        eprintln!("  {level}: {} MiB given back", held / MIB);
        extra.push((key, (held / MIB).to_string()));
    }
    run.finish(|_, _| extra)
}

#[test]
fn balloon_stress_on_a_seated_desktop() {
    let name = "balloon_stress_on_a_seated_desktop";
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED {name}: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    if let Err(e) = tier_config() {
        eprintln!("SKIPPED {name}: {e}");
        return;
    }
    let bench = BenchRun::create(&format!("stress{}", tier().suffix())).expect("the run dir");
    eprintln!("stress ({} tier) -> {:?}", tier().label(), bench.dir());
    let mut results = Vec::new();
    for p in points() {
        let json = match p.scenario.as_str() {
            "burst" => burst(&bench, &p),
            "cache" => cache(&bench, &p),
            "stream" => stream(&bench, &p),
            "idle" => idle(&bench, &p),
            "host" => host(&bench, &p),
            _ => unreachable!(),
        };
        results.push(json);
        bench
            .write("metrics.json", &format!("[{}]", results.join(",\n")))
            .unwrap();
    }
}
