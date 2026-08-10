// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S1 — mechanism deflate/inflate, no policy**
//! (`docs/design/balloon-bench.md` §6).
//!
//! Writes balloon targets directly at the worker's control socket (the supervisor's PSI
//! policy is NOT running: no `--memory`), so what it measures is the pure mechanism — the
//! D4 / I3 stages: guest driver fill/leak throughput, `actual` tracking lag, and the host's
//! madvise reclaim per ballooned MiB. Everything upstream (detection, transport, decision)
//! is exactly what S1 *excludes*; the split is the point.
//!
//! Two full inflate→deflate cycles (cold, then warm — the second cycle sees whatever state
//! the first left in the guest allocator and the host coalescer). Artifacts per run:
//! guest.csv (250 ms in-guest sampler), host.csv (200 ms target/actual/reclaimed/footprint),
//! journal.txt (kernel balloon chatter, unix timestamps), metrics.json.
//!
//! A measurement run, not a guard: gated behind `LIMINA_BALLOON_BENCH=1` on top of the HVF
//! gate, so the default suite never pays for it. Asserts sanity only (the balloon really
//! inflated and really emptied — a run that measured nothing must not look green).

use std::time::{Duration, Instant};

use limina_test::bench::{
    fetch_balloon_journal, json_object, mib_per_s, now_ms, sample_host, BenchRun, GuestSampler,
    HostSample,
};
use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;
/// Guest RAM: comfortably above the balloon excursion so the idle guest never fights S1.
const RAM_MIB: usize = 6144;
/// The balloon excursion each cycle inflates to and deflates from.
const TARGET_BYTES: u64 = 3 * 1024 * MIB;
/// `actual` must reach this fraction of the target for the inflate leg to count as filled.
/// The last few MiB can trickle (the driver allocates opportunistically); the throughput
/// number is quoted to 99% fill.
const FILL_NUM: u64 = 99;
const FILL_DEN: u64 = 100;

struct Leg {
    /// Command instant (host clock).
    t_cmd_ms: u64,
    /// First sample at/after the leg's completion condition.
    t_done_ms: u64,
    /// Bytes moved (excursion size actually observed).
    bytes: u64,
    /// Worker footprint at command and at completion (bytes; 0 = unreadable).
    footprint_cmd: u64,
    footprint_done: u64,
    /// Cumulative `reclaimed=` at command and completion.
    reclaimed_cmd: u64,
    reclaimed_done: u64,
}

impl Leg {
    fn elapsed(&self) -> Duration {
        Duration::from_millis(self.t_done_ms.saturating_sub(self.t_cmd_ms))
    }
}

/// Drive one leg: command `target_bytes`, poll host samples at ~200 ms into `samples` until
/// `done(actual)` holds (or panic at `timeout` — S1 without a completed leg measured nothing).
fn drive_leg(
    guest: &Guest,
    samples: &mut Vec<HostSample>,
    target_bytes: u64,
    timeout: Duration,
    label: &str,
    done: impl Fn(u64) -> bool,
) -> Leg {
    let before = sample_host(guest).expect("host sample before leg");
    samples.push(before);
    let t_cmd_ms = now_ms();
    guest
        .set_balloon_target(target_bytes)
        .expect("commanding balloon target");
    let deadline = Instant::now() + timeout;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let s = sample_host(guest).expect("host sample during leg");
        samples.push(s);
        if done(s.actual) {
            return Leg {
                t_cmd_ms,
                t_done_ms: s.ts_ms,
                bytes: s.actual.abs_diff(before.actual),
                footprint_cmd: before.worker_footprint,
                footprint_done: s.worker_footprint,
                reclaimed_cmd: before.reclaimed,
                reclaimed_done: s.reclaimed,
            };
        }
        assert!(
            Instant::now() < deadline,
            "S1 {label} leg did not complete within {timeout:?}: \
             target={target_bytes} actual={} — the mechanism stalled, no number to report",
            s.actual
        );
    }
}

#[test]
fn s1_mechanism_deflate_inflate() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s1_mechanism_deflate_inflate: set LIMINA_BALLOON_BENCH=1 (bench runs are on-demand, see docs/design/balloon-bench.md §9)");
        return;
    }
    if !limina_test::require_hvf_or_skip("s1_mechanism_deflate_inflate") {
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = RAM_MIB;
            cfg.with_net().with_balloon_control()
        }
        Err(e) => {
            eprintln!("SKIPPED s1_mechanism_deflate_inflate: {e}");
            return;
        }
    };
    let run = BenchRun::create("s1").expect("creating the bench run dir");
    eprintln!(
        "S1 mechanism bench: stock baseline, {RAM_MIB} MiB RAM, {} MiB excursion, no policy; artifacts -> {:?}",
        TARGET_BYTES / MIB,
        run.dir()
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    // Let boot-time churn settle so the idle baseline is honest.
    std::thread::sleep(Duration::from_secs(10));

    let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
    let mut host_samples: Vec<HostSample> = Vec::new();
    let mut cycles: Vec<(Leg, Leg)> = Vec::new();

    for cycle in 0..2 {
        eprintln!("cycle {cycle}: inflating to {} MiB", TARGET_BYTES / MIB);
        let inflate = drive_leg(
            &guest,
            &mut host_samples,
            TARGET_BYTES,
            Duration::from_secs(180),
            "inflate",
            |actual| actual >= TARGET_BYTES / FILL_DEN * FILL_NUM,
        );
        eprintln!(
            "  inflate: {} MiB in {:.2?} = {:.0} MiB/s",
            inflate.bytes / MIB,
            inflate.elapsed(),
            mib_per_s(inflate.bytes, inflate.elapsed())
        );
        // Settle so the deflate leg starts from a quiescent balloon (and the host coalescer
        // has caught up — reclaimed keeps counting after actual converges).
        std::thread::sleep(Duration::from_secs(5));

        eprintln!("cycle {cycle}: deflating to 0");
        let deflate = drive_leg(
            &guest,
            &mut host_samples,
            0,
            Duration::from_secs(120),
            "deflate",
            |actual| actual == 0,
        );
        eprintln!(
            "  deflate: {} MiB in {:.2?} = {:.0} MiB/s",
            deflate.bytes / MIB,
            deflate.elapsed(),
            mib_per_s(deflate.bytes, deflate.elapsed())
        );
        std::thread::sleep(Duration::from_secs(5));
        cycles.push((inflate, deflate));
    }

    // Collect artifacts.
    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write("guest.csv", &guest_csv)
        .expect("writing guest.csv");
    run.write_host_samples(&host_samples)
        .expect("writing host.csv");
    let journal = fetch_balloon_journal(&guest).expect("fetching balloon journal");
    run.write("journal.txt", &journal)
        .expect("writing journal.txt");

    // Metrics: one JSON object per cycle leg, plus context.
    let leg_json = |l: &Leg| {
        json_object(&[
            ("t_cmd_ms", l.t_cmd_ms.to_string()),
            ("t_done_ms", l.t_done_ms.to_string()),
            ("bytes", l.bytes.to_string()),
            (
                "elapsed_ms",
                (l.t_done_ms.saturating_sub(l.t_cmd_ms)).to_string(),
            ),
            (
                "mib_per_s",
                format!("{:.1}", mib_per_s(l.bytes, l.elapsed())),
            ),
            ("footprint_cmd", l.footprint_cmd.to_string()),
            ("footprint_done", l.footprint_done.to_string()),
            ("reclaimed_cmd", l.reclaimed_cmd.to_string()),
            ("reclaimed_done", l.reclaimed_done.to_string()),
        ])
    };
    let cycles_json: Vec<String> = cycles
        .iter()
        .map(|(i, d)| json_object(&[("inflate", leg_json(i)), ("deflate", leg_json(d))]))
        .collect();
    let puff_lines = journal
        .lines()
        .filter(|l| l.contains("Out of puff"))
        .count();
    let metrics = json_object(&[
        ("scenario", "\"s1-mechanism\"".to_string()),
        ("guest_tier", "\"stock-4k\"".to_string()),
        ("ram_mib", RAM_MIB.to_string()),
        ("target_bytes", TARGET_BYTES.to_string()),
        ("cycles", format!("[{}]", cycles_json.join(","))),
        ("out_of_puff_lines", puff_lines.to_string()),
        ("guest_samples", guest_samples.len().to_string()),
        ("host_samples", host_samples.len().to_string()),
    ]);
    run.write("metrics.json", &metrics)
        .expect("writing metrics.json");

    eprintln!("== S1 metrics ==\n{metrics}");

    // Sanity: both cycles really moved the balloon both ways, and the recorder recorded.
    for (n, (inflate, deflate)) in cycles.iter().enumerate() {
        assert!(
            inflate.bytes >= TARGET_BYTES / FILL_DEN * FILL_NUM,
            "cycle {n}: inflate leg moved only {} bytes",
            inflate.bytes
        );
        assert!(
            deflate.bytes >= TARGET_BYTES / FILL_DEN * FILL_NUM,
            "cycle {n}: deflate leg moved only {} bytes",
            deflate.bytes
        );
    }
    assert!(
        guest_samples.len() as u64 >= host_samples.len() as u64 / 4,
        "guest sampler produced suspiciously few samples ({} vs {} host): it died mid-run",
        guest_samples.len(),
        host_samples.len()
    );

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
