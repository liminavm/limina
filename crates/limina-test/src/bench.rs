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
    pub fn stop_and_fetch(&self, guest: &Guest) -> Result<(String, Vec<GuestSample>)> {
        guest
            .ssh_exec("pkill -f '[l]imina-bench-sampler.py' || true")
            .context("stopping the guest sampler")?;
        let csv = guest
            .ssh_exec(&format!("cat {SAMPLER_CSV}"))
            .context("fetching the guest sampler CSV")?;
        let samples = parse_guest_csv(&csv)?;
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
        let mut csv = String::from("ts_ms,target,actual,reclaimed,worker_footprint\n");
        for s in samples {
            let _ = writeln!(
                csv,
                "{},{},{},{},{}",
                s.ts_ms, s.target, s.actual, s.reclaimed, s.worker_footprint
            );
        }
        self.write("host.csv", &csv)
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
    fn sampler_script_is_valid_python() {
        let script = sampler_script(250);
        assert!(script.contains(GUEST_CSV_HEADER));
        let dir = std::env::temp_dir().join(format!("limina-bench-pyck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sampler.py");
        std::fs::write(&path, &script).unwrap();
        let out = std::process::Command::new("python3")
            .args(["-m", "py_compile"])
            .arg(&path)
            .output();
        std::fs::remove_dir_all(&dir).ok();
        match out {
            Ok(o) => assert!(
                o.status.success(),
                "sampler script does not compile:\n{}\n--- script ---\n{script}",
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(_) => eprintln!("SKIPPED sampler_script_is_valid_python: no python3 on host"),
        }
    }
}
