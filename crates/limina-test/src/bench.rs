// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Recorder + summarizer for the balloon characterization bench
//! (`docs/design/balloon-bench.md`). The bench scenarios (`tests/balloon_bench_*.rs`) are
//! measurement runs, not pass/fail guards: they emit a merged time-series trace plus a
//! metrics summary under a run directory, and assert only sanity (the run actually
//! exercised what it claims to measure).
//!
//! Two samplers share one timeline — the guest clock is host `CLOCK_REALTIME` (PL031 +
//! TimeSync), so guest CSV rows and host samples merge on wall-clock milliseconds:
//! - [`GuestSampler`]: a self-contained python script staged over ssh and run *detached in
//!   the guest* (sub-second resolution; per-sample ssh round-trips would alias deflate
//!   latency). Records meminfo (MemFree *and* MemAvailable — the free-vs-available split is
//!   the `Out of puff` H1 discriminator), PSI avgs AND cumulative `total=` stall counters
//!   for memory+io, the vmstat casualty counters, and the reclaim-work channel: kswapd0
//!   CPU ticks plus `pgscan/pgsteal_{kswapd,direct}` — kswapd deltas are the kernel
//!   working for the balloon in the background, direct-reclaim deltas are allocating
//!   processes paying for it in their own latency.
//! - Host-side sampling is a plain function ([`sample_host`]) the scenario calls from its
//!   own poll loop — balloon `target`/`actual`/`reclaimed` plus the worker's
//!   `phys_footprint` (inflation that doesn't shrink the worker is pain for nothing).
//!
//! [`BenchRun`] owns the artifact directory (`LIMINA_BENCH_DIR` or
//! `target/balloon-bench/<name>-<unix-secs>`): raw guest CSV, host CSV, the guest kernel
//! journal (`Out of puff` lines land on the same timeline; note the driver ratelimits, so
//! line counts are a floor), and a hand-assembled `metrics.json`.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::Guest;

/// Wall-clock now in milliseconds since the epoch (the shared trace timeline).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const SAMPLER_PY: &str = "/tmp/limina-bench-sampler.py";
const SAMPLER_CSV: &str = "/tmp/limina-bench-sampler.csv";

/// One row of the in-guest sampler CSV. All memory quantities in KiB, PSI `avg10` in
/// hundredths of a percent (the `MemPressure` scale), PSI `total` in microseconds of stall.
#[derive(Debug, Clone, Copy, Default)]
pub struct GuestSample {
    pub ts_ms: u64,
    pub mem_total_kib: u64,
    pub mem_free_kib: u64,
    pub mem_available_kib: u64,
    pub cached_kib: u64,
    pub swap_free_kib: u64,
    pub mem_some_avg10: u64,
    pub mem_some_total_us: u64,
    pub mem_full_avg10: u64,
    pub mem_full_total_us: u64,
    pub io_some_avg10: u64,
    pub io_some_total_us: u64,
    pub io_full_avg10: u64,
    pub io_full_total_us: u64,
    pub pgmajfault: u64,
    pub pswpin: u64,
    pub pswpout: u64,
    /// Pages scanned/stolen by kswapd (background reclaim — the kernel working for the
    /// balloon behind the guest's back).
    pub pgscan_kswapd: u64,
    pub pgsteal_kswapd: u64,
    /// Pages scanned/stolen in *direct* reclaim — allocating processes stalled doing the
    /// reclaim themselves, the user-visible-latency shape of the same work.
    pub pgscan_direct: u64,
    pub pgsteal_direct: u64,
    /// kswapd0 CPU (utime+stime, clock ticks): how busy the reclaim daemon itself is.
    pub kswapd_cpu_ticks: u64,
}

/// The CSV header the sampler writes and [`parse_guest_csv`] expects — one place, so the
/// python and the parser cannot drift apart.
const GUEST_CSV_HEADER: &str = "ts_ms,mem_total_kib,mem_free_kib,mem_available_kib,cached_kib,\
swap_free_kib,mem_some_avg10,mem_some_total_us,mem_full_avg10,mem_full_total_us,io_some_avg10,\
io_some_total_us,io_full_avg10,io_full_total_us,pgmajfault,pswpin,pswpout,pgscan_kswapd,\
pgsteal_kswapd,pgscan_direct,pgsteal_direct,kswapd_cpu_ticks";

/// Column count of [`GUEST_CSV_HEADER`] (a row with any other arity is torn or stale).
const GUEST_CSV_COLUMNS: usize = 22;

/// The in-guest 250 ms (configurable) sampler, run detached so it survives between ssh
/// commands and samples while the guest is under pressure.
pub struct GuestSampler {
    period_ms: u64,
}

/// The staged sampler script (unit-tested for python syntax on the host — a `format!`
/// brace slip or indentation break must fail `cargo test`, not a live boot).
// NOTE: real newlines only — a `\`-continued Rust string strips next-line leading
// whitespace and flattens python indentation (the balloon_burst.rs lesson).
fn sampler_script(period_ms: u64) -> String {
    format!(
        r#"import glob
import time

kswapd_stat = None
for p in glob.glob('/proc/[0-9]*/comm'):
    try:
        if open(p).read().strip() == 'kswapd0':
            kswapd_stat = p.replace('/comm', '/stat')
            break
    except OSError:
        pass
import sys
print('kswapd_stat=' + str(kswapd_stat), file=sys.stderr)

def kswapd_ticks():
    if not kswapd_stat:
        return 0
    try:
        s = open(kswapd_stat).read()
        f = s[s.rindex(')') + 2:].split()
        return int(f[11]) + int(f[12])
    except (OSError, ValueError, IndexError):
        return 0

def meminfo():
    d = {{}}
    with open('/proc/meminfo') as f:
        for line in f:
            parts = line.split()
            d[parts[0].rstrip(':')] = int(parts[1])
    return d

def psi(path):
    vals = {{'some': (0, 0), 'full': (0, 0)}}
    with open(path) as f:
        for line in f:
            parts = line.split()
            kv = dict(p.split('=') for p in parts[1:])
            vals[parts[0]] = (int(float(kv['avg10']) * 100), int(kv['total']))
    return vals

def vmstat():
    d = {{}}
    with open('/proc/vmstat') as f:
        for line in f:
            k, v = line.split()
            d[k] = int(v)
    return d

out = open('{SAMPLER_CSV}', 'w', buffering=1)
out.write('{GUEST_CSV_HEADER}\n')
while True:
    ts = int(time.time() * 1000)
    m = meminfo()
    pm = psi('/proc/pressure/memory')
    pi = psi('/proc/pressure/io')
    v = vmstat()
    row = [ts, m.get('MemTotal', 0), m.get('MemFree', 0), m.get('MemAvailable', 0),
           m.get('Cached', 0), m.get('SwapFree', 0),
           pm['some'][0], pm['some'][1], pm['full'][0], pm['full'][1],
           pi['some'][0], pi['some'][1], pi['full'][0], pi['full'][1],
           v.get('pgmajfault', 0), v.get('pswpin', 0), v.get('pswpout', 0),
           v.get('pgscan_kswapd', 0), v.get('pgsteal_kswapd', 0),
           v.get('pgscan_direct', 0), v.get('pgsteal_direct', 0),
           kswapd_ticks()]
    out.write(','.join(str(x) for x in row) + '\n')
    time.sleep({period_s})
"#,
        period_s = period_ms as f64 / 1000.0,
    )
}

impl GuestSampler {
    /// Stage and start the sampler. Requires python3 in the guest (the F44 images have it).
    pub fn start(guest: &Guest, period_ms: u64) -> Result<GuestSampler> {
        let script = sampler_script(period_ms);
        guest
            .ssh_exec(&format!(
                "cat > {SAMPLER_PY} <<'BENCH_SAMPLER_EOF'\n{script}\nBENCH_SAMPLER_EOF"
            ))
            .context("staging the guest sampler")?;
        guest
            .ssh_exec(&format!(
                "rm -f {SAMPLER_CSV}; \
                 setsid nohup python3 {SAMPLER_PY} </dev/null >/tmp/limina-bench-sampler.log 2>&1 & \
                 echo started"
            ))
            .context("starting the guest sampler")?;
        // Prove it is alive before the scenario relies on it: the CSV header must appear.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let out = guest
                .ssh_exec(&format!("head -1 {SAMPLER_CSV} 2>/dev/null"))
                .unwrap_or_default();
            if out.trim() == GUEST_CSV_HEADER {
                // Discovery check: an all-zero kswapd_cpu_ticks column can mean a quiet
                // kernel OR a failed PID lookup — make the two distinguishable. Warn only:
                // a kernel without kswapd0 must not fail the whole bench.
                let log = guest
                    .ssh_exec("grep kswapd_stat= /tmp/limina-bench-sampler.log 2>/dev/null")
                    .unwrap_or_default();
                if log.contains("kswapd_stat=None") || !log.contains("kswapd_stat=/proc/") {
                    eprintln!(
                        "WARNING: guest sampler did not find kswapd0 ({}) — the \
                         kswapd_cpu_ticks column will be all zeros by construction",
                        log.trim()
                    );
                }
                return Ok(GuestSampler { period_ms });
            }
            if std::time::Instant::now() > deadline {
                let log = guest
                    .ssh_exec("cat /tmp/limina-bench-sampler.log 2>/dev/null")
                    .unwrap_or_default();
                anyhow::bail!("guest sampler never produced its header; log:\n{log}");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Nominal sampling period (for gap detection in analysis).
    pub fn period_ms(&self) -> u64 {
        self.period_ms
    }

    /// Stop the sampler and fetch every sample. The bracketed pgrep pattern avoids the
    /// ssh-shell self-match trap.
    ///
    /// Errors if the sampler died early (last row much older than now): a scenario whose
    /// sampler quietly died mid-run once produced hollow all-green metrics — the S3 tmpfs
    /// incident, where a workload filled /tmp (a RAM-backed tmpfs) and the sampler's own
    /// CSV writes ENOSPC'd it to death 9 s in.
    pub fn stop_and_fetch(&self, guest: &Guest) -> Result<(String, Vec<GuestSample>)> {
        guest
            .ssh_exec("pkill -f '[l]imina-bench-sampler.py' || true")
            .context("stopping the guest sampler")?;
        let csv = guest
            .ssh_exec(&format!("cat {SAMPLER_CSV}"))
            .context("fetching the guest sampler CSV")?;
        let samples = parse_guest_csv(&csv)?;
        let last_ts = samples.last().map(|s| s.ts_ms).unwrap_or(0);
        let age_ms = now_ms().saturating_sub(last_ts);
        anyhow::ensure!(
            age_ms < 10_000,
            "guest sampler died mid-run: last sample is {age_ms} ms old ({} rows) — \
             the scenario's time series is truncated, its metrics would be hollow",
            samples.len()
        );
        Ok((csv, samples))
    }
}

/// Parse the sampler CSV (header + numeric rows; a torn final row is dropped).
pub fn parse_guest_csv(csv: &str) -> Result<Vec<GuestSample>> {
    let mut lines = csv.lines();
    let header = lines.next().unwrap_or_default().trim();
    anyhow::ensure!(
        header == GUEST_CSV_HEADER,
        "guest CSV header mismatch: {header:?}"
    );
    let mut out = Vec::new();
    for line in lines {
        let fields: Vec<u64> = line
            .trim()
            .split(',')
            .filter_map(|f| f.parse().ok())
            .collect();
        if fields.len() != GUEST_CSV_COLUMNS {
            continue; // torn tail row mid-write, or noise
        }
        out.push(GuestSample {
            ts_ms: fields[0],
            mem_total_kib: fields[1],
            mem_free_kib: fields[2],
            mem_available_kib: fields[3],
            cached_kib: fields[4],
            swap_free_kib: fields[5],
            mem_some_avg10: fields[6],
            mem_some_total_us: fields[7],
            mem_full_avg10: fields[8],
            mem_full_total_us: fields[9],
            io_some_avg10: fields[10],
            io_some_total_us: fields[11],
            io_full_avg10: fields[12],
            io_full_total_us: fields[13],
            pgmajfault: fields[14],
            pswpin: fields[15],
            pswpout: fields[16],
            pgscan_kswapd: fields[17],
            pgsteal_kswapd: fields[18],
            pgscan_direct: fields[19],
            pgsteal_direct: fields[20],
            kswapd_cpu_ticks: fields[21],
        });
    }
    Ok(out)
}

/// One host-side sample: balloon socket stats + the worker's physical footprint.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostSample {
    pub ts_ms: u64,
    /// Last commanded balloon target (bytes).
    pub target: u64,
    /// Guest-reported balloon size (bytes).
    pub actual: u64,
    /// Cumulative host bytes reclaimed via madvise.
    pub reclaimed: u64,
    /// Worker `phys_footprint` (bytes); 0 if unreadable this tick.
    pub worker_footprint: u64,
}

/// Take one host sample now (the scenario's poll loop decides the cadence).
pub fn sample_host(guest: &Guest) -> Result<HostSample> {
    let stats = guest.balloon_stats()?;
    Ok(HostSample {
        ts_ms: now_ms(),
        target: stats.target,
        actual: stats.actual,
        reclaimed: stats.reclaimed,
        worker_footprint: guest.worker_phys_footprint().unwrap_or(0),
    })
}

/// Fetch the guest kernel's balloon chatter with unix timestamps (`-o short-unix`), for the
/// shared timeline. Ratelimited at the source: counts are a floor, episodes are the unit.
pub fn fetch_balloon_journal(guest: &Guest) -> Result<String> {
    guest
        .ssh_exec(
            "sudo journalctl -k -o short-unix --no-pager 2>/dev/null \
             | grep -i 'virtio_balloon\\|balloon' || true",
        )
        .context("fetching the guest kernel balloon journal")
}

/// The artifact directory for one bench run. Everything a run measured lands here; the
/// RESULTS.md that interprets a sweep is written by a human (or Claude) afterwards.
pub struct BenchRun {
    dir: PathBuf,
}

impl BenchRun {
    /// Create the run directory: `$LIMINA_BENCH_DIR` if set, else
    /// `target/balloon-bench/<name>-<unix-secs>` under the workspace.
    pub fn create(name: &str) -> Result<BenchRun> {
        let dir = match std::env::var("LIMINA_BENCH_DIR") {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => {
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                crate::repo_root().join(format!("target/balloon-bench/{name}-{secs}"))
            }
        };
        fs::create_dir_all(&dir).with_context(|| format!("creating bench run dir {dir:?}"))?;
        Ok(BenchRun { dir })
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// Write one artifact file (raw text) into the run directory.
    pub fn write(&self, file: &str, contents: &str) -> Result<()> {
        let path = self.dir.join(file);
        fs::write(&path, contents).with_context(|| format!("writing bench artifact {path:?}"))
    }

    /// Serialize host samples as CSV alongside the guest CSV.
    pub fn write_host_samples(&self, samples: &[HostSample]) -> Result<()> {
        self.write_host_samples_named("host.csv", samples)
    }

    /// Like [`BenchRun::write_host_samples`] under an explicit name (multi-point sweeps).
    pub fn write_host_samples_named(&self, file: &str, samples: &[HostSample]) -> Result<()> {
        let mut csv = String::from("ts_ms,target,actual,reclaimed,worker_footprint\n");
        for s in samples {
            let _ = writeln!(
                csv,
                "{},{},{},{},{}",
                s.ts_ms, s.target, s.actual, s.reclaimed, s.worker_footprint
            );
        }
        self.write(file, &csv)
    }
}

/// Format `(key, value-json)` pairs as one flat JSON object (the metrics contract is simple
/// enough that hand assembly beats a serde dependency; values arrive pre-encoded).
pub fn json_object(pairs: &[(&str, String)]) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "\"{k}\":{v}");
    }
    s.push('}');
    s
}

/// MiB/s over a byte delta and a wall-clock interval (0 if the interval is empty).
pub fn mib_per_s(bytes: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    (bytes as f64 / (1024.0 * 1024.0)) / secs
}

// ---------------------------------------------------------------------------
// Decision-trace parsing (the LIMINA_BALLOON_TRACE JSONL the supervisor writes)
// ---------------------------------------------------------------------------

/// One parsed `LIMINA_BALLOON_TRACE` line — the fields the summarizer consumes. The writer
/// (`balloon_policy::trace_decision`) emits flat JSON with stable keys; this parser is
/// deliberately a key-scanner, not a JSON library: same no-serde trade as the writer.
#[derive(Debug, Clone, Default)]
pub struct TraceEvent {
    pub ts_ms: u64,
    pub some_avg10: u64,
    pub avail_kib: u64,
    /// Blended host level acted on ("normal"/"warn"/"critical").
    pub host: String,
    pub current_pages: u64,
    /// "set" or the hold gate ("converged"/"not-idle"/"dead-band"/"not-calm"/"cooldown"/"dwell").
    pub decision: String,
    pub new_target_pages: Option<u64>,
    pub cooldown_active: bool,
    pub sent: bool,
}

fn json_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let tag = format!("\"{key}\":");
    let start = line.find(&tag)? + tag.len();
    let rest = &line[start..];
    let end = rest
        .char_indices()
        .find(|(i, c)| {
            if rest.starts_with('"') {
                *c == '"' && *i > 0
            } else {
                *c == ',' || *c == '}'
            }
        })
        .map(|(i, _)| if rest.starts_with('"') { i + 1 } else { i })?;
    Some(&rest[..end])
}

fn json_u64(line: &str, key: &str) -> u64 {
    json_field(line, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn json_str(line: &str, key: &str) -> String {
    json_field(line, key)
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_default()
}

/// Parse a full trace file; unparseable lines are skipped (the writer appends atomically
/// per line, but a run can die mid-write).
pub fn parse_trace(jsonl: &str) -> Vec<TraceEvent> {
    jsonl
        .lines()
        .filter(|l| l.contains("\"decision\":"))
        .map(|l| TraceEvent {
            ts_ms: json_u64(l, "ts_ms"),
            some_avg10: json_u64(l, "some_avg10"),
            avail_kib: json_u64(l, "avail_kib"),
            host: json_str(l, "host"),
            current_pages: json_u64(l, "current_pages"),
            decision: json_str(l, "decision"),
            new_target_pages: json_field(l, "new_target_pages")
                .filter(|v| *v != "null")
                .and_then(|v| v.parse().ok()),
            cooldown_active: json_field(l, "cooldown_active") == Some("true"),
            sent: json_field(l, "sent") == Some("true"),
        })
        .collect()
}

/// The first *sent* target decrease at/after `t_ms` — the policy's detection instant for a
/// release. Returns `(ts_ms, from_pages, to_pages)`.
pub fn first_target_decrease_after(trace: &[TraceEvent], t_ms: u64) -> Option<(u64, u64, u64)> {
    trace.iter().find_map(|e| {
        let new = e.new_target_pages?;
        (e.ts_ms >= t_ms && e.sent && new < e.current_pages).then_some((
            e.ts_ms,
            e.current_pages,
            new,
        ))
    })
}

/// Count direction reversals in the *sent* target sequence (the oscillation metric).
pub fn target_reversals(trace: &[TraceEvent]) -> usize {
    let targets: Vec<u64> = trace
        .iter()
        .filter(|e| e.sent)
        .filter_map(|e| e.new_target_pages)
        .collect();
    targets
        .windows(3)
        .filter(|w| (w[1] > w[0]) != (w[2] > w[1]))
        .count()
}

// ---------------------------------------------------------------------------
// Guest-side workload: the throttled allocation burst (S2/S5)
// ---------------------------------------------------------------------------

const BURST_PY: &str = "/tmp/limina-burst.py";
const BURST_OUT: &str = "/tmp/limina-burst.out";

/// What the burst allocator has done so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurstStatus {
    /// Still allocating: bytes touched so far.
    Running(u64),
    /// Touched everything; holding the allocation (ts_ms of completion).
    Complete(u64),
    /// The process is gone without BURST-OK — killed (the OOM killer's signature).
    Died,
}

/// The staged burst script (unit-tested for syntax like the sampler). Touches `total` bytes
/// of anonymous memory at `rate` bytes/s (0 = unthrottled), logging per-chunk timestamps,
/// then HOLDS the allocation (relief measurements need the pressure to persist until the
/// harness kills it).
fn burst_script(total: u64, rate: u64) -> String {
    format!(
        r#"import ctypes
import time

CH = 64 * 1024 * 1024
total = {total}
rate = {rate}
chunks = []
out = open('{BURST_OUT}', 'w', buffering=1)
start = time.time()
alloc = 0
while alloc < total:
    b = ctypes.create_string_buffer(CH)
    ctypes.memset(b, 1, CH)
    chunks.append(b)
    alloc += CH
    out.write('chunk %d %d\n' % (alloc // CH, int(time.time() * 1000)))
    if rate > 0:
        d = (start + alloc / rate) - time.time()
        if d > 0:
            time.sleep(d)
out.write('BURST-OK %d\n' % int(time.time() * 1000))
while True:
    time.sleep(60)
"#
    )
}

/// Stage and launch the burst detached; returns the host-clock ms just before the spawn
/// command went out (`t0` for detection-latency metrics; the guest logs its own per-chunk
/// timestamps on the same clock).
pub fn start_burst(guest: &Guest, total: u64, rate: u64) -> Result<u64> {
    let script = burst_script(total, rate);
    guest
        .ssh_exec(&format!(
            "cat > {BURST_PY} <<'BENCH_BURST_EOF'\n{script}\nBENCH_BURST_EOF"
        ))
        .context("staging the burst allocator")?;
    let t0 = now_ms();
    guest
        .ssh_exec(&format!(
            "rm -f {BURST_OUT}; \
             setsid nohup python3 {BURST_PY} </dev/null >/dev/null 2>&1 & \
             echo spawned"
        ))
        .context("spawning the burst allocator")?;
    Ok(t0)
}

/// Poll the burst's progress. The bracketed pgrep avoids the ssh-shell self-match trap.
pub fn burst_status(guest: &Guest) -> Result<BurstStatus> {
    let out = guest
        .ssh_exec(&format!("cat {BURST_OUT} 2>/dev/null"))
        .unwrap_or_default();
    if let Some(ok) = out.lines().find(|l| l.starts_with("BURST-OK")) {
        let ts = ok
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        return Ok(BurstStatus::Complete(ts));
    }
    let alive = guest
        .ssh_exec("pgrep -f '[l]imina-burst.py' | head -1")
        .unwrap_or_default();
    if alive.trim().is_empty() {
        return Ok(BurstStatus::Died);
    }
    let chunks = out.lines().filter(|l| l.starts_with("chunk")).count() as u64;
    Ok(BurstStatus::Running(chunks * 64 * 1024 * 1024))
}

/// Kill the burst (releases its held allocation).
pub fn kill_burst(guest: &Guest) -> Result<()> {
    guest
        .ssh_exec("pkill -f '[l]imina-burst.py' || true")
        .map(|_| ())
        .context("killing the burst allocator")
}

// ---------------------------------------------------------------------------
// Control-plane relay (the harness playing limina-agent on the stock baseline)
// ---------------------------------------------------------------------------

/// Join the control plane as the agent (Hello/Welcome) with the `mempressure` capability.
pub fn join_control_as_agent(guest: &mut Guest, name: &str) -> Result<crate::AgentConn> {
    use limina_proto::{Hello, Message};
    let mut conn = guest
        .connect_control(Duration::from_secs(30))
        .context("connecting to the control plane")?;
    conn.send(&Message::Hello(Hello {
        agent: name.to_string(),
        caps: vec!["mempressure".to_string()],
        pagesize: 4096,
    }))
    .context("sending Hello")?;
    match conn.recv(Duration::from_secs(10)) {
        Ok((_, Message::Welcome(_))) => Ok(conn),
        other => anyhow::bail!("expected Welcome from the control plane, got {other:?}"),
    }
}

/// Read the guest's REAL pressure + meminfo over ssh, as limina-agent would report them
/// (`avg10=1.23` → `123`, the MemPressure hundredths scale).
pub fn real_report(guest: &Guest) -> Option<limina_proto::MemPressure> {
    let out = guest
        .ssh_exec(
            "cat /proc/pressure/memory; awk '/MemTotal|MemAvailable|MemFree/{print $1, $2}' /proc/meminfo",
        )
        .ok()?;
    let pct100 = |line_tag: &str, field: &str| -> u32 {
        out.lines()
            .find(|l| l.starts_with(line_tag))
            .and_then(|l| l.split(&format!("{field}=")).nth(1))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|v| v.parse::<f64>().ok())
            .map(|f| (f * 100.0) as u32)
            .unwrap_or(0)
    };
    let mem_kib = |tag: &str| -> u64 {
        out.lines()
            .find(|l| l.starts_with(tag))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    Some(limina_proto::MemPressure {
        some_avg10: pct100("some", "avg10"),
        some_avg60: pct100("some", "avg60"),
        full_avg10: pct100("full", "avg10"),
        full_avg60: pct100("full", "avg60"),
        mem_available_kib: mem_kib("MemAvailable:"),
        mem_total_kib: mem_kib("MemTotal:"),
        mem_free_kib: mem_kib("MemFree:"),
        ..Default::default()
    })
}

/// A synthetic idle, memory-rich report for `total_mib`: drives the policy to inflate
/// (the balloon_psi/balloon_burst drive).
pub fn idle_report(total_mib: u64) -> limina_proto::MemPressure {
    let total = total_mib * 1024;
    limina_proto::MemPressure {
        mem_available_kib: total * 70 / 100,
        mem_total_kib: total,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Summarizer helpers over the sample series
// ---------------------------------------------------------------------------

/// Time-integral of a PSI avg10 channel over `[from_ms, to_ms]`, in percent·seconds
/// (step-sum: each sample's value held until the next).
pub fn psi_integral_pct_s(
    samples: &[GuestSample],
    field: impl Fn(&GuestSample) -> u64,
    from_ms: u64,
    to_ms: u64,
) -> f64 {
    let window: Vec<&GuestSample> = samples
        .iter()
        .filter(|s| s.ts_ms >= from_ms && s.ts_ms <= to_ms)
        .collect();
    window
        .windows(2)
        .map(|w| {
            let dt_s = (w[1].ts_ms - w[0].ts_ms) as f64 / 1000.0;
            (field(w[0]) as f64 / 100.0) * dt_s
        })
        .sum()
}

/// Delta of a cumulative counter between the last sample ≤ `from_ms` (or the first) and the
/// last sample ≤ `to_ms`.
pub fn counter_delta(
    samples: &[GuestSample],
    field: impl Fn(&GuestSample) -> u64,
    from_ms: u64,
    to_ms: u64,
) -> u64 {
    let at = |t: u64| {
        samples
            .iter()
            .filter(|s| s.ts_ms <= t)
            .next_back()
            .or(samples.first())
            .map(&field)
            .unwrap_or(0)
    };
    at(to_ms).saturating_sub(at(from_ms))
}

/// Count OOM kills in the guest kernel log since the given guest epoch-seconds watermark.
pub fn count_oom_since(guest: &Guest, since_epoch_secs: &str) -> u64 {
    guest
        .ssh_exec(&format!(
            "sudo journalctl -k --since=@{since_epoch_secs} 2>/dev/null \
             | grep -ci 'out of memory\\|oom-kill' || true"
        ))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The guest's wall clock in epoch seconds (journal watermark).
pub fn guest_epoch_secs(guest: &Guest) -> Result<String> {
    Ok(guest.ssh_exec("date +%s")?.trim().to_string())
}

// ---- The tier axis (Phase 2) ----

/// The guest tier a bench scenario runs on (`LIMINA_BENCH_TIER`: `stock` | `enhanced`,
/// default stock). Unknown values panic — a typo silently benching the wrong tier is
/// exactly the failure mode the tier stamp exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Stock-4k baseline image; the harness plays the agent.
    Stock,
    /// F44 enhanced test golden, EFI-booted on its own 16k kernel; the REAL limina-agent
    /// reports (the harness must NOT relay).
    Enhanced,
}

impl Tier {
    /// Run-directory suffix (`s2` vs `s2enh`) so tier archives never collide.
    pub fn suffix(self) -> &'static str {
        match self {
            Tier::Stock => "",
            Tier::Enhanced => "enh",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tier::Stock => "stock",
            Tier::Enhanced => "enhanced",
        }
    }
}

pub fn tier() -> Tier {
    match std::env::var("LIMINA_BENCH_TIER") {
        Err(_) => Tier::Stock,
        Ok(v) if v == "stock" => Tier::Stock,
        Ok(v) if v == "enhanced" => Tier::Enhanced,
        Ok(v) => panic!("LIMINA_BENCH_TIER={v:?} is not a tier (stock|enhanced)"),
    }
}

/// Boot config for the current tier. Stock = the baseline EFI Fedora. Enhanced = the
/// seated enhanced golden EFI-booted through the GOP firmware (guest's own 16k kernel,
/// real agent) — with the disk **pinned to the F44 family**: `LIMINA_FEDORA_REL`
/// defaults to 43 and the F43 enhanced golden also exists on this machine, so trusting
/// the env would silently bench a 6.12-kernel guest with every assert green.
/// `LIMINA_TEST_DISK_ENH` still overrides for a deliberate different disk.
pub fn tier_config() -> Result<crate::GuestConfig> {
    match tier() {
        Tier::Stock => crate::GuestConfig::baseline_fedora_from_env(),
        Tier::Enhanced => {
            let mut cfg = crate::GuestConfig::seated_efi_fedora_from_env()?;
            if std::env::var("LIMINA_TEST_DISK_ENH").is_err() {
                let disk = crate::repo_root().join("Fedora-Workstation-44.enhanced.test.raw");
                anyhow::ensure!(
                    disk.exists(),
                    "F44 enhanced test golden not found at {disk:?} (set LIMINA_TEST_DISK_ENH)"
                );
                match &mut cfg.boot {
                    crate::Boot::Firmware { disk: d, .. } => *d = disk,
                    other => anyhow::bail!("seated EFI config built unexpected boot {other:?}"),
                }
            }
            Ok(cfg)
        }
    }
}

/// What actually booted — folded into every scenario's `metrics.json`.
#[derive(Debug, Clone)]
pub struct TierStamp {
    pub tier: Tier,
    pub pagesize: u64,
    pub uname_r: String,
    pub disk: String,
}

impl TierStamp {
    /// Entries for [`json_object`].
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        vec![
            ("tier", format!("\"{}\"", self.tier.label())),
            ("guest_pagesize", self.pagesize.to_string()),
            ("guest_kernel", format!("\"{}\"", self.uname_r)),
            ("disk", format!("\"{}\"", self.disk)),
        ]
    }
}

/// The tier positive control — "verify the fix is actually loaded", applied to the tier
/// axis: assert in-guest that the pagesize (and for enhanced, the F44-family 7.x kernel)
/// matches what `LIMINA_BENCH_TIER` asked for, and stamp what booted. Every enhanced
/// scenario calls this right after ssh comes up; a wrong disk fails loudly instead of
/// producing a green run of the wrong guest.
pub fn verify_tier(guest: &Guest, cfg: &crate::GuestConfig) -> Result<TierStamp> {
    let t = tier();
    let pagesize: u64 = guest
        .ssh_exec("getconf PAGESIZE")?
        .trim()
        .parse()
        .context("parsing guest PAGESIZE")?;
    let uname_r = guest.ssh_exec("uname -r")?.trim().to_string();
    // File NAME only: the stamp lands in committed metrics.json, and an absolute path
    // would publish the private home directory (the repo is public, scrub-token rule).
    let disk = match &cfg.boot {
        crate::Boot::Firmware { disk, .. } | crate::Boot::KernelDisk { disk, .. } => disk
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<no-file-name>".to_string()),
        other => format!("{other:?}"),
    };
    let want = match t {
        Tier::Stock => 4096,
        Tier::Enhanced => 16384,
    };
    anyhow::ensure!(
        pagesize == want,
        "tier mismatch: LIMINA_BENCH_TIER={} but the guest runs PAGESIZE={pagesize} \
         (kernel {uname_r}, disk {disk}) — the wrong image/kernel booted",
        t.label()
    );
    if t == Tier::Enhanced {
        anyhow::ensure!(
            uname_r.starts_with('7'),
            "enhanced tier expected the F44-family 7.x 16k kernel, booted {uname_r} \
             (disk {disk}) — F43 golden by mistake?"
        );
    }
    Ok(TierStamp {
        tier: t,
        pagesize,
        uname_r,
        disk,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CSV columns are a contract between the staged python and the parser; a drift
    /// (like the kswapd/reclaim columns added later) must break loudly here, not by rows
    /// silently failing the arity filter in a live run.
    #[test]
    fn guest_csv_header_and_parser_agree() {
        assert_eq!(GUEST_CSV_HEADER.split(',').count(), GUEST_CSV_COLUMNS);
        let row: Vec<String> = (0..GUEST_CSV_COLUMNS as u64)
            .map(|i| (i * 10).to_string())
            .collect();
        // One good row, then a torn tail row (mid-write) that must be dropped.
        let csv = format!("{GUEST_CSV_HEADER}\n{}\n123,456", row.join(","));
        let samples = parse_guest_csv(&csv).unwrap();
        assert_eq!(samples.len(), 1);
        let s = samples[0];
        assert_eq!(s.ts_ms, 0);
        assert_eq!(s.pswpout, 160);
        assert_eq!(s.pgscan_kswapd, 170);
        assert_eq!(s.pgsteal_direct, 200);
        assert_eq!(s.kswapd_cpu_ticks, 210);
        // A foreign header is a broken pipeline, not an empty result.
        assert!(parse_guest_csv("bogus,header\n1,2\n").is_err());
    }

    /// The staged python must at least be syntactically valid — a `format!` brace slip or
    /// flattened indentation otherwise only surfaces as a dead sampler in a live boot.
    /// Skips silently on a host without python3 (the dev Macs all have it).
    #[test]
    fn staged_python_is_valid() {
        for (name, script) in [
            ("sampler", sampler_script(250)),
            (
                "burst",
                burst_script(3 * 1024 * 1024 * 1024, 512 * 1024 * 1024),
            ),
        ] {
            let dir = std::env::temp_dir()
                .join(format!("limina-bench-pyck-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("staged.py");
            std::fs::write(&path, &script).unwrap();
            let out = std::process::Command::new("python3")
                .args(["-m", "py_compile"])
                .arg(&path)
                .output();
            std::fs::remove_dir_all(&dir).ok();
            match out {
                Ok(o) => assert!(
                    o.status.success(),
                    "{name} script does not compile:\n{}\n--- script ---\n{script}",
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(_) => {
                    eprintln!("SKIPPED staged_python_is_valid: no python3 on host");
                    return;
                }
            }
        }
    }

    /// Trace-line parsing against the exact shape `trace_decision` writes.
    #[test]
    fn trace_parser_reads_the_writer_shape() {
        let jsonl = concat!(
            r#"{"ts_ms":1000,"mode":"Moderate","some_avg10":0,"some_avg60":0,"full_avg10":0,"full_avg60":0,"avail_kib":4194304,"total_kib":6291456,"host_raw_level":1,"host_avail_pct":80,"host":"normal","host_injected":false,"current_pages":0,"decision":"set","new_target_pages":65536,"cooldown_active":false,"sent":true,"actual_bytes":0,"reclaimed_bytes":0,"heals":0,"released_bytes":0,"remapped_bytes":0,"stray_faults":0}"#,
            "\n",
            r#"{"ts_ms":2000,"mode":"Moderate","some_avg10":1500,"some_avg60":300,"full_avg10":10,"full_avg60":5,"avail_kib":262144,"total_kib":6291456,"host_raw_level":null,"host_avail_pct":null,"host":"warn","host_injected":true,"current_pages":65536,"decision":"set","new_target_pages":0,"cooldown_active":true,"sent":true,"actual_bytes":268435456,"reclaimed_bytes":1073741824,"heals":12,"released_bytes":1073741824,"remapped_bytes":4194304,"stray_faults":0}"#,
            "\n",
            r#"{"ts_ms":3000,"mode":"Moderate","some_avg10":0,"some_avg60":0,"full_avg10":0,"full_avg60":0,"avail_kib":4194304,"total_kib":6291456,"host_raw_level":1,"host_avail_pct":80,"host":"normal","host_injected":false,"current_pages":0,"decision":"cooldown","new_target_pages":null,"cooldown_active":true,"sent":false,"actual_bytes":null,"reclaimed_bytes":null,"heals":null,"released_bytes":null,"remapped_bytes":null,"stray_faults":null}"#,
            "\ngarbage line\n",
        );
        let trace = parse_trace(jsonl);
        assert_eq!(trace.len(), 3);
        assert_eq!(trace[0].new_target_pages, Some(65536));
        assert_eq!(trace[0].host, "normal");
        assert!(trace[0].sent && !trace[0].cooldown_active);
        assert_eq!(trace[1].some_avg10, 1500);
        assert_eq!(trace[1].new_target_pages, Some(0));
        assert_eq!(trace[2].decision, "cooldown");
        assert_eq!(trace[2].new_target_pages, None);
        // The release at ts=2000 is the first sent decrease after ts=1500.
        assert_eq!(
            first_target_decrease_after(&trace, 1500),
            Some((2000, 65536, 0))
        );
        assert_eq!(first_target_decrease_after(&trace, 2500), None);
    }

    #[test]
    fn reversal_counting() {
        let ev = |ts: u64, tgt: u64| TraceEvent {
            ts_ms: ts,
            new_target_pages: Some(tgt),
            sent: true,
            ..Default::default()
        };
        // up, up, down, up: two reversals.
        let trace = vec![ev(1, 10), ev(2, 20), ev(3, 30), ev(4, 5), ev(5, 40)];
        assert_eq!(target_reversals(&trace), 2);
    }
}
