// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M6 PSI autoballoon **policy** (supervisor side). Consumes guest [`MemPressure`] reports from the
//! control plane and drives the balloon target between `0` and `max-min` pages with hysteresis + a
//! dwell, by writing `target <bytes>` to the worker's balloon control socket.
//!
//! Mechanism vs policy: the balloon device, the target/`actual` loop, and the control socket are in
//! libkrun / limina-vmm; *this* is the policy that decides when and how much. The rule is simple and
//! conservative: release fast under pressure (always safe), reclaim gradually when the guest is idle
//! with memory to spare. The thresholds are starting points, not a tuned policy.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use limina_proto::MemPressure;

/// `some` pressure (hundredths of a percent) at/above which we deflate fast (give memory back).
const PRESSURE_HIGH: u32 = 1000; // 10.00%
/// `some` pressure at/below which the guest counts as idle (eligible to inflate).
const PRESSURE_LOW: u32 = 200; // 2.00%
/// Aggressive only: inflate while MemAvailable is at least this fraction of MemTotal (percent).
const IDLE_FREE_PERCENT: u64 = 30;
/// Minimum time between *inflation* steps (releasing under pressure ignores this — it's urgent).
const DWELL: Duration = Duration::from_secs(2);
/// Ignore target changes smaller than this (pages; 16 MiB) — anti-dribble dead band.
const DEAD_BAND_PAGES: u32 = 4096;
/// One inflation step (pages; 256 MiB). The PSI sensor lags the balloon by ~10 s (avg10 window),
/// so the actuator must move slower than the sensor: 256 MiB per 2 s dwell bounds the overshoot
/// between reports to a few hundred MiB. The old ¼-of-room step (5.9 GiB on a 24 GiB VM) inflated
/// 0→20 GB before pressure could register, thrashing the guest to swap.
const INFLATE_STEP_PAGES: u32 = 256 * PAGES_PER_MIB;
/// After a pressure-triggered release, don't re-inflate for this long: a blowout proves the guest
/// is actively using its memory, and each squeeze/release cycle costs it GiBs of disk swap.
const RELEASE_COOLDOWN: Duration = Duration::from_secs(300);

/// 4 KiB balloon pages per MiB.
pub const PAGES_PER_MIB: u32 = 256;

/// How hard the balloon claws guest memory back when the guest is idle. The spike
/// `spikes/mem-overhead-2026-07-02` (Run D) quantified the trade: a full squeeze costs the guest
/// its page cache — 4 KiB warm random reads go 852k IOPS/1 µs → 13.3k IOPS/75 µs (64×) — while
/// buying the host ~2–4 GB at idle. So everything but Aggressive keys the squeeze to *host*
/// memory pressure and leaves the guest a cache allowance when the host doesn't need the RAM.
/// Free-page reporting is unaffected by this knob: it returns only truly-free pages (no cache
/// cost) and always runs.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ReclaimMode {
    /// Never drive the balloon (free-page reporting still returns freed guest memory).
    Disabled,
    /// Host-pressure-driven, generous cache: no inflation while the host is fine; under host
    /// warn leave the guest 25% of max as cache; under critical squeeze down to the minimal
    /// working-set floor (never to zero).
    Light,
    /// Host-pressure-driven (the default): while the host is fine leave the guest 12.5% of max
    /// (min 1 GiB) as cache; under host warn 6.25% (min 1 GiB); under critical squeeze down to
    /// the minimal working-set floor (never to zero).
    Moderate,
    /// Squeeze to the floor whenever the guest is idle, ignoring host pressure (the original
    /// M6 policy).
    Aggressive,
}

/// macOS memory-pressure level, as reported by `kern.memorystatus_vm_pressure_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPressure {
    Normal,
    Warn,
    Critical,
}

impl HostPressure {
    /// Stable lowercase label (trace format).
    fn label(self) -> &'static str {
        match self {
            HostPressure::Normal => "normal",
            HostPressure::Warn => "warn",
            HostPressure::Critical => "critical",
        }
    }
}

/// `kern.memorystatus_level` (percent of memory jetsam counts as available) at/above which the
/// host is demonstrably fine regardless of what the pressure *level* claims.
const HOST_HEALTHY_AVAILABLE_PERCENT: i32 = 40;

/// One host-pressure reading with its components kept apart, so the decision trace can record
/// what the sysctls said, what the blend concluded, and whether an override was in force.
pub struct HostPressureSample {
    /// Raw `kern.memorystatus_vm_pressure_level` (1/2/4), if readable.
    pub raw_level: Option<i32>,
    /// Raw `kern.memorystatus_level` (jetsam available %), if readable.
    pub available_percent: Option<i32>,
    /// The level the policy acts on.
    pub blended: HostPressure,
    /// True when `LIMINA_HOST_PRESSURE` pinned the level (the raw fields are still real).
    pub injected: bool,
}

/// Sample the host's memory-pressure level (1 = normal, 2 = warn, 4 = critical), blended with
/// the actual availability percentage. Errors read as Normal: the host kernel manages its own
/// pressure, and "don't squeeze the guest" is the safe default for guest performance.
///
/// `LIMINA_HOST_PRESSURE=normal|warn|critical` bypasses the sysctls entirely — a test/bench
/// seam, not a user knob: `Light` and `Moderate` differ only under host Warn/Critical, which a
/// healthy dev host never reports, so without injection most of the mode×host matrix is
/// unreachable. Logged loudly (once) when active; the raw sysctl fields stay real either way.
pub fn sample_host_pressure() -> HostPressureSample {
    let raw_level = sysctl_i32(c"kern.memorystatus_vm_pressure_level");
    let available_percent = sysctl_i32(c"kern.memorystatus_level");
    if let Some(level) = host_pressure_override(std::env::var("LIMINA_HOST_PRESSURE").ok()) {
        static ANNOUNCED: std::sync::Once = std::sync::Once::new();
        ANNOUNCED.call_once(|| {
            log::warn!("autoballoon: host pressure OVERRIDDEN to {level:?} (LIMINA_HOST_PRESSURE)");
        });
        return HostPressureSample {
            raw_level,
            available_percent,
            blended: level,
            injected: true,
        };
    }
    HostPressureSample {
        raw_level,
        available_percent,
        blended: blend_host_pressure(raw_level, available_percent),
        injected: false,
    }
}

/// Parse the `LIMINA_HOST_PRESSURE` override. Unset/empty means no override; a value that
/// parses to none of the three levels is a misconfigured bench run and must not silently read
/// as real sysctls, so it pins Normal (and [`sample_host_pressure`] announces the override).
///
/// `@/path/to/file` re-reads the level from that file on every sample: a process env var is
/// frozen at spawn, but the bench's S6 staircase needs the level to *change mid-run* — the
/// harness rewrites the file. An unreadable/empty file, unlike an unset var, is still an
/// active override (pinned Normal): mid-bench the real sysctls must never leak back in.
fn host_pressure_override(var: Option<String>) -> Option<HostPressure> {
    let var = var?;
    let var = var.trim();
    if var.is_empty() {
        return None;
    }
    let level = if let Some(path) = var.strip_prefix('@') {
        std::fs::read_to_string(path).unwrap_or_default()
    } else {
        var.to_string()
    };
    Some(match level.trim().to_ascii_lowercase().as_str() {
        "warn" => HostPressure::Warn,
        "critical" => HostPressure::Critical,
        _ => HostPressure::Normal,
    })
}

fn sysctl_i32(name: &std::ffi::CStr) -> Option<i32> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value)
}

/// `kern.memorystatus_vm_pressure_level` is STICKY around swap: a host can report Warn
/// with ~49% of RAM free because the swapfile stays near-full (macOS never proactively drains
/// it), and the policy then squeezes the guest to a tiny balloon against a healthy host for hours.
/// Demote one level when `kern.memorystatus_level` (jetsam's available-memory percentage — the
/// number `memory_pressure -Q` prints) says the host demonstrably has memory.
fn blend_host_pressure(
    pressure_level: Option<i32>,
    available_percent: Option<i32>,
) -> HostPressure {
    let raw = match pressure_level {
        Some(l) if l >= 4 => HostPressure::Critical,
        Some(l) if l >= 2 => HostPressure::Warn,
        _ => HostPressure::Normal,
    };
    let healthy = available_percent.is_some_and(|p| p >= HOST_HEALTHY_AVAILABLE_PERCENT);
    match (raw, healthy) {
        (HostPressure::Critical, true) => HostPressure::Warn,
        (HostPressure::Warn, true) => HostPressure::Normal,
        (level, _) => level,
    }
}

/// The supervisor-side autoballoon policy. Cheap to construct; thread-safe (`on_pressure` locks).
pub struct BalloonPolicy {
    /// Floor: effective guest RAM never shrinks below this (in 4 KiB pages).
    min_pages: u32,
    /// Ceiling: total guest RAM libkrun allocated (in 4 KiB pages).
    max_pages: u32,
    /// How hard to claw back (see [`ReclaimMode`]).
    mode: ReclaimMode,
    /// The worker's balloon control socket (`target <bytes>` / `stats`).
    socket: PathBuf,
    state: Mutex<State>,
}

struct State {
    /// A kept-open connection to the balloon socket (reconnected on error).
    conn: Option<UnixStream>,
    /// The balloon size we've currently commanded (4 KiB pages).
    target_pages: u32,
    /// When we last changed the target (for the inflation dwell).
    last_change: Option<Instant>,
    /// No inflation before this instant (armed on every high-pressure report).
    cooldown_until: Option<Instant>,
    /// `LIMINA_BALLOON_TRACE` decision journal: one JSON line per consumed report.
    trace: Option<std::fs::File>,
}

impl BalloonPolicy {
    /// `default_trace`: where the decision journal goes when `LIMINA_BALLOON_TRACE` is
    /// unset — managed VMs pass `<bundle>/logs/balloon-trace.jsonl` so a Dock-launched
    /// app traces without any environment plumbing; flat CLI runs pass `None` (env-only,
    /// no surprise files). The default path is rotated (one `.1` generation) at every
    /// VM start so it stays bounded to two boots; an explicit env path keeps plain
    /// append semantics (the bench harness owns those files and their lifecycle).
    pub fn new(
        min_pages: u32,
        max_pages: u32,
        mode: ReclaimMode,
        socket: PathBuf,
        default_trace: Option<PathBuf>,
    ) -> Self {
        // The bench's decision journal (docs/design/balloon-bench.md §3): every consumed
        // report with the verdict AND the gate that held — "why didn't it move" is the
        // question incident debugging keeps re-deriving from scattered logs.
        let trace = match std::env::var("LIMINA_BALLOON_TRACE")
            .ok()
            .filter(|p| !p.is_empty())
        {
            Some(p) => match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
            {
                Ok(f) => {
                    log::warn!("autoballoon: decision trace -> {p}");
                    Some(f)
                }
                Err(e) => {
                    log::warn!("autoballoon: cannot open LIMINA_BALLOON_TRACE {p:?}: {e}");
                    None
                }
            },
            None => default_trace.and_then(|p| {
                let rotated = p.with_extension("1.jsonl");
                if p.exists() {
                    let _ = std::fs::rename(&p, &rotated);
                }
                match p
                    .parent()
                    .map(std::fs::create_dir_all)
                    .transpose()
                    .and_then(|_| {
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&p)
                    }) {
                    Ok(f) => {
                        log::warn!("autoballoon: decision trace -> {}", p.display());
                        Some(f)
                    }
                    Err(e) => {
                        log::warn!("autoballoon: cannot open trace {}: {e}", p.display());
                        None
                    }
                }
            }),
        };
        Self {
            min_pages,
            max_pages,
            mode,
            socket,
            state: Mutex::new(State {
                conn: None,
                target_pages: 0,
                last_change: None,
                cooldown_until: None,
                trace,
            }),
        }
    }

    /// The most the balloon may inflate to, leaving the guest with `min`.
    fn room(&self) -> u32 {
        self.max_pages.saturating_sub(self.min_pages)
    }

    /// Decide the next target from a pressure report and drive it. Pure decision in
    /// [`decide`]; this wrapper samples host pressure and adds the I/O (connect + write) and
    /// dwell bookkeeping.
    pub fn on_pressure(&self, p: &MemPressure) {
        let room = self.room();
        if room == 0 || self.mode == ReclaimMode::Disabled {
            return;
        }
        let host = sample_host_pressure();
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        // A high-pressure or starvation report arms the re-inflation cooldown whether or not
        // there is a balloon to release: the guest just proved it needs its memory.
        if p.some_avg10 >= PRESSURE_HIGH || guest_starved(p) {
            st.cooldown_until = Some(now + RELEASE_COOLDOWN);
        }
        let inputs = DecideInputs {
            mode: self.mode,
            host: host.blended,
            current: st.target_pages,
            room,
            max_pages: self.max_pages,
            last_change: st.last_change,
            cooldown_until: st.cooldown_until,
            now,
        };
        let decision = decide(p, &inputs);
        let mut sent = false;
        if let Decision::Set(new_target) = decision {
            sent = self.send_target(&mut st, new_target);
            if sent {
                st.target_pages = new_target;
                st.last_change = Some(now);
                log::debug!(
                    "autoballoon: target -> {new_target} pages (some_avg10={}, avail/total={}/{})",
                    p.some_avg10,
                    p.mem_available_kib,
                    p.mem_total_kib
                );
            }
        }
        trace_decision(&mut st, p, &host, &inputs, decision, sent);
    }

    /// Write `target <bytes>` to the balloon socket, reconnecting once on failure. Returns whether
    /// the command went out.
    fn send_target(&self, st: &mut State, pages: u32) -> bool {
        let bytes = (pages as u64) << 12;
        for attempt in 0..2 {
            if st.conn.is_none() {
                match UnixStream::connect(&self.socket) {
                    Ok(c) => st.conn = Some(c),
                    Err(e) => {
                        if attempt == 1 {
                            log::warn!("autoballoon: connect {:?}: {e}", self.socket);
                        }
                        continue;
                    }
                }
            }
            let conn = st.conn.as_mut().unwrap();
            if writeln!(conn, "target {bytes}")
                .and_then(|()| conn.flush())
                .is_ok()
            {
                return true;
            }
            st.conn = None; // broken pipe — drop and retry once
        }
        false
    }
}

/// Append one JSON line to the decision journal (no-op without `LIMINA_BALLOON_TRACE`).
/// Hand-formatted flat JSON — stable keys for the bench summarizer, no serde in the hot path.
/// A write error drops the trace (never the policy).
fn trace_decision(
    st: &mut State,
    p: &MemPressure,
    host: &HostPressureSample,
    i: &DecideInputs,
    decision: Decision,
    sent: bool,
) {
    let Some(f) = st.trace.as_mut() else {
        return;
    };
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let json_opt = |v: Option<i32>| v.map_or("null".to_string(), |v| v.to_string());
    let new_target = decision
        .target()
        .map_or("null".to_string(), |t| t.to_string());
    let cooldown_active = i.cooldown_until.is_some_and(|t| i.now < t);
    let line = format!(
        concat!(
            "{{\"ts_ms\":{},\"mode\":\"{:?}\",",
            "\"some_avg10\":{},\"some_avg60\":{},\"full_avg10\":{},\"full_avg60\":{},",
            "\"avail_kib\":{},\"total_kib\":{},",
            "\"host_raw_level\":{},\"host_avail_pct\":{},\"host\":\"{}\",\"host_injected\":{},",
            "\"current_pages\":{},\"decision\":\"{}\",\"new_target_pages\":{},",
            "\"cooldown_active\":{},\"sent\":{}}}\n"
        ),
        ts_ms,
        i.mode,
        p.some_avg10,
        p.some_avg60,
        p.full_avg10,
        p.full_avg60,
        p.mem_available_kib,
        p.mem_total_kib,
        json_opt(host.raw_level),
        json_opt(host.available_percent),
        host.blended.label(),
        host.injected,
        i.current,
        decision.label(),
        new_target,
        cooldown_active,
        sent,
    );
    if f.write_all(line.as_bytes()).is_err() {
        st.trace = None;
    }
}

/// Everything [`decide`] needs besides the guest report, bundled so the function stays pure and
/// unit-testable (host pressure is sampled by the caller, never inside).
struct DecideInputs {
    mode: ReclaimMode,
    host: HostPressure,
    /// The balloon size we've currently commanded (pages).
    current: u32,
    /// max − min: the most the balloon may hold (pages).
    room: u32,
    /// Total guest RAM libkrun allocated (pages) — the allowance percentages key off this.
    max_pages: u32,
    last_change: Option<Instant>,
    /// No inflation before this instant (armed by the caller on high-pressure reports).
    cooldown_until: Option<Instant>,
    now: Instant,
}

/// The cache allowance for a mode/host-pressure pair: how much of the guest's memory the policy
/// must leave available (as page cache) when inflating. `None` = do not hold any balloon at all
/// in this state (drift the target to 0). Numbers from `spikes/mem-overhead-2026-07-02` Run D:
/// even a few hundred MiB of cache keeps small hot sets at ~1 µs hits; a full squeeze costs 64×
/// on warm random reads.
fn allowance_pages(mode: ReclaimMode, host: HostPressure, max_pages: u32) -> Option<u32> {
    const GIB: u32 = 1024 * PAGES_PER_MIB;
    // Even a critical-pressure squeeze leaves the guest a minimal cache working set: a
    // zero-allowance target strands a desktop guest re-reading its executables from disk at
    // GB/s with idle CPUs (observed as ~263 MiB available, io-PSI full 44%, memory-PSI
    // quiet — unusable, and invisible to the PSI release gate).
    let squeeze_floor = (max_pages / 32).max(GIB / 2);
    match (mode, host) {
        (ReclaimMode::Disabled, _) => None, // not reached: policy isn't constructed
        (ReclaimMode::Light, HostPressure::Normal) => None,
        (ReclaimMode::Light, HostPressure::Warn) => Some((max_pages / 4).max(2 * GIB)),
        (ReclaimMode::Light, HostPressure::Critical) => Some(squeeze_floor),
        (ReclaimMode::Moderate, HostPressure::Normal) => Some((max_pages / 8).max(GIB)),
        (ReclaimMode::Moderate, HostPressure::Warn) => Some((max_pages / 16).max(GIB)),
        (ReclaimMode::Moderate, HostPressure::Critical) => Some(squeeze_floor),
        (ReclaimMode::Aggressive, _) => Some(0),
    }
}

/// A guest is *starved* when MemAvailable is critically low: catastrophic cache starvation
/// manifests as IO pressure (swap-in and refault storms), NOT as memory-PSI — a wedged guest
/// can sit at 263 MiB available / 2.3% memory-some / 44% io-full, stuck behind a
/// 21 GiB balloon the PSI gates never release. Available this low while we hold a balloon is
/// the balloon's fault by definition.
fn guest_starved(p: &MemPressure) -> bool {
    p.mem_total_kib > 0 && p.mem_available_kib < (256 * 1024).max(p.mem_total_kib / 64)
}

/// The pure policy verdict: a new target, or a hold carrying the gate that held. The reason is
/// not decoration — "why didn't it move" is the question every balloon incident re-derived from
/// scattered logs, and the bench's decision trace records it per report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Command this new target (pages).
    Set(u32),
    /// Leave the current target alone.
    Hold(Hold),
}

impl Decision {
    /// The commanded target, if any (test/trace convenience).
    fn target(self) -> Option<u32> {
        match self {
            Decision::Set(t) => Some(t),
            Decision::Hold(_) => None,
        }
    }

    /// Short stable label for the trace (`set` or the hold gate).
    fn label(self) -> &'static str {
        match self {
            Decision::Set(_) => "set",
            Decision::Hold(Hold::Converged) => "converged",
            Decision::Hold(Hold::NotIdle) => "not-idle",
            Decision::Hold(Hold::DeadBand) => "dead-band",
            Decision::Hold(Hold::NotCalm) => "not-calm",
            Decision::Hold(Hold::Cooldown) => "cooldown",
            Decision::Hold(Hold::Dwell) => "dwell",
        }
    }
}

/// Which gate held the target in place (see [`decide`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// The desired target equals the current one (including "release, but already at 0").
    Converged,
    /// Aggressive only: the guest has <30% available, not idle enough to squeeze.
    NotIdle,
    /// The adjustment is smaller than the dead band.
    DeadBand,
    /// Inflation gate: PSI is not sustainedly calm (avg10 or avg60 above the low threshold).
    NotCalm,
    /// Inflation gate: inside the post-release cooldown.
    Cooldown,
    /// Inflation gate: inside the dwell since the last target change.
    Dwell,
}

/// The pure policy decision (unit-tested): given a pressure report and the current state, return
/// the next target in pages, or the gate that held it.
///
/// All modes release to 0 immediately when the *guest* is under acute pressure (avg10 ≥ 10%).
/// Deflation is otherwise always allowed, at any pressure: when available drops below the mode's
/// cache [`allowance_pages`] the target shrinks by the shortfall immediately — giving memory back
/// is always safe, and holding it was what let the guest thrash against an unreachable target
/// (a squeeze/release limit cycle). Inflation is the guarded direction: it requires
/// *sustained* calm (avg10 AND avg60 ≤ 2%), no recent release blowout ([`RELEASE_COOLDOWN`]),
/// and moves in small dwell-limited [`INFLATE_STEP_PAGES`] steps so the lagging PSI sensor can
/// push back before the squeeze overshoots. Aggressive keeps its original shape (squeeze to the
/// floor while ≥30% is available, host pressure ignored) with the same inflation guards.
fn decide(p: &MemPressure, i: &DecideInputs) -> Decision {
    // Guest under acute pressure OR starved of cache: hand memory back, now. All modes. (The
    // caller also arms the re-inflation cooldown on these signals.) The starvation check exists
    // because thrash shows up as IO pressure, not memory-PSI — see [`guest_starved`].
    if p.some_avg10 >= PRESSURE_HIGH || guest_starved(p) {
        return if i.current != 0 {
            Decision::Set(0)
        } else {
            Decision::Hold(Hold::Converged)
        };
    }

    let desired = match allowance_pages(i.mode, i.host, i.max_pages) {
        // Host is fine and the mode says don't hold a balloon at all: give it all back.
        None => 0,
        Some(0) if i.mode == ReclaimMode::Aggressive => {
            // Original M6 behavior: squeeze to the floor while the guest has ≥30% available.
            let idle_free = p.mem_total_kib > 0
                && p.mem_available_kib.saturating_mul(100)
                    >= p.mem_total_kib.saturating_mul(IDLE_FREE_PERCENT);
            if !idle_free {
                return Decision::Hold(Hold::NotIdle);
            }
            i.room
        }
        Some(allow) => {
            // The balloon may take what the guest has available beyond the allowance; if the
            // guest's available has dropped below the allowance, give some back.
            let avail_pages = (p.mem_available_kib / 4).min(u32::MAX as u64) as u32;
            if avail_pages >= allow {
                i.current.saturating_add(avail_pages - allow).min(i.room)
            } else {
                i.current.saturating_sub(allow - avail_pages)
            }
        }
    };

    if desired.abs_diff(i.current) < DEAD_BAND_PAGES && desired != 0 {
        return Decision::Hold(if desired == i.current {
            Hold::Converged
        } else {
            Hold::DeadBand
        });
    }
    let next = if desired <= i.current {
        // Deflation: immediate, no idle/cooldown/dwell gates.
        desired
    } else {
        // Inflation: only from a sustainedly calm guest (a 10 s window is just a busy guest
        // catching its breath), never inside the post-release cooldown, one small step per dwell.
        if p.some_avg10 > PRESSURE_LOW || p.some_avg60 > PRESSURE_LOW {
            return Decision::Hold(Hold::NotCalm);
        }
        if i.cooldown_until.is_some_and(|t| i.now < t) {
            return Decision::Hold(Hold::Cooldown);
        }
        if let Some(t) = i.last_change {
            if i.now.duration_since(t) < DWELL {
                return Decision::Hold(Hold::Dwell);
            }
        }
        i.current.saturating_add(INFLATE_STEP_PAGES).min(desired)
    };
    if next != i.current {
        Decision::Set(next)
    } else {
        Decision::Hold(Hold::Converged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8 GiB VM with a 1 GiB floor, in pages: plenty of space above every allowance clamp.
    const MAX: u32 = 8 * 1024 * PAGES_PER_MIB;
    const ROOM: u32 = 7 * 1024 * PAGES_PER_MIB;
    const GIB_PAGES: u32 = 1024 * PAGES_PER_MIB;

    fn pressure(some_avg10: u32, avail_kib: u64, total_kib: u64) -> MemPressure {
        MemPressure {
            some_avg10,
            some_avg60: 0,
            full_avg10: 0,
            full_avg60: 0,
            mem_available_kib: avail_kib,
            mem_total_kib: total_kib,
        }
    }

    /// Report with `avail`/`total` given in pages for easy comparison with targets.
    fn report_pages(some_avg10: u32, avail_pages: u32, total_pages: u32) -> MemPressure {
        pressure(some_avg10, avail_pages as u64 * 4, total_pages as u64 * 4)
    }

    fn inputs(mode: ReclaimMode, host: HostPressure, current: u32) -> DecideInputs {
        DecideInputs {
            mode,
            host,
            current,
            room: ROOM,
            max_pages: MAX,
            last_change: None,
            cooldown_until: None,
            now: Instant::now(),
        }
    }

    #[test]
    fn guest_pressure_releases_to_zero_immediately_in_every_mode() {
        for mode in [
            ReclaimMode::Light,
            ReclaimMode::Moderate,
            ReclaimMode::Aggressive,
        ] {
            for host in [
                HostPressure::Normal,
                HostPressure::Warn,
                HostPressure::Critical,
            ] {
                let mut i = inputs(mode, host, ROOM / 2);
                i.last_change = Some(i.now); // release must ignore the dwell
                let next = decide(&report_pages(5000, GIB_PAGES / 8, MAX), &i);
                assert_eq!(next, Decision::Set(0), "{mode:?}/{host:?}");
            }
        }
    }

    #[test]
    fn aggressive_keeps_the_original_shape() {
        // Idle with ≥30% available: one step toward full, regardless of host pressure.
        let i = inputs(ReclaimMode::Aggressive, HostPressure::Normal, 0);
        let next = decide(&report_pages(0, MAX * 7 / 10, MAX), &i);
        assert_eq!(next, Decision::Set(INFLATE_STEP_PAGES));
        // Idle but <30% available: hold.
        let next = decide(&report_pages(0, MAX / 10, MAX), &i);
        assert_eq!(next, Decision::Hold(Hold::NotIdle));
        // Dwell gates inflation.
        let mut i = inputs(ReclaimMode::Aggressive, HostPressure::Critical, ROOM / 4);
        i.last_change = Some(i.now);
        let next = decide(&report_pages(0, MAX * 7 / 10, MAX), &i);
        assert_eq!(next, Decision::Hold(Hold::Dwell));
    }

    #[test]
    fn neutral_band_holds_inflation() {
        // some=5% is between LOW(2%) and HIGH(10%) -> no inflation, even with room to take.
        // (Light@Normal is excluded here: its desired target is 0, a deflation — covered by
        // `light_normal_gives_back_in_the_neutral_band`.)
        for mode in [ReclaimMode::Moderate, ReclaimMode::Aggressive] {
            let i = inputs(mode, HostPressure::Normal, ROOM / 4);
            let next = decide(&report_pages(500, MAX / 2, MAX), &i);
            assert_eq!(next, Decision::Hold(Hold::NotCalm), "{mode:?}");
        }
    }

    #[test]
    fn moderate_normal_leaves_the_cache_allowance() {
        // Guest idle with 4 GiB available; allowance = max/8 = 1 GiB. The balloon may take
        // avail − allowance = 3 GiB, one step at a time.
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        let next = decide(&report_pages(0, 4 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Set(INFLATE_STEP_PAGES));
        // Fully converged: current already at avail − allowance → hold (sub-dead-band).
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 3 * GIB_PAGES);
        let next = decide(&report_pages(0, GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Hold(Hold::Converged));
    }

    #[test]
    fn moderate_gives_cache_back_when_guest_dips_below_allowance() {
        // Guest available fell to 256 MiB (< 1 GiB allowance) while idle: deflate by the
        // shortfall, immediately (no dwell on deflation).
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 3 * GIB_PAGES);
        i.last_change = Some(i.now);
        let next = decide(&report_pages(0, GIB_PAGES / 4, MAX), &i);
        assert_eq!(
            next,
            Decision::Set(3 * GIB_PAGES - (GIB_PAGES - GIB_PAGES / 4))
        );
    }

    #[test]
    fn moderate_squeezes_harder_under_host_warn_but_keeps_a_floor() {
        // Host warn → allowance max/16 (min 1 GiB): with 4 GiB available the balloon may take
        // 3 GiB, stepped; from 0 that's one step.
        let i = inputs(ReclaimMode::Moderate, HostPressure::Warn, 0);
        let next = decide(&report_pages(0, 4 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Set(INFLATE_STEP_PAGES));
        // The allowance never reaches zero under warn (the sticky-Warn wedge class).
        assert_eq!(
            allowance_pages(ReclaimMode::Moderate, HostPressure::Warn, MAX),
            Some(GIB_PAGES)
        );
    }

    #[test]
    fn light_normal_holds_no_balloon() {
        // Host fine → Light drifts the target to 0 (give the guest its cache back)…
        let i = inputs(ReclaimMode::Light, HostPressure::Normal, 2 * GIB_PAGES);
        let next = decide(&report_pages(0, 2 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Set(0));
        // …and stays at 0.
        let i = inputs(ReclaimMode::Light, HostPressure::Normal, 0);
        let next = decide(&report_pages(0, 4 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Hold(Hold::Converged));
    }

    #[test]
    fn light_engages_under_host_warn_with_a_generous_allowance() {
        // Host warn → allowance max/4 = 2 GiB; guest has 4 GiB available → may take 2 GiB,
        // one step at a time.
        let i = inputs(ReclaimMode::Light, HostPressure::Warn, 0);
        let next = decide(&report_pages(0, 4 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Set(INFLATE_STEP_PAGES));
    }

    #[test]
    fn light_critical_squeezes_to_the_floor() {
        // allowance = the minimal working-set floor: desired = current + (avail − floor),
        // capped at room; the last step lands exactly on the floor squeeze.
        let i = inputs(
            ReclaimMode::Light,
            HostPressure::Critical,
            ROOM - INFLATE_STEP_PAGES / 2,
        );
        let next = decide(&report_pages(0, 2 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Set(ROOM));
    }

    /// Oscillation regression 1: inflation must move in small bounded
    /// steps (the PSI sensor lags ~10 s; ¼-of-room steps outran it and thrashed the guest).
    #[test]
    fn inflation_steps_are_small_and_bounded() {
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        // 7 GiB available, 1 GiB allowance: desired is ~6 GiB away, but one decision may only
        // move one INFLATE_STEP.
        let next = decide(&report_pages(0, 7 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Set(INFLATE_STEP_PAGES));
    }

    /// Regression 2: while the guest sits in the neutral band (2–10%), the policy must still
    /// give memory back when available drops below the allowance — waiting for ≤2% calm left
    /// the guest thrashing against an unreachable target until the 10% panic release.
    #[test]
    fn neutral_band_still_deflates_below_the_allowance() {
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 5 * GIB_PAGES);
        // Deflation must ignore the dwell.
        i.last_change = Some(i.now);
        // PSI 7%, available 256 MiB < 1 GiB allowance: deflate by the shortfall, now.
        let next = decide(&report_pages(700, GIB_PAGES / 4, MAX), &i);
        assert_eq!(
            next,
            Decision::Set(5 * GIB_PAGES - (GIB_PAGES - GIB_PAGES / 4))
        );
    }

    /// Regression 2b: Light at host-normal holds no balloon; that drift-to-0 is a deflation and
    /// must not wait for the guest to go calm either.
    #[test]
    fn light_normal_gives_back_in_the_neutral_band() {
        let i = inputs(ReclaimMode::Light, HostPressure::Normal, 2 * GIB_PAGES);
        let next = decide(&report_pages(500, 2 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Decision::Set(0));
    }

    /// Regression 3: a 10-second calm window is not "idle" — inflation also requires the 60 s
    /// average to be low, so a busy guest catching its breath doesn't get squeezed.
    #[test]
    fn inflation_requires_sustained_calm() {
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        let mut p = report_pages(0, 7 * GIB_PAGES, MAX);
        p.some_avg60 = 500; // 5% over the last minute
        assert_eq!(decide(&p, &i), Decision::Hold(Hold::NotCalm));
    }

    /// Regression 4: after a pressure-triggered release the policy must back off, not re-inflate
    /// the moment avg10 decays — that was the all-day 40 s squeeze/thrash/dump limit cycle.
    #[test]
    fn release_cooldown_blocks_reinflation() {
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        i.cooldown_until = Some(i.now + Duration::from_secs(100));
        assert_eq!(
            decide(&report_pages(0, 7 * GIB_PAGES, MAX), &i),
            Decision::Hold(Hold::Cooldown)
        );
        // Cooldown elapsed: inflation resumes.
        i.cooldown_until = Some(i.now - Duration::from_secs(1));
        assert_eq!(
            decide(&report_pages(0, 7 * GIB_PAGES, MAX), &i),
            Decision::Set(INFLATE_STEP_PAGES)
        );
        // Deflation is never cooldown-gated (giving memory back is always safe).
        i.cooldown_until = Some(i.now + Duration::from_secs(100));
        i.current = 5 * GIB_PAGES;
        let next = decide(&report_pages(700, GIB_PAGES / 4, MAX), &i);
        assert_eq!(
            next,
            Decision::Set(5 * GIB_PAGES - (GIB_PAGES - GIB_PAGES / 4))
        );
    }

    #[test]
    fn dead_band_swallows_dribble() {
        // A 8 MiB adjustment (< 16 MiB dead band) is held.
        let cur = 2 * GIB_PAGES;
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, cur);
        let next = decide(&report_pages(0, GIB_PAGES + 2048, MAX), &i);
        assert_eq!(next, Decision::Hold(Hold::DeadBand));
    }

    #[test]
    fn allowance_clamps_have_floors() {
        // A small 2 GiB VM: Moderate normal allowance is the 1 GiB floor, not max/8 = 256 MiB.
        let small_max = 2 * GIB_PAGES;
        assert_eq!(
            allowance_pages(ReclaimMode::Moderate, HostPressure::Normal, small_max),
            Some(GIB_PAGES)
        );
        assert_eq!(
            allowance_pages(ReclaimMode::Light, HostPressure::Warn, small_max),
            Some(2 * GIB_PAGES)
        );
        // Warn keeps the 1 GiB floor; Critical keeps the minimal working-set floor. Zero
        // allowances died with the sticky-Warn wedge.
        assert_eq!(
            allowance_pages(ReclaimMode::Moderate, HostPressure::Warn, small_max),
            Some(GIB_PAGES)
        );
        assert_eq!(
            allowance_pages(ReclaimMode::Moderate, HostPressure::Critical, small_max),
            Some(GIB_PAGES / 2)
        );
        assert_eq!(
            allowance_pages(ReclaimMode::Light, HostPressure::Critical, small_max),
            Some(GIB_PAGES / 2)
        );
    }

    /// Sticky-Warn wedge regression: a 24 GiB VM squeezed to a ~21 GiB
    /// balloon, 263 MiB available, memory-PSI quiet (2.28% — the thrash showed as 44% io-full,
    /// which the policy can't see), host stuck at Warn. The policy held forever. A starved
    /// guest must release the balloon regardless of memory-PSI, host state, or mode.
    #[test]
    fn starved_guest_releases_even_under_host_warn() {
        let max = 24 * GIB_PAGES;
        let current = 21 * GIB_PAGES;
        for mode in [
            ReclaimMode::Light,
            ReclaimMode::Moderate,
            ReclaimMode::Aggressive,
        ] {
            let mut i = inputs(mode, HostPressure::Warn, current);
            i.room = max - GIB_PAGES;
            i.max_pages = max;
            i.last_change = Some(i.now); // release must ignore the dwell
            let p = MemPressure {
                some_avg10: 228,
                some_avg60: 308,
                full_avg10: 228,
                full_avg60: 307,
                mem_available_kib: 263 * 1024,
                mem_total_kib: 24870560,
            };
            assert_eq!(decide(&p, &i), Decision::Set(0), "{mode:?}");
        }
    }

    /// Below-allowance (but not starved) under Warn deflates by the shortfall — the allowance
    /// floor is what makes this reachable (the old zero allowance under Warn never deflated).
    #[test]
    fn moderate_warn_deflates_to_the_floor_allowance() {
        let max = 24 * GIB_PAGES;
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Warn, 21 * GIB_PAGES);
        i.room = max - GIB_PAGES;
        i.max_pages = max;
        i.last_change = Some(i.now);
        // 600 MiB available; allowance = max/16 = 1.5 GiB → give back the 936 MiB shortfall.
        let avail = 600 * PAGES_PER_MIB;
        let allow = max / 16;
        let next = decide(&report_pages(300, avail, max), &i);
        assert_eq!(next, Decision::Set(21 * GIB_PAGES - (allow - avail)));
    }

    /// The trace line is consumed by the bench summarizer: keys and shapes are a contract.
    /// One `Set` line and one `Hold` line, round-tripped through a real file.
    #[test]
    fn trace_lines_carry_the_verdict_and_the_gate() {
        let path = std::env::temp_dir().join(format!("balloon-trace-test-{}", std::process::id()));
        let mut st = State {
            conn: None,
            target_pages: 0,
            last_change: None,
            cooldown_until: None,
            trace: Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap(),
            ),
        };
        let host = HostPressureSample {
            raw_level: Some(2),
            available_percent: Some(49),
            blended: HostPressure::Normal,
            injected: false,
        };
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        trace_decision(
            &mut st,
            &report_pages(0, 4 * GIB_PAGES, MAX),
            &host,
            &i,
            Decision::Set(INFLATE_STEP_PAGES),
            true,
        );
        i.cooldown_until = Some(i.now + Duration::from_secs(100));
        trace_decision(
            &mut st,
            &report_pages(0, 4 * GIB_PAGES, MAX),
            &host,
            &i,
            Decision::Hold(Hold::Cooldown),
            false,
        );
        let out = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(
            lines[0].contains("\"decision\":\"set\"")
                && lines[0].contains(&format!("\"new_target_pages\":{INFLATE_STEP_PAGES}"))
                && lines[0].contains("\"sent\":true")
                && lines[0].contains("\"host\":\"normal\"")
                && lines[0].contains("\"host_raw_level\":2"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("\"decision\":\"cooldown\"")
                && lines[1].contains("\"new_target_pages\":null")
                && lines[1].contains("\"cooldown_active\":true")
                && lines[1].contains("\"sent\":false"),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn host_pressure_override_parses_the_bench_seam() {
        let over = |s: &str| host_pressure_override(Some(s.to_string()));
        assert_eq!(host_pressure_override(None), None);
        assert_eq!(over(""), None);
        assert_eq!(over("normal"), Some(HostPressure::Normal));
        assert_eq!(over("Warn"), Some(HostPressure::Warn));
        assert_eq!(over("CRITICAL"), Some(HostPressure::Critical));
        // A typo must not silently fall through to the real sysctls mid-bench.
        assert_eq!(over("critcal"), Some(HostPressure::Normal));
    }

    /// `@file` re-reads the level per sample (the S6 staircase seam); a vanished file stays
    /// an active override (Normal), never a fall-through to the sysctls.
    #[test]
    fn host_pressure_override_follows_a_file() {
        let path = std::env::temp_dir().join(format!("hp-seam-test-{}", std::process::id()));
        let arg = Some(format!("@{}", path.display()));
        std::fs::write(&path, "warn\n").unwrap();
        assert_eq!(
            host_pressure_override(arg.clone()),
            Some(HostPressure::Warn)
        );
        std::fs::write(&path, "critical").unwrap();
        assert_eq!(
            host_pressure_override(arg.clone()),
            Some(HostPressure::Critical)
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(host_pressure_override(arg), Some(HostPressure::Normal));
    }

    #[test]
    fn sticky_host_warn_with_healthy_availability_reads_as_normal() {
        // level=2 (Warn) with 49% available — swap-full stickiness.
        assert_eq!(blend_host_pressure(Some(2), Some(49)), HostPressure::Normal);
        // Genuine warn (little available memory) is preserved.
        assert_eq!(blend_host_pressure(Some(2), Some(15)), HostPressure::Warn);
        // Critical demotes one level when availability is healthy, never two.
        assert_eq!(blend_host_pressure(Some(4), Some(49)), HostPressure::Warn);
        assert_eq!(
            blend_host_pressure(Some(4), Some(10)),
            HostPressure::Critical
        );
        // Missing/failed sysctls: pressure level stands alone; total failure reads Normal.
        assert_eq!(blend_host_pressure(Some(2), None), HostPressure::Warn);
        assert_eq!(blend_host_pressure(None, None), HostPressure::Normal);
    }
}
