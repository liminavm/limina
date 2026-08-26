// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Balloon bench **S9 — the ledger-churn reproducer** (paired with the hv-ledger-gap
//! investigation, `spikes/hv-ledger-gap/RESULTS.md`).
//!
//! The hv-ledger-gap spike found real orphaned compressor slots outliving the worker
//! task (~35 G on the dogfood Mac), suspect: `MADV_FREE_REUSABLE` via the FRQ
//! interacting with compressed guest pages. Its host-side toy proved every host-side
//! REUSABLE-on-compressed transition accounts exactly — the missing ingredient is
//! **guest stage-2 faulting at realistic FRQ granularity**, which only a real VM
//! produces. This scenario is that vehicle:
//!
//! - **churn** (the reviewed per-cycle sequence): normal dwell (toucher running) →
//!   **COMPRESS phase**: spawn the spike's incompressible host-side `ballast`
//!   (adaptively sized to host free − 2 GiB, capped by `LIMINA_BENCH_S9_BALLAST_MIB`)
//!   and gate on the worker's `internal_compressed` rising ≥ 1 GiB — a cycle where
//!   ic never rises is marked **VOID** (the differential never reached the system
//!   under test) → critical = full-room inflate, so the REUSABLE storm hits
//!   compressed, guest-created, stage-2-faulted pages → release the ballast (close
//!   its stdin) → normal dwell = instant full release (light has no allowance) while
//!   the persistent in-guest toucher (fixed anon buffer + cache-file re-read)
//!   re-faults the released ranges.
//! - **readout**: `spikes/hv-ledger-gap/ledger-dump -a` on the worker pid at 60 s
//!   cadence (10 s during the compress gate; full dumps kept in `ledger.log`,
//!   `internal_compressed` + `phys_footprint` parsed into `ledger.csv`), a
//!   once-per-cycle `vmmap --summary` at balloon peak (`vmmap.log`; the attributable
//!   side of metric (a)) + one full `vmmap` at end of run (`vmmap-full.txt`), plus
//!   host-wide `vm_stat` "Pages stored in compressor" brackets before boot and
//!   after teardown, plus the spike's `net-sweep.sh` unattributed-oracle line in
//!   both brackets AND once per cycle (`netsweep.log`) — raw stored-after is
//!   polluted by squeezed-but-alive bystanders, so the leak is the UNATTRIBUTED
//!   delta (stored − Σ live tasks' net compressed), ~0.2–0.3 G noise floor.
//! - **success metric** (per the paired session): `internal_compressed` diverging
//!   from what's attributable, and the post-teardown stored count not returning to
//!   the pre-boot baseline. Worker death is explicitly NOT a metric — the scenario
//!   asserts only guest health (no OOM kills) and plumbing (ledger rows parsed).
//!
//! Tier-aware like the rest of the bench (stock = harness relays real reports;
//! enhanced = the real agent). The dogfood machine is 16k, so the enhanced run is
//! the realistic one; stock is the cheap smoke.
//!
//! Gated: `LIMINA_BALLOON_BENCH=1` + HVF. Cycles via `LIMINA_BENCH_S9_CYCLES`
//! (default 5).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use limina_proto::Message;
use limina_test::bench::{
    count_oom_since, fetch_balloon_journal, guest_epoch_secs, join_control_as_agent, json_object,
    now_ms, real_report, sample_host, tier, tier_config, verify_tier, BenchRun, GuestSampler,
    HostSample, Tier,
};
use limina_test::Guest;

const MIB: u64 = 1 << 20;
/// Defaults size a cheap smoke (no ballast → every cycle VOID by construction, which
/// exercises the plumbing only). The REAL run (per the paired session's review) keeps
/// the VM moderate and constructs compression deterministically with the host-side
/// ballast instead of relying on emergent pressure — the first smoke proved a 6 GiB
/// VM on an idle 32 GiB host never wakes the compressor at all (vm_stat stored
/// pinned 0). Knobs: `LIMINA_BENCH_S9_MIN_MIB`/`MAX_MIB` (real run: 3072/12288),
/// `LIMINA_BENCH_S9_BALLAST_MIB` (0 = no ballast; >0 = the CAP on the adaptive size
/// host_free − 2 GiB; real run: 14336 — sized so the critical-phase total stays
/// under ~28 G on the 32 G box, since incompressible ballast permanently grows the
/// host swapfiles until reboot).
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
/// The compress-phase gate: the worker's internal_compressed must rise by this much
/// or the cycle is VOID.
const IC_RISE_GATE: u64 = 1 << 30;
/// Compression happens via the pageout scan, which can lag the exhaustion of the
/// reclaimable pool by minutes — give the gate room.
const IC_GATE_TIMEOUT_SECS: u64 = 360;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Compressor segments currently swapped OUT to disk. The final reproducer axis:
/// every observed orphan pool sits in swapped-out segments on swap-full hosts, and
/// freeing a slot whose segment is on disk is a different code path (deferred
/// compaction, swapin-for-free) than the in-RAM frees every earlier construction
/// exercised. Gate 2 of the compress phase: the REUSABLE storm only counts as
/// hitting the swap regime when this is > 0 at storm time.
fn swapped_out_segments() -> u64 {
    let out = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "vm.compressor.segment.swappedout"])
        .output()
        .expect("running sysctl");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("parsing vm.compressor.segment.swappedout")
}
/// The in-guest toucher's anon working set (`LIMINA_BENCH_S9_TOUCH_MIB` overrides).
/// Default sizes the smoke: small enough that the guest at `min` never needs the OOM
/// killer, big enough that every release→re-touch cycle re-faults a meaningful range.
/// The real run wants it bigger (e.g. 2048): guest-pinned memory shrinks the host's
/// reclaimable pool so the capped ballast can actually exhaust it, and at full
/// inflate the surplus churns through zram — heavy, realistic stage-2 fault traffic.
const TOUCH_MIB: u64 = 768;
const CACHE_FILE: &str = "/var/tmp/limina-s9-cache";
/// `LIMINA_BENCH_S9_CACHE_MIB` overrides (final spill spec: 3072). Cache is the
/// re-dirty medium of choice — it's what the dogfood guest held 17.5 G of, and it's
/// guest-reclaimable, so the critical inflate FRQs it away without OOM risk (a cold
/// anon set under light's full-room inflate is an S7-style guest-death invitation).
const CACHE_MIB: u64 = 1024;
/// Dwells: critical must cover the light ramp to full room (4 GiB at 256 MiB/2 s
/// ≈ 64 s) plus slack; normal covers the instant release plus a re-touch window.
const CRITICAL_DWELL_SECS: u64 = 120;
const NORMAL_DWELL_SECS: u64 = 60;

fn write_level(path: &Path, level: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{level}\n")).expect("writing the host-level temp file");
    std::fs::rename(&tmp, path).expect("renaming the host-level file into place");
}

/// Host-wide compressor occupancy (host 16 KiB pages) — the per-scenario leak
/// oracle's bracket. Panics rather than defaulting: a bracket that silently reads 0
/// would fake a clean baseline.
fn vm_stat_stored_pages() -> u64 {
    let out = std::process::Command::new("/usr/bin/vm_stat")
        .output()
        .expect("running vm_stat");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|l| l.contains("stored in compressor"))
        .and_then(|l| {
            l.split_whitespace()
                .last()
                .and_then(|n| n.trim_end_matches('.').parse().ok())
        })
        .expect("parsing 'Pages stored in compressor' out of vm_stat")
}

/// Locate (building if needed) the hv-ledger-gap spike's ledger-dump tool.
fn ledger_dump_bin() -> PathBuf {
    let dir = limina_test::repo_root().join("spikes/hv-ledger-gap");
    let bin = dir.join("ledger-dump");
    if !bin.exists() {
        let status = std::process::Command::new("cc")
            .args(["-O2", "-o"])
            .arg(&bin)
            .arg(dir.join("ledger-dump.c"))
            .status()
            .expect("running cc for ledger-dump");
        assert!(status.success(), "building ledger-dump failed");
    }
    bin
}

/// One ledger sample of the worker: full dump appended to `log`, the two rows the
/// analysis joins on parsed into (internal_compressed_balance, ic_credit, ic_debit,
/// phys_footprint_balance), all bytes.
fn sample_ledger(bin: &Path, pid: i32, log: &mut std::fs::File) -> Option<(u64, u64, u64, u64)> {
    // -a: include zero-balance rows — internal_compressed is legitimately 0 until the
    // host compressor first touches the worker, and a missing row would read as a
    // broken readout.
    let out = std::process::Command::new(bin)
        .arg(pid.to_string())
        .arg("-a")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    writeln!(log, "=== ts_ms={} pid={pid}", now_ms()).ok()?;
    log.write_all(text.as_bytes()).ok()?;
    // Rows look like: `internal_compressed  physmem  bytes  4.093 G  4.361 G  0.268 G`.
    let parse_row = |name: &str| -> Option<(u64, u64, u64)> {
        let l = text
            .lines()
            .find(|l| l.split_whitespace().next() == Some(name))?;
        let g: Vec<u64> = l
            .split_whitespace()
            .filter(|t| t.parse::<f64>().is_ok())
            .filter_map(|t| t.parse::<f64>().ok())
            .map(|v| (v * (1u64 << 30) as f64) as u64)
            .collect();
        (g.len() >= 3).then(|| (g[0], g[1], g[2]))
    };
    let ic = parse_row("internal_compressed")?;
    let fp = parse_row("phys_footprint")?;
    Some((ic.0, ic.1, ic.2, fp.0))
}

/// Locate (building if needed) the spike's incompressible host-ballast tool.
fn ballast_bin() -> PathBuf {
    let dir = limina_test::repo_root().join("spikes/hv-ledger-gap");
    let bin = dir.join("ballast");
    if !bin.exists() {
        let status = std::process::Command::new("cc")
            .args(["-O2", "-o"])
            .arg(&bin)
            .arg(dir.join("ballast.c"))
            .status()
            .expect("running cc for ballast");
        assert!(status.success(), "building ballast failed");
    }
    bin
}

/// The host's RECLAIMABLE pool right now, in MiB: free + inactive + speculative +
/// purgeable (vm_stat rows, host 16 KiB pages). This — not "Pages free" — is what
/// stands between the host and compression: macOS keeps free perpetually small and
/// absorbs allocations by evicting cache first, so a ballast sized from free alone
/// never wakes the compressor (the first full run proved it: free 4.5 G → 2.4 G
/// ballast → five VOID cycles while ~15 G of cache soaked everything up).
fn host_reclaimable_mib() -> u64 {
    let out = std::process::Command::new("/usr/bin/vm_stat")
        .output()
        .expect("running vm_stat");
    let text = String::from_utf8_lossy(&out.stdout);
    let row = |name: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| {
                l.split_whitespace()
                    .last()
                    .and_then(|n| n.trim_end_matches('.').parse::<u64>().ok())
            })
            .unwrap_or_else(|| panic!("parsing '{name}' out of vm_stat"))
    };
    (row("Pages free") + row("Pages inactive") + row("Pages speculative") + row("Pages purgeable"))
        * 16384
        / (1 << 20)
}

/// The host-side incompressible ballast, spawned with a piped stdin (dropping it is
/// the release signal) and a reader thread that flips `grown` once the target line
/// arrives. An early exit (jetsam) is tolerated by design — "pressure achieved,
/// partially" per the tool's contract.
struct Ballast {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    grown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Ballast {
    fn spawn(bin: &Path, mib: u64) -> Ballast {
        let mut child = std::process::Command::new(bin)
            .arg(mib.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawning ballast");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("ballast stdout");
        let grown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = grown.clone();
        let target_line = format!("grown {mib}");
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if line.trim() == target_line {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            // EOF (exit or jetsam): whatever grew is the pressure we have.
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        Ballast {
            child,
            stdin,
            grown,
        }
    }

    fn grown(&self) -> bool {
        self.grown.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Close stdin (the release signal) and reap, escalating to SIGKILL if the free
    /// loop wedges.
    fn release(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200))
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

/// The paired session's per-scenario leak oracle (`net-sweep.sh`): stored minus the
/// sum of every LIVE task's net compressor charge — raw stored-after is polluted by
/// squeezed-but-alive bystanders (the VOID run's 0.79 G residue decomposed to 0.67 G
/// of exactly that), so the leak is specifically the UNATTRIBUTED remainder, with a
/// ~0.2–0.3 G root-task noise floor. ~20 s per sweep; called in both brackets and
/// once per cycle. Returns the raw summary line for the log; parsing is joint work.
fn net_sweep(label: &str, log: &mut String) -> Option<String> {
    let script = limina_test::repo_root().join("spikes/hv-ledger-gap/net-sweep.sh");
    let out = std::process::Command::new("/bin/bash")
        .arg(&script)
        .arg(label)
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if line.is_empty() {
        return None;
    }
    log.push_str(&format!("{} {line}\n", now_ms()));
    Some(line)
}

/// Once-per-cycle attributable capture: `vmmap --summary` appended to the log with a
/// cycle header. The Writable-regions swapped_out figure in it is the attributable
/// side of the ic-divergence metric; parsing is the analysis session's job.
fn capture_vmmap_summary(pid: i32, cycle: u32, log: &mut std::fs::File) {
    let out = std::process::Command::new("/usr/bin/vmmap")
        .args(["--summary", &pid.to_string()])
        .output();
    if let Ok(out) = out {
        let _ = writeln!(log, "=== cycle {cycle} ts_ms={} pid={pid}", now_ms());
        let _ = log.write_all(&out.stdout);
    }
}

/// Stage and start the persistent in-guest toucher: holds `TOUCH_MIB` of anon and
/// forever re-touches every page of it + re-reads the cache file. Its liveness flag
/// is the mtime of /tmp/limina-s9-touch (bumped each pass).
fn spawn_toucher(guest: &Guest, touch_mib: u64) {
    let script = format!(
        r#"import os, time
SZ = {touch_mib} * 1024 * 1024
buf = bytearray(SZ)
step = os.sysconf('SC_PAGE_SIZE')
while True:
    for off in range(0, SZ, step):
        buf[off] = (buf[off] + 1) & 0xFF
    os.system('cat {CACHE_FILE} > /dev/null 2>&1')
    open('/tmp/limina-s9-touch', 'w').write(str(time.time()))
    time.sleep(1)
"#
    );
    guest
        .ssh_exec(&format!(
            "cat > /tmp/limina-s9-touch.py <<'BENCH_S9_EOF'\n{script}\nBENCH_S9_EOF"
        ))
        .expect("staging the toucher");
    guest
        .ssh_exec(
            "rm -f /tmp/limina-s9-touch; \
             setsid nohup python3 /tmp/limina-s9-touch.py </dev/null >/dev/null 2>&1 & \
             echo spawned",
        )
        .expect("spawning the toucher");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !guest
        .ssh_exec("test -f /tmp/limina-s9-touch && echo yes")
        .map(|o| o.contains("yes"))
        .unwrap_or(false)
    {
        assert!(
            Instant::now() < deadline,
            "the toucher never completed a first pass"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The toucher must still be running — a dead toucher means the churn stopped and
/// the cycle measured nothing.
fn assert_toucher_alive(guest: &Guest, when: &str) {
    let alive = guest
        .ssh_exec("pgrep -f '[l]imina-s9-touch.py' >/dev/null && echo yes")
        .map(|o| o.contains("yes"))
        .unwrap_or(false);
    assert!(alive, "the in-guest toucher died ({when}) — churn stopped");
}

#[test]
fn s9_ledger_churn() {
    if std::env::var("LIMINA_BALLOON_BENCH").ok().as_deref() != Some("1") {
        eprintln!("SKIPPED s9_ledger_churn: set LIMINA_BALLOON_BENCH=1");
        return;
    }
    if !limina_test::require_hvf_or_skip("s9_ledger_churn") {
        return;
    }
    let cycles: u32 = std::env::var("LIMINA_BENCH_S9_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let touch_mib = env_usize("LIMINA_BENCH_S9_TOUCH_MIB", TOUCH_MIB as usize) as u64;
    let ballast_cap_mib = env_usize("LIMINA_BENCH_S9_BALLAST_MIB", 0) as u64;
    // The swap-spill variant deliberately overcommits physical RAM so compressor
    // segments reach disk (the guard's old 16 G ceiling kept everything in-RAM and
    // swap at zero for the whole PASS run). Swapfiles grown by the spill stay grown
    // until reboot — accepted per the paired session, we run post-reboot.
    assert!(
        ballast_cap_mib <= 22528,
        "ballast cap {ballast_cap_mib} MiB exceeds the 22 GiB budget guard"
    );
    // moderate = the dogfood policy shape (held allowance, sub-range REUSABLE
    // dribbles over shifting boundaries); light = full-room dumps (the cheap smoke).
    let reclaim = std::env::var("LIMINA_BENCH_S9_RECLAIM").unwrap_or_else(|_| "light".into());
    let ledger_bin = ledger_dump_bin();
    let ballast_prog = (ballast_cap_mib > 0).then(ballast_bin);
    let run = BenchRun::create(&format!("s9{}", tier().suffix())).expect("creating the run dir");
    eprintln!(
        "S9 ledger churn ({} tier, {cycles} cycles); artifacts -> {:?}",
        tier().label(),
        run.dir()
    );

    // Bracket 1: host compressor occupancy before the VM exists — raw stored plus the
    // unattributed-oracle sweep.
    let stored_before = vm_stat_stored_pages();
    eprintln!(
        "  vm_stat stored before boot: {stored_before} pages (~{:.2} G)",
        (stored_before * 16384) as f64 / (1u64 << 30) as f64
    );
    let mut netsweep_log = String::new();
    let sweep_before = net_sweep("bracket-before", &mut netsweep_log);
    eprintln!(
        "  {}",
        sweep_before.as_deref().unwrap_or("net-sweep: FAILED")
    );

    let trace_path = run.dir().join("trace.jsonl");
    let level_path = run.dir().join("host-level");
    write_level(&level_path, "normal");
    let min_mib = env_usize("LIMINA_BENCH_S9_MIN_MIB", MIN_MIB);
    let max_mib = env_usize("LIMINA_BENCH_S9_MAX_MIB", MAX_MIB);
    let cfg = match tier_config() {
        Ok(cfg) => cfg
            .with_net()
            .with_memory(min_mib, max_mib)
            .with_balloon_control()
            .with_control_socket()
            .with_reclaim(&reclaim)
            .with_env("LIMINA_BALLOON_TRACE", &trace_path.display().to_string())
            .with_env(
                "LIMINA_HOST_PRESSURE",
                &format!("@{}", level_path.display()),
            ),
        Err(e) => {
            eprintln!("SKIPPED s9_ledger_churn: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable");
    let stamp = verify_tier(&guest, &cfg).expect("tier positive control");
    let worker_pid = guest.worker_pid().expect("resolving the worker pid");
    eprintln!("  worker pid {worker_pid}");
    std::thread::sleep(Duration::from_secs(5));

    // Stage-set: a warm cache file + the persistent toucher. The cache size adapts to
    // the guest disk (leave ≥ 1 GiB free); if constrained below 2 GiB, the ic gate is
    // recalibrated for the smaller shape rather than weakened (per the final spec).
    let cache_mib = {
        let want = env_usize("LIMINA_BENCH_S9_CACHE_MIB", CACHE_MIB as usize) as u64;
        let avail: u64 = guest
            .ssh_exec("df -m --output=avail /var/tmp | tail -1")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let fit = avail.saturating_sub(1024).max(512).min(want);
        if fit < want {
            eprintln!("  cache file sized DOWN to {fit} MiB ({avail} MiB avail on /var/tmp)");
        }
        fit
    };
    let ic_rise_gate = if cache_mib < 2048 {
        768 * MIB
    } else {
        IC_RISE_GATE
    };
    guest
        .ssh_exec(&format!(
            "dd if=/dev/urandom of={CACHE_FILE} bs=1M count={cache_mib} 2>/dev/null; sync"
        ))
        .expect("writing the cache file");
    spawn_toucher(&guest, touch_mib);
    let journal_mark = guest_epoch_secs(&guest).expect("guest clock");

    let sampler = GuestSampler::start(&guest, 500).expect("starting the guest sampler");
    let mut conn = match tier() {
        Tier::Stock => {
            Some(join_control_as_agent(&mut guest, "limina-bench-s9/0").expect("joining control"))
        }
        Tier::Enhanced => None,
    };

    let mut ledger_log =
        std::fs::File::create(run.dir().join("ledger.log")).expect("creating ledger.log");
    let mut ledger_csv =
        String::from("ts_ms,ic_balance,ic_credit,ic_debit,phys_footprint,swappedout_segs\n");
    let mut ledger_rows = 0u32;
    let mut last_ledger = Instant::now() - Duration::from_secs(120);
    let mut host_samples: Vec<HostSample> = Vec::new();
    let mut cycle_ends: Vec<String> = Vec::new();

    let dwell = |guest: &mut Guest,
                 conn: &mut Option<limina_test::AgentConn>,
                 host_samples: &mut Vec<HostSample>,
                 ledger_log: &mut std::fs::File,
                 ledger_csv: &mut String,
                 ledger_rows: &mut u32,
                 last_ledger: &mut Instant,
                 secs: u64| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut tick = 0u32;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            if let Ok(s) = sample_host(guest) {
                host_samples.push(s);
            }
            tick += 1;
            if tick.is_multiple_of(5) {
                if let Some(c) = conn.as_mut() {
                    if let Some(r) = real_report(guest) {
                        let _ = c.send(&Message::MemPressure(r));
                    }
                }
            }
            if last_ledger.elapsed() >= Duration::from_secs(60) {
                *last_ledger = Instant::now();
                if let Some((b, cr, d, fp)) = sample_ledger(&ledger_bin, worker_pid, ledger_log) {
                    ledger_csv.push_str(&format!(
                        "{},{b},{cr},{d},{fp},{}\n",
                        now_ms(),
                        swapped_out_segments()
                    ));
                    *ledger_rows += 1;
                }
            }
        }
    };

    let mut vmmap_log =
        std::fs::File::create(run.dir().join("vmmap.log")).expect("creating vmmap.log");
    let mut non_void_cycles = 0u32;
    let mut swap_regime_cycles = 0u32;
    for cycle in 0..cycles {
        eprintln!("== cycle {}/{cycles} ==", cycle + 1);
        // 1. Normal dwell: toucher running, balloon released (light gives the guest
        //    its whole RAM back), guest RE-WARMED with fresh page cache — the cold
        //    compressible mass the ic gate needs. Cache only, no anon: cache is the
        //    dogfood shape AND guest-reclaimable, so the later full-room inflate FRQs
        //    it away instead of OOM-killing over it. (The PASS run showed one
        //    inflate/release strips the guest — without the re-warm only cycle 0
        //    could gate.)
        write_level(&level_path, "normal");
        guest
            .ssh_exec(&format!(
                "dd if=/dev/urandom of={CACHE_FILE} bs=1M count={cache_mib} 2>/dev/null; sync; \
                 cat {CACHE_FILE} > /dev/null"
            ))
            .expect("re-warming the guest cache");
        dwell(
            &mut guest,
            &mut conn,
            &mut host_samples,
            &mut ledger_log,
            &mut ledger_csv,
            &mut ledger_rows,
            &mut last_ledger,
            NORMAL_DWELL_SECS,
        );

        // 2. COMPRESS phase: adaptively-sized incompressible ballast, then TWO gates —
        //    (a) the worker's internal_compressed rising (the differential reached the
        //    system under test) and (b) compressor segments swapped OUT to disk (the
        //    storm hits the swap regime, where every observed orphan pool lives).
        //    Failing (a) ⇒ VOID cycle; (b) is recorded per cycle and asserted on the
        //    whole run when a ballast is armed.
        let mut ballast: Option<Ballast> = None;
        let mut non_void = true;
        let mut swapped_at_gate = swapped_out_segments();
        if let Some(prog) = &ballast_prog {
            let pool = host_reclaimable_mib();
            let size = pool.saturating_sub(2048).clamp(2048, ballast_cap_mib);
            eprintln!("  compress: host reclaimable {pool} MiB -> ballast {size} MiB");
            let b = Ballast::spawn(prog, size);
            let ic_base = sample_ledger(&ledger_bin, worker_pid, &mut ledger_log)
                .map(|(b, ..)| b)
                .unwrap_or(0);
            let deadline = Instant::now() + Duration::from_secs(IC_GATE_TIMEOUT_SECS);
            let mut last_gate = Instant::now();
            let mut ic_rose = false;
            non_void = loop {
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(200));
                if let Ok(s) = sample_host(&guest) {
                    host_samples.push(s);
                }
                if b.grown() && last_gate.elapsed() >= Duration::from_secs(10) {
                    last_gate = Instant::now();
                    if let Some((ic, cr, d, fp)) =
                        sample_ledger(&ledger_bin, worker_pid, &mut ledger_log)
                    {
                        swapped_at_gate = swapped_out_segments();
                        ledger_csv.push_str(&format!(
                            "{},{ic},{cr},{d},{fp},{swapped_at_gate}\n",
                            now_ms()
                        ));
                        ledger_rows += 1;
                        ic_rose = ic.saturating_sub(ic_base) >= ic_rise_gate;
                        // Hold for BOTH conditions while time remains: the swap
                        // regime is the variant's whole point.
                        if ic_rose && swapped_at_gate > 0 {
                            break true;
                        }
                    }
                }
            };
            // ic alone still counts as non-VOID for the compression gate; the swap
            // gate is tracked separately so an in-RAM-only cycle is legible.
            if !non_void && ic_rose {
                non_void = true;
            }
            eprintln!(
                "  compress gates: ic {}, swappedout {}",
                if ic_rose {
                    "rose >= 1 GiB"
                } else {
                    "NO RISE — VOID"
                },
                swapped_at_gate
            );
            ballast = Some(b);
        }

        // 3. Critical: full-room inflate — the REUSABLE storm against compressed pages.
        write_level(&level_path, "critical");
        dwell(
            &mut guest,
            &mut conn,
            &mut host_samples,
            &mut ledger_log,
            &mut ledger_csv,
            &mut ledger_rows,
            &mut last_ledger,
            CRITICAL_DWELL_SECS,
        );
        let peak = host_samples.last().copied().unwrap_or_default();
        capture_vmmap_summary(worker_pid, cycle, &mut vmmap_log);
        // The cycle-resolved oracle point: if the leak reproduces, unattributed grows
        // across cycles (or appears at teardown). ~20 s, sampled at balloon peak.
        if let Some(l) = net_sweep(&format!("cycle-{cycle}"), &mut netsweep_log) {
            eprintln!("  {l}");
        }

        // 4. Release the ballast, then normal dwell: instant full balloon release +
        //    the toucher re-faulting the returned ranges.
        if let Some(b) = ballast.take() {
            b.release();
        }
        write_level(&level_path, "normal");
        dwell(
            &mut guest,
            &mut conn,
            &mut host_samples,
            &mut ledger_log,
            &mut ledger_csv,
            &mut ledger_rows,
            &mut last_ledger,
            NORMAL_DWELL_SECS,
        );
        let trough = host_samples.last().copied().unwrap_or_default();
        assert_toucher_alive(&guest, &format!("after cycle {}", cycle + 1));
        if non_void {
            non_void_cycles += 1;
        }
        if swapped_at_gate > 0 {
            swap_regime_cycles += 1;
        }
        eprintln!(
            "  cycle {}: peak actual={} MiB, trough actual={} MiB, {}, swappedout@storm={}",
            cycle + 1,
            peak.actual / MIB,
            trough.actual / MIB,
            if non_void { "counted" } else { "VOID" },
            swapped_at_gate
        );
        cycle_ends.push(json_object(&[
            ("cycle", cycle.to_string()),
            ("peak_actual", peak.actual.to_string()),
            ("trough_actual", trough.actual.to_string()),
            ("non_void", non_void.to_string()),
            ("swappedout_at_storm", swapped_at_gate.to_string()),
        ]));
    }

    let oom = count_oom_since(&guest, &journal_mark);
    let _ = guest.ssh_exec("pkill -f '[l]imina-s9-touch.py' || true");
    let (guest_csv, guest_samples) = sampler.stop_and_fetch(&guest).expect("fetching guest CSV");
    run.write("guest.csv", &guest_csv).unwrap();
    run.write_host_samples(&host_samples).unwrap();
    run.write("ledger.csv", &ledger_csv).unwrap();
    let journal = fetch_balloon_journal(&guest).unwrap_or_default();
    run.write("journal.txt", &journal).unwrap();

    // Final in-VM ledger sample + the region-level attributable view, then teardown,
    // then bracket 2.
    let final_ledger = sample_ledger(&ledger_bin, worker_pid, &mut ledger_log);
    if let Ok(out) = std::process::Command::new("/usr/bin/vmmap")
        .arg(worker_pid.to_string())
        .output()
    {
        let _ = run.write("vmmap-full.txt", &String::from_utf8_lossy(&out.stdout));
    }
    let outcome = guest.shutdown(Duration::from_secs(15));
    eprintln!("teardown: {outcome:?}");
    // Give the kernel a beat to settle the dead task's ledger before the bracket.
    std::thread::sleep(Duration::from_secs(10));
    let stored_after = vm_stat_stored_pages();
    let residue_pages = stored_after.saturating_sub(stored_before);
    eprintln!(
        "  vm_stat stored after teardown: {stored_after} pages (residue vs pre-boot: \
         {residue_pages} pages ≈ {:.2} G — raw; the oracle is the unattributed delta)",
        (residue_pages * 16384) as f64 / (1u64 << 30) as f64
    );
    let sweep_after = net_sweep("bracket-after", &mut netsweep_log);
    eprintln!(
        "  {}",
        sweep_after.as_deref().unwrap_or("net-sweep: FAILED")
    );
    run.write("netsweep.log", &netsweep_log).unwrap();

    let mut entries = stamp.entries();
    entries.extend([
        ("scenario", "\"s9-ledger-churn\"".to_string()),
        ("min_mib", min_mib.to_string()),
        ("max_mib", max_mib.to_string()),
        ("cycles", cycles.to_string()),
        ("ballast_cap_mib", ballast_cap_mib.to_string()),
        ("reclaim_mode", format!("{reclaim:?}")),
        ("non_void_cycles", non_void_cycles.to_string()),
        ("swap_regime_cycles", swap_regime_cycles.to_string()),
        ("touch_mib", touch_mib.to_string()),
        ("cache_mib", cache_mib.to_string()),
        ("worker_pid", worker_pid.to_string()),
        ("stored_before_pages", stored_before.to_string()),
        ("stored_after_pages", stored_after.to_string()),
        ("residue_pages", residue_pages.to_string()),
        (
            "netsweep_before",
            sweep_before.map_or("null".into(), |l| format!("{l:?}")),
        ),
        (
            "netsweep_after",
            sweep_after.map_or("null".into(), |l| format!("{l:?}")),
        ),
        ("ledger_rows", ledger_rows.to_string()),
        (
            "final_ic_balance",
            final_ledger.map_or("null".into(), |(b, ..)| b.to_string()),
        ),
        (
            "final_phys_footprint",
            final_ledger.map_or("null".into(), |(.., fp)| fp.to_string()),
        ),
        ("oom_kills", oom.to_string()),
        ("guest_samples", guest_samples.len().to_string()),
        ("cycle_ends", format!("[{}]", cycle_ends.join(","))),
    ]);
    let metrics = json_object(&entries);
    run.write("metrics.json", &metrics).unwrap();
    eprintln!("== S9 metrics ==\n{metrics}");

    // Plumbing assertions only — the leak verdict is the paired session's analysis.
    assert!(
        ledger_rows >= cycles,
        "ledger sampling produced only {ledger_rows} rows over {cycles} cycles — the \
         readout channel is broken, the run measured nothing"
    );
    assert_eq!(
        oom, 0,
        "the guest OOM-killed under churn — resize the toucher before trusting the run"
    );
    // With the ballast enabled, a run where NO cycle passed the ic-rise gate never
    // exercised the compressed-slot interaction at all.
    assert!(
        ballast_cap_mib == 0 || non_void_cycles > 0,
        "every cycle was VOID (internal_compressed never rose ≥ 1 GiB under the \
         ballast) — the compress construction failed, the run is not evidence"
    );
    // The swap-spill variant's own gate: with a ballast big enough to overcommit
    // (cap > 16 G), at least one storm must have fired with segments on disk, or the
    // one axis this variant exists to construct was never reached.
    assert!(
        ballast_cap_mib <= 16384 || swap_regime_cycles > 0,
        "no cycle reached the swap regime (vm.compressor.segment.swappedout stayed 0 \
         at every storm) — the spill construction failed, raise the ballast cap or \
         pre-fill the compressor"
    );
}
