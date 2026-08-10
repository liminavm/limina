// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S7 — the `Out of puff` retry loop in isolation**
//! (`docs/design/balloon-bench.md` §2, §6). No policy — targets go straight to the
//! worker's socket, so every observation is the *driver's* behavior.
//!
//! Four phases:
//! - **A (H1 stage-set):** fill the page cache (write + sync a file sized near RAM) so the
//!   guest reaches the H1 shape — MemFree LOW, MemAvailable HIGH.
//! - **B (slow fill):** command a 2 GiB target into that shape. `balloon_page_alloc` is
//!   `__GFP_NORETRY`: it can nibble cache via light direct reclaim but fails easily — this
//!   measures the *fill rate against cache* vs S1's ~1.8 GiB/s against free pages, and how
//!   much `Out of puff` a mere ramp produces (H2's magnitude).
//! - **C (chase):** escalate the target past whatever the driver manages to fill (adaptive
//!   — a fixed guess either OOMs guest daemons or turns out fillable via zram), so
//!   fill_balloon runs its 5 Hz retry loop against the guest's natural ceiling for the
//!   whole hold: journal line rate, kswapd/direct-reclaim cost, plateau level. This is the
//!   **journal channel's positive control** — the run FAILS if no `Out of puff` line
//!   appears, because that means the pipeline (not the driver) is broken.
//! - **D (silence):** re-command exactly the plateau `actual`. The gap closes to zero and
//!   the retry loop must stop: at most a ratelimit straggler in the window. Proves the
//!   fix direction H1 implies (close the gap → the spam stops) at the mechanism level.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF.

use std::time::Duration;

use limina_test::bench::{
    count_oom_since, fetch_balloon_journal, guest_epoch_secs, json_object, mib_per_s, now_ms,
    sample_host, tier, tier_config, verify_tier, BenchRun, GuestSampler, HostSample,
};
use limina_test::Guest;

const MIB: u64 = 1 << 20;

/// The stage sizes are TIER-DEPENDENT: the shape (cache-full guest, chase past the
/// plateau) is the same, but the guest's own working set is not. Stock is a bare console
/// (~200 MiB alive; a RAM−512 cap is survivable, and the baseline image's /var/tmp only
/// holds ~4.4 GiB, so the guest stays at 4 GiB). The enhanced guest runs a real GNOME
/// session + the agent (~1.2 GiB alive): the first enhanced run used the stock cap and
/// squeezed the desktop to 512 MiB — the session swapped to catatonia, the AGENT
/// disconnected and sshd died (a finding in itself: an overshooting target severs the
/// pressure channel). Enhanced runs a 6 GiB guest, a 4 GiB cache file (the 40G image has
/// the disk), and a cap that leaves the desktop 1.5 GiB.
struct Sizes {
    ram_mib: usize,
    cache_file_mib: u64,
    chase_start: u64,
    chase_cap: u64,
    /// Phase-C pinned working set (0 = none). Stock's bare console reaches its natural
    /// plateau unaided; the enhanced 16k+zram guest yields everything reclaimable up to
    /// any survivable cap (measured: 4 escalations to the cap in 1.5 s, zero puff), so
    /// the unfillable gap must be guaranteed by pinning what a real desktop pins.
    ballast_mib: u64,
    /// `swapoff -a` before the stage-set. Stock FALSE (the bare console plateaus on its
    /// own); enhanced TRUE — with zram on there is no wall short of desktop eviction,
    /// because every unpinned page (cache first, then the desktop's anon via zram) is
    /// obtainable to a patient chaser. Swapoff makes anon unreclaimable, so the wall is
    /// real and the chase stalls at it instead of grinding the session out.
    swapoff: bool,
}

fn sizes() -> Sizes {
    match tier() {
        limina_test::bench::Tier::Stock => Sizes {
            ram_mib: 4096,
            cache_file_mib: 3072,
            chase_start: 3072 * MIB,
            chase_cap: (4096 - 512) * MIB,
            ballast_mib: 0,
            swapoff: false,
        },
        // CONCLUDED UNCONSTRUCTIBLE (the test SKIPS on this tier; the arm is the
        // record). Six constructions, one convergent finding: **the driver has no
        // self-preservation** — a chased/oversized target takes everything, and the
        // only "plateau" is guest death. The record: (1) stock cap on 4 GiB —
        // desktop squeezed to catatonia, agent+sshd died (s7enh-1786378025); (2) no
        // pinning — the 16k+zram guest yielded everything to any survivable cap
        // (4 escalations to cap in 1.5 s, zero puff); (3) ballast + zram — the chase
        // ground the DESKTOP out via zram once cache was gone (s7enh-1786380313,
        // again at 10 GiB in s7enh-1786383422); (4) ballast + swapoff on 6 GiB —
        // OOM collapse at the absolute-zero-cache wall (s7enh-1786382977); (5) fixed
        // mid-range target, "the grind" (s7enh-1786383821) — closed silently in
        // seconds: the driver has NO slow-approach state, it fills fast or it fails
        // (the "~33 MiB/s grind" that motivated it was an instrument artifact —
        // actual/60 s on a fill that finished in <1 s); (6) ballast + swapoff +
        // 10 GiB scale (s7enh-1786384443) — every 256 MiB escalation closed in one
        // 200 ms sample, 4096→6400 in <5 s, the OOM killer fed the (unprotected)
        // desktop to the balloon and sshd died. With swap the desktop is evicted,
        // without swap it is killed; either way the wall that protects the guest
        // does not exist in the guest — it has to live in the HOST POLICY (the
        // MemFree-informed target clamp lever). Stock's journal positive control
        // covers the mechanism; the dogfood field spam is the enhanced-tier
        // positive control.
        limina_test::bench::Tier::Enhanced => Sizes {
            ram_mib: 10240,
            cache_file_mib: 8192,
            chase_start: 4096 * MIB,
            chase_cap: (10240 - 512) * MIB,
            ballast_mib: 3072,
            swapoff: true,
        },
    }
}

const CACHE_FILE: &str = "/var/tmp/limina-cachefill";
/// Phase-B target: should be fillable by nibbling cache, slowly.
const SLOW_FILL_TARGET: u64 = 2048 * MIB;
const CHASE_STEP: u64 = 256 * MIB;

/// Count `Out of puff` journal lines since a guest-epoch watermark. PANICS if the ssh
/// round-trip fails: a dead guest must read as "instrument down", never as a zero — the
/// first enhanced run wedged its guest and phase C reported puff=0 through a reset ssh.
fn puff_since(guest: &Guest, mark: &str) -> u64 {
    guest
        .ssh_exec(&format!(
            "sudo journalctl -k --since=@{mark} 2>/dev/null | grep -c 'Out of puff' || true"
        ))
        .expect("puff_since: ssh to the guest failed — the count would be a lie, not a zero")
        .trim()
        .parse()
        .expect("puff_since: unparseable grep -c output")
}

/// Stage and spawn an mlocked, OOM-protected anon ballast of `mib` MiB (root: mlockall
/// needs CAP_IPC_LOCK, the -1000 oom_score_adj needs root). Panics if the lock fails —
/// unlocked ballast is quietly reclaimable and the phase-C guarantee evaporates.
///
/// Why it exists: the first enhanced chase proved a 16k+zram guest with nothing pinned
/// yields EVERYTHING — every escalation step filled instantly to the cap (and the cap
/// that would exceed its yield kills the desktop instead, agent and sshd first). The
/// dogfood guest's gap exists because real desktops pin a working set (browser anon
/// memory) the driver can never take. The ballast is that working set, synthesized:
/// with it held, a mid-range target is guaranteed unfillable while the guest stays
/// healthy.
fn spawn_ballast(guest: &Guest, mib: u64) {
    let script = format!(
        r#"import ctypes, time
SZ = {mib} * 1024 * 1024
libc = ctypes.CDLL(None, use_errno=True)
buf = ctypes.create_string_buffer(SZ)
ctypes.memset(buf, 1, SZ)
r = libc.mlockall(1)  # MCL_CURRENT
open('/proc/self/oom_score_adj', 'w').write('-1000')
open('/tmp/limina-ballast-ok', 'w').write('locked %d\n' % r)
while True:
    time.sleep(60)
"#
    );
    guest
        .ssh_exec(&format!(
            "cat > /tmp/limina-ballast.py <<'BENCH_BALLAST_EOF'\n{script}\nBENCH_BALLAST_EOF"
        ))
        .expect("staging the ballast");
    guest
        .ssh_exec(
            "rm -f /tmp/limina-ballast-ok; \
             sudo setsid nohup python3 /tmp/limina-ballast.py </dev/null >/dev/null 2>&1 & \
             echo spawned",
        )
        .expect("spawning the ballast");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let out = guest
            .ssh_exec("cat /tmp/limina-ballast-ok 2>/dev/null")
            .unwrap_or_default();
        if let Some(line) = out.lines().next() {
            assert_eq!(
                line.trim(),
                "locked 0",
                "ballast mlockall failed ({line}) — an unlocked ballast is reclaimable \
                 and phase C's unfillable-target guarantee is gone"
            );
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ballast never reported in — spawn failed or the touch loop wedged the guest"
        );
    }
}

fn kill_ballast(guest: &Guest) {
    let _ = guest.ssh_exec("sudo pkill -f '[l]imina-ballast.py' || true");
}

/// Sample the host at ~200 ms for `secs`, appending to `samples`; returns the last sample.
fn observe(guest: &Guest, samples: &mut Vec<HostSample>, secs: u64) -> HostSample {
    let mut last = HostSample::default();
    for _ in 0..(secs * 5) {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(guest) {
            last = s;
            samples.push(s);
        }
    }
    last
}

#[test]
fn s7_out_of_puff_chase() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s7_out_of_puff_chase: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip("s7_out_of_puff_chase") {
        return;
    }
    if matches!(tier(), limina_test::bench::Tier::Enhanced) {
        eprintln!(
            "SKIPPED s7_out_of_puff_chase on the enhanced tier: the scenario is \
             CONCLUDED UNCONSTRUCTIBLE there — six constructions all ended in guest \
             death because the driver has no self-preservation (see the Sizes doc \
             comment and spikes/balloon-bench-2026-08-10/RESULTS.md, S7enh). The \
             dogfood field spam is the enhanced-tier positive control."
        );
        return;
    }
    let sz = sizes();
    let cfg = match tier_config() {
        Ok(mut cfg) => {
            cfg.ram_mib = sz.ram_mib;
            cfg.with_net().with_balloon_control()
        }
        Err(e) => {
            eprintln!("SKIPPED s7_out_of_puff_chase: {e}");
            return;
        }
    };
    let run =
        BenchRun::create(&format!("s7{}", tier().suffix())).expect("creating the bench run dir");
    eprintln!(
        "S7 out-of-puff chase (no policy, {} tier); artifacts -> {:?}",
        tier().label(),
        run.dir()
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    let stamp = verify_tier(&guest, &cfg).expect("tier positive control");
    if sz.swapoff {
        eprintln!("  swapoff -a (zram would make every unpinned page obtainable)");
        guest.ssh_exec("sudo swapoff -a").expect("swapoff");
    }
    std::thread::sleep(Duration::from_secs(5));
    let sampler = GuestSampler::start(&guest, 250).expect("starting the guest sampler");
    let mut host_samples: Vec<HostSample> = Vec::new();

    // ---- Phase A: the H1 stage-set (MemFree low, MemAvailable high). --------------------
    eprintln!(
        "phase A: filling the page cache ({} MiB file on disk)",
        sz.cache_file_mib
    );
    let df_avail_mib: u64 = guest
        .ssh_exec("df -m --output=avail /var/tmp | tail -1")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    assert!(
        df_avail_mib > sz.cache_file_mib + 1024,
        "guest disk too small for the cache-fill file ({df_avail_mib} MiB avail on /var/tmp)"
    );
    // Write, then read back: the write path caches the pages, the read-back re-warms
    // anything writeback pressure evicted — the point is a page-cache-full guest.
    guest
        .ssh_exec(&format!(
            "dd if=/dev/zero of={CACHE_FILE} bs=1M count={} 2>/dev/null; sync; \
             cat {CACHE_FILE} > /dev/null",
            sz.cache_file_mib
        ))
        .expect("cache-fill dd + read-back");
    std::thread::sleep(Duration::from_secs(3));
    let shape = guest
        .ssh_exec(
            "awk '/MemTotal|MemFree|MemAvailable|^Cached/{printf \"%s%s \", $1, $2}' /proc/meminfo",
        )
        .unwrap_or_default();
    eprintln!("  H1 shape: {}", shape.trim());
    let free_kib: u64 = guest
        .ssh_exec("awk '/MemFree/{print $2}' /proc/meminfo")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let avail_kib: u64 = guest
        .ssh_exec("awk '/MemAvailable/{print $2}' /proc/meminfo")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    // ---- Phase B: slow fill against cache. ----------------------------------------------
    let mark_b = guest_epoch_secs(&guest).expect("guest clock");
    let t_b = now_ms();
    guest
        .set_balloon_target(SLOW_FILL_TARGET)
        .expect("commanding the slow-fill target");
    eprintln!("phase B: 2 GiB target into the cache-full guest (60 s observation)");
    // Record WHEN the target is first reached — the old actual/window arithmetic
    // reported "~33 MiB/s" for a fill that completed in under a second (2048/60
    // exactly, every run). The rate is only honest over the time the fill took.
    let mut b_last = HostSample::default();
    let mut b_fill_ms: Option<u64> = None;
    for _ in 0..(60 * 5) {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(&guest) {
            if b_fill_ms.is_none() && s.actual >= SLOW_FILL_TARGET {
                b_fill_ms = Some(s.ts_ms.saturating_sub(t_b).max(1));
            }
            b_last = s;
            host_samples.push(s);
        }
    }
    let puff_b = puff_since(&guest, &mark_b);
    let b_fill_rate = match b_fill_ms {
        Some(ms) => mib_per_s(SLOW_FILL_TARGET, Duration::from_millis(ms)),
        None => mib_per_s(
            b_last.actual,
            Duration::from_millis(b_last.ts_ms.saturating_sub(t_b).max(1)),
        ),
    };
    eprintln!(
        "  phase B: actual={} MiB ({}, ~{:.0} MiB/s vs S1's ~1840 against free), puff lines={}",
        b_last.actual / MIB,
        match b_fill_ms {
            Some(ms) => format!("target reached in {:.2} s", ms as f64 / 1000.0),
            None => "target NOT reached in 60 s".to_string(),
        },
        b_fill_rate,
        puff_b
    );

    // ---- Phase C: chase the driver past its plateau — the permanent retry loop. ---------
    // Adaptive: whenever the driver closes the gap, raise the target one step (capped),
    // so the loop runs against the guest's NATURAL allocation ceiling for the whole hold.
    if sz.ballast_mib > 0 {
        eprintln!(
            "  pinning {} MiB of mlocked ballast (the synthetic desktop working set)",
            sz.ballast_mib
        );
        spawn_ballast(&guest, sz.ballast_mib);
    }
    let mark_c = guest_epoch_secs(&guest).expect("guest clock");
    let mut chase_target = sz.chase_start;
    guest
        .set_balloon_target(chase_target)
        .expect("commanding the chase target");
    eprintln!(
        "phase C: chasing from {} MiB (step {} MiB, cap {} MiB), 90 s hold",
        sz.chase_start / MIB,
        CHASE_STEP / MIB,
        sz.chase_cap / MIB
    );
    let mut c_last = HostSample::default();
    for _ in 0..(90 * 5) {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(s) = sample_host(&guest) {
            c_last = s;
            host_samples.push(s);
            if chase_target.saturating_sub(s.actual) < 64 * MIB && chase_target < sz.chase_cap {
                chase_target = (chase_target + CHASE_STEP).min(sz.chase_cap);
                guest.set_balloon_target(chase_target).ok();
                eprintln!(
                    "  gap closed — escalating target to {} MiB",
                    chase_target / MIB
                );
            }
        }
    }
    let puff_c = puff_since(&guest, &mark_c);
    let oom_c = count_oom_since(&guest, &mark_c);
    eprintln!(
        "  phase C: plateau actual={} MiB (final target {} MiB), puff lines={} in 90 s, \
         oom kills={}",
        c_last.actual / MIB,
        chase_target / MIB,
        puff_c,
        oom_c
    );

    // ---- Phase D: close the gap, the loop must stop. ------------------------------------
    let plateau = c_last.actual;
    let mark_d = guest_epoch_secs(&guest).expect("guest clock");
    guest
        .set_balloon_target(plateau)
        .expect("re-commanding the plateau actual");
    eprintln!(
        "phase D: target = actual ({} MiB), 30 s silence check",
        plateau / MIB
    );
    observe(&guest, &mut host_samples, 30);
    let puff_d = puff_since(&guest, &mark_d);
    eprintln!("  phase D: puff lines={puff_d} (want ≤1 ratelimit straggler)");

    // Deflate and collect.
    if sz.ballast_mib > 0 {
        kill_ballast(&guest);
    }
    guest.set_balloon_target(0).ok();
    std::thread::sleep(Duration::from_secs(5));
    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write("guest.csv", &guest_csv).unwrap();
    run.write_host_samples(&host_samples).unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write("journal.txt", &journal).unwrap();

    let kswapd_delta = {
        let first = guest_samples
            .first()
            .map(|s| s.kswapd_cpu_ticks)
            .unwrap_or(0);
        let last = guest_samples
            .last()
            .map(|s| s.kswapd_cpu_ticks)
            .unwrap_or(0);
        last.saturating_sub(first)
    };
    let mut entries = stamp.entries();
    entries.extend([
        ("scenario", "\"s7-out-of-puff\"".to_string()),
        ("ram_mib", sz.ram_mib.to_string()),
        ("cache_file_mib", sz.cache_file_mib.to_string()),
        ("ballast_mib", sz.ballast_mib.to_string()),
        ("swapoff", sz.swapoff.to_string()),
        ("h1_free_kib", free_kib.to_string()),
        ("h1_avail_kib", avail_kib.to_string()),
        ("slow_fill_target", SLOW_FILL_TARGET.to_string()),
        ("slow_fill_actual_60s", b_last.actual.to_string()),
        (
            "slow_fill_reach_ms",
            b_fill_ms.map_or("null".to_string(), |ms| ms.to_string()),
        ),
        ("slow_fill_mib_s", format!("{b_fill_rate:.1}")),
        ("puff_lines_b", puff_b.to_string()),
        ("chase_final_target", chase_target.to_string()),
        ("plateau_actual", plateau.to_string()),
        ("puff_lines_c_90s", puff_c.to_string()),
        ("oom_kills_c", oom_c.to_string()),
        ("puff_lines_d_30s", puff_d.to_string()),
        ("kswapd_cpu_ticks_delta", kswapd_delta.to_string()),
        ("guest_samples", guest_samples.len().to_string()),
    ]);
    let metrics = json_object(&entries);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S7 metrics ==\n{metrics}");

    // The journal channel's positive control: an unfillable target MUST produce the spam.
    assert!(
        puff_c > 0,
        "phase C held an unfillable target for 90 s with ZERO 'Out of puff' journal lines — \
         either the journal pipeline is broken (grep/sudo/journalctl) or the driver model \
         in docs/design/balloon-bench.md §2 is wrong; both need investigating before any \
         journal-based conclusion is trusted"
    );
    // The wall must be HEALTHY: the retry loop spamming while the guest OOM-kills is
    // the attempt-3 collapse, not the dogfood state this scenario reproduces.
    assert_eq!(
        oom_c, 0,
        "phase C's plateau came with {oom_c} OOM kill(s) — the wall is not a healthy \
         stall, the construction squeezed the guest"
    );
    // Closing the gap must stop the loop (ratelimit may land one straggler).
    assert!(
        puff_d <= 1,
        "phase D closed the target/actual gap but the retry loop kept logging \
         ({puff_d} lines in 30 s) — the §2 requeue model is wrong"
    );
    // The H1 stage-set must actually have been the H1 shape, or phase B measured nothing.
    assert!(
        free_kib < 1024 * 1024 && avail_kib > 5 * 512 * 1024,
        "phase A did not produce the H1 shape (free={free_kib} KiB, avail={avail_kib} KiB) — \
         phase B's fill-vs-cache number is not measuring fill-vs-cache"
    );

    let outcome = guest.shutdown(Duration::from_secs(15));
    eprintln!("teardown: {outcome:?}");
}
