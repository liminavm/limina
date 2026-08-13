// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M6 PSI autoballoon **policy** (supervisor side). Consumes guest [`MemPressure`] reports from the
//! control plane and drives the balloon target between `0` and `max-min` pages with hysteresis + a
//! dwell, by writing `target <bytes>` to the worker's balloon control socket.
//!
//! Mechanism vs policy: the balloon device, the target/`actual` loop, and the control socket are in
//! libkrun / limina-vmm; *this* is the policy that decides when and how much. The rule is simple and
//! conservative: release fast under pressure (always safe), reclaim gradually when the guest is idle
//! with memory to spare. Because the guest driver has NO self-preservation (it satisfies any target,
//! digging into page cache and toward guest death at full inflate speed), the protective wall is
//! host-side: inflation steps are paced by the guest's reported MemFree (the clamp in [`decide`]),
//! and a commanded target the driver can't fill decays back to `actual` instead of standing as a
//! permanent 5 Hz retry loop ([`gap_action`]).
//!
//! Besides the reactive target loop, the policy runs a **pressure-triggered scrub**
//! (`spikes/balloon-retention-testbed/`): one inflate → hold → deflate cycle. Inflating routes
//! guest-freed pages through the release path (unmap), which settles the *retention pool* —
//! content the guest dirtied then freed that the host compressor keeps billing to the worker — and
//! is the only measured lever that shrinks phys_footprint (14.5 → 6.9 G on the testbed). Trigger
//! level, cadence, and depth are mode-keyed ([`scrub_params`]): Light/Moderate run *bounded*
//! cycles (inflate only over the guest's free list — captures the dead share, keeps the page
//! cache), Aggressive runs eager full cycles (also reaches cold cache, dumping it). [`scrub_due`]
//! keeps every variant rare and host-need-driven. `LIMINA_BALLOON_SCRUB=0` disables it.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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
/// Inflation step when the guest's free list is exhausted but the squeeze contract still owes
/// memory (host Warn/Critical, or Aggressive): 32 MiB per dwell ≈ 16 MiB/s. The driver satisfies
/// any target by digging into page cache at full inflate speed; this keeps the dig at a pace the
/// guest's reclaim absorbs instead of the 128 MiB/s cache-dump sprints.
const TRICKLE_STEP_PAGES: u32 = 32 * PAGES_PER_MIB;
/// A commanded-but-unfilled target gap smaller than this is noise (pages; same as the dead band).
const GAP_EPS_PAGES: u32 = DEAD_BAND_PAGES;
/// How long a target>actual gap must sit with NO fill progress before the target decays to
/// `actual`. The driver fills a 256 MiB step in ~140 ms when it can; ten seconds of a stuck gap
/// is the driver's permanent 5 Hz retry loop ("Out of puff"), not a slow fill.
const GAP_DECAY_AFTER: Duration = Duration::from_secs(10);
/// After a gap decay, don't ask for more for this long: the guest just proved it can't give
/// more, and immediately re-commanding the same unfillable target re-enters the retry loop.
const GAP_BACKOFF: Duration = Duration::from_secs(60);
/// An inflation step judges as INELASTIC when the guest's free list surrendered less than half
/// of what the balloon absorbed: the difference was backfilled by reclaim, i.e. the step came
/// out of page cache, not free memory. (kswapd holds MemFree at its watermark equilibrium —
/// ~450–550 MiB on a 12 GiB guest, above every mode margin — so the MemFree clamp alone never
/// binds on a cache-warm guest: the 08-11 `out-clampgrade` testbed run.)
const ELASTIC_MIN_FILL_PAGES: u32 = DEAD_BAND_PAGES;
/// An inelastic hold at host-Normal releases only when the guest's free list rises this far
/// (KiB; 128 MiB) above the LOWEST level observed while held: real frees show up as a rise,
/// while the watermark equilibrium just wobbles ±tens of MiB. Measured from the observed floor,
/// not the verdict-time level — a verdict struck on a falling free list would otherwise leave a
/// release bar nothing can ever reach again (out-clampgrade2: baseline 8 GiB mid-mix, the mix
/// then legitimately converted that free to cache, hold permanent). No timer — if free never
/// rises off its floor, there is nothing new to take and probing again would only eat cache.
const INELASTIC_FREE_RISE_KIB: u64 = 128 * 1024;
/// Arm an elasticity probe only when the step departs from the low-free chase regime: free
/// within the mode margin plus this many pages (2 full steps). A verdict is only meaningful
/// near the reclaim equilibrium — at high free, concurrent guest churn (writeback, allocation,
/// process exits) dominates the free delta and produces false verdicts (out-clampgrade2
/// latched a permanent hold at free≈8 GiB during the dd). High-free steps need no probe:
/// they are self-evidently drawing a deep free list, the regime the clamp already handles.
const ELASTIC_PROBE_REGIME_PAGES: u32 = 2 * INFLATE_STEP_PAGES;
/// Guest io-PSI `full` avg10 (hundredths of a %) at/above which a held balloon is judged to be
/// starving the guest's page cache: every re-read misses and stalls on disk while memory-PSI
/// stays quiet (the 2026-07-09 sticky-wedge signature read 44% here with every memory threshold
/// "fine"). One arm of the pressure give-back's trigger disjunction (the other is sustained
/// memory-some — see the gate in [`decide`]) — both act well before the catastrophic
/// [`guest_starved`] release.
const IO_PRESSURE_HIGH: u32 = 1000; // 10.00%
/// io-PSI `full` avg10 above which a cache dig in force (an inelastic verdict at host
/// Warn/Critical) is paced down to the trickle: dropping cold cache is free, so full speed is
/// fine while io-PSI stays quiet — refaults pushing io-full past this mean the squeeze is
/// taking pages the guest still reads.
const IO_PRESSURE_LOW: u32 = 200; // 2.00%
/// How long a LOWER blended host level must sustain before the policy acts on it (bench
/// lever 7). The sysctl blend flaps at the 40% availability boundary, and one stray Normal
/// sample dumps Light's entire ramp — a rebuild the 256 MiB dwell-paced steps take minutes
/// to complete. Worsening levels act immediately (host distress must never wait on a
/// debounce); improving levels must prove themselves for this long.
const HOST_DEMOTE_SUSTAIN: Duration = Duration::from_secs(60);
/// Give-back escalation cap: the step doubles per consecutive sent give-back
/// ([`INFLATE_STEP_PAGES`] << streak) up to this shift — 256 MiB, 512 MiB, 1 GiB. One fixed
/// step walked a 4 GiB dig down over 16 dwells (~35 s of the guest still thrashing) on the S3
/// warn-dug point; escalation cuts that to ~6 while the first step — most give-back episodes
/// in full — stays the small sensor-paced one. The remaining tail is re-fault warm-up, which
/// no deflate pacing can remove.
const GIVEBACK_MAX_SHIFT: u32 = 2;

/// Minimum time between scrub cycles — and, because the timer starts armed at construction, the
/// minimum uptime before the first one (never scrub a freshly booted VM; this also keeps the
/// bench scenarios, which inject host Warn/Critical for minutes at a time, scrub-free).
/// This is the Moderate baseline; [`scrub_params`] keys the working value by mode.
const SCRUB_COOLDOWN: Duration = Duration::from_secs(30 * 60);
/// Don't scrub when the balloon can grow by less than this (pages; 512 MiB): a near-full balloon
/// means the freed pages already went through the release path — there is no pool to settle.
const SCRUB_MIN_INFLATE: u32 = 512 * PAGES_PER_MIB;
/// Stop waiting for the inflate after this long. `target = room` is usually unreachable (the
/// guest's live set bounds it), and a held gap is the driver's permanent 5 Hz retry loop — it
/// must be bounded, never left standing.
const SCRUB_INFLATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Consecutive stats reads with no inflate progress that end the inflate early (reports arrive
/// ~1 s apart, so ~10 s at a plateau) — no point burning the full timeout once the guest has
/// given all it will give.
const SCRUB_STALL_TICKS: u32 = 10;
/// How long the fully-inflated balloon is held before deflating, giving the host's pageout scan
/// a beat to finish settling the freshly-released ranges.
const SCRUB_HOLD: Duration = Duration::from_secs(15);
/// Stop waiting for the deflate to converge after this long.
const SCRUB_DEFLATE_TIMEOUT: Duration = Duration::from_secs(60);
/// The deflate counts as converged within this much of the resume target (bytes).
const SCRUB_DONE_SLACK: u64 = 64 << 20;
/// Hard deadline for a whole scrub, enforced by a detached watchdog thread: the tick machinery
/// runs on guest pressure reports, and the inflate itself can kill the reporter (an agent
/// thrashed out of its heartbeat) — a full balloon must never strand the guest at min RAM.
/// Comfortably above inflate-timeout + hold + deflate-timeout, so ticks always finish first.
const SCRUB_WATCHDOG: Duration = Duration::from_secs(300);

/// Cadence for the task-pmap ledger settle sweep (`settle` on the balloon socket): xnu bills
/// resident memory once per pmap, so the guest's disk-fed pages double-bill against the worker
/// and Activity Monitor shows up to 2× the VM's real memory (spikes/hv-ledger-marker). The
/// sweep debits the task share with a guest-invisible mprotect NONE→RW cycle over live guest
/// RAM; host re-touches re-bill their pages, so it runs on a cadence — this Moderate baseline,
/// keyed by mode in [`sweep_cooldown`] like the scrub. The timer starts armed at construction
/// (a fresh VM has nothing to settle), and every scrub completion also sweeps (the pool was
/// just settled by the release path; the sweep catches the live side at its low point).
const SWEEP_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// 4 KiB balloon pages per MiB.
pub const PAGES_PER_MIB: u32 = 256;

/// How hard the balloon claws guest memory back when the guest is idle. The spike
/// `spikes/mem-overhead-2026-07-02` (Run D) quantified the trade: a full squeeze costs the guest
/// its page cache — 4 KiB warm random reads go 852k IOPS/1 µs → 13.3k IOPS/75 µs (64×) — while
/// buying the host ~2–4 GB at idle. So everything but Aggressive keys the squeeze to *host*
/// memory pressure and leaves the guest a cache allowance when the host doesn't need the RAM.
/// Free-page reporting is unaffected by this knob: it returns only truly-free pages (no cache
/// cost) and always runs.
///
/// Inflation steps are paced by the guest's reported MemFree down to a mode-keyed margin
/// (`free_margin_pages`). The margin alone cannot preserve page cache — kswapd refills the
/// free list from cache above every margin, so a MemFree reading is a lie while cache is
/// being eaten (measured 2026-08-11, spikes/balloon-retention-testbed out-clampgrade run).
/// Cache preservation at host-Normal is instead the free-ELASTICITY gate: a sent step whose
/// pages the free list didn't surrender judges inelastic ([`elasticity_action`]) and holds
/// further inflation ([`Hold::Inelastic`]) until MemFree genuinely rises. When memory is owed
/// (host Warn/Critical, or Aggressive by contract) cache digging is intended and proceeds —
/// paced by the guest's io-PSI (full speed while the evicted cache is cold, the trickle once
/// refaults push io-full past [`IO_PRESSURE_LOW`]), and at the trickle once the free list is
/// exhausted. The margin clamp remains as death-spiral protection for the genuinely-
/// unreclaimable case. In the other direction, a guest left dug-down (say, by a past
/// host-pressure episode) that sustainedly hurts behind the balloon — io-full past
/// [`IO_PRESSURE_HIGH`], or the NotCalm memory band held on avg60 — gets memory back one
/// step per dwell (the pressure give-back) at host Normal/Warn: the S3 sticky-wedge shape,
/// caught well before the [`guest_starved`] release.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ReclaimMode {
    /// Never drive the balloon (free-page reporting still returns freed guest memory).
    Disabled,
    /// Host-pressure-driven, generous cache: no inflation while the host is fine; under host
    /// warn leave the guest 25% of max as cache; under critical squeeze toward the minimal
    /// working-set floor (never to zero).
    Light,
    /// Host-pressure-driven (the default): while the host is fine leave the guest 12.5% of
    /// max (min 1 GiB) as cache; under host warn 6.25% (min 1 GiB); under critical squeeze
    /// toward the minimal working-set floor (never to zero).
    Moderate,
    /// Squeeze to the floor whenever the guest is idle, ignoring host pressure (the original
    /// M6 policy).
    Aggressive,
}

/// macOS memory-pressure level, as reported by `kern.memorystatus_vm_pressure_level`.
/// Ordered by severity (declaration order): `Normal < Warn < Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// The blended level at sample time. `on_pressure` further applies the downward
    /// debounce ([`HostDebounce`]) before anything acts on it.
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
    /// `LIMINA_BALLOON_SCRUB=0` kill-switch for the scrub cycle (field safety valve).
    scrub_enabled: bool,
    /// `LIMINA_LEDGER_SWEEP=0` kill-switch for the ledger settle sweep (field safety valve).
    sweep_enabled: bool,
    /// Shared with the scrub watchdog threads — the only other holders.
    state: Arc<Mutex<State>>,
}

/// One `stats` reply from the worker (all cumulative since boot, bytes unless noted).
/// `heals`/`released`/`remapped`/`strays` are the stage-2 release/heal counters
/// (see `hvf::ReleasedRamStats` in the libkrun fork).
struct WorkerStats {
    actual_bytes: u64,
    reclaimed_bytes: u64,
    heals: u64,
    released_bytes: u64,
    remapped_bytes: u64,
    stray_faults: u64,
    /// Ledger settle sweeps completed, and the last one's debit/duration (observability
    /// only — the policy sends `settle` blind and reads the effect here).
    sweeps: u64,
    sweep_debited_bytes: u64,
    sweep_ms: u64,
    sweep_faults: u64,
    /// The worker's task-wide compressor-billed bytes (observability: the idle scrub's
    /// settle effect shows here as a drop across the cycle).
    compressed_bytes: u64,
}

struct State {
    /// A kept-open connection to the balloon socket (reconnected on error).
    conn: Option<UnixStream>,
    /// The balloon size we've currently commanded (4 KiB pages).
    target_pages: u32,
    /// When we last changed the target (for the inflation dwell).
    last_change: Option<Instant>,
    /// No inflation before this instant (armed on every high-pressure report, every sent
    /// give-back, and gap decay).
    cooldown_until: Option<Instant>,
    /// No scrub before this instant — armed ONLY by acute guest distress (memory-PSI high or
    /// starvation), never by a give-back. The scrub gate must not share the inflation
    /// cooldown: a guest whose own workload IO fires give-backs every few minutes would
    /// starve the scrub exactly while host pressure grows the retention pool (the 08-12
    /// dogfood episode — 46 min between scrubs against the 30-min Moderate cadence).
    distress_until: Option<Instant>,
    /// In-flight scrub cycle, if any. While active it owns the balloon target; the normal
    /// [`decide`] path is bypassed.
    scrub: Option<Scrub>,
    /// Bumped per scrub start; see [`Scrub::gen`].
    scrub_gen: u64,
    /// End of the last scrub cycle. Initialized to construction time so a fresh VM never
    /// scrubs before [`SCRUB_COOLDOWN`] of uptime.
    last_scrub_end: Instant,
    /// When the last ledger settle sweep was commanded (see [`SWEEP_COOLDOWN`]). Initialized
    /// to construction time so a fresh VM never sweeps before a cooldown of uptime.
    last_sweep: Instant,
    /// Tracking for a commanded-but-unfilled target gap (see [`gap_action`]). Only updated on
    /// ticks with a known `actual` — a failed stats query freezes it rather than resetting or
    /// advancing it.
    gap: Option<GapTrack>,
    /// Armed after each sent inflation step: judged by [`elasticity_action`] once the fill
    /// progresses. Cleared by any deflate, decay, or scrub (a stale probe must never judge).
    elastic_probe: Option<ElasticityProbe>,
    /// An in-force inelastic verdict: inflation came from reclaim, not free memory. Gates
    /// inflation at host-Normal (see [`Hold::Inelastic`]) until the guest's free list rises
    /// [`INELASTIC_FREE_RISE_KIB`] above the level recorded here.
    inelastic: Option<InelasticHold>,
    /// Consecutive sent give-backs in the current episode: doubles the give-back step (see
    /// [`GIVEBACK_MAX_SHIFT`]). Advanced by [`giveback_streak_next`]; cleared by a scrub start.
    giveback_streak: u32,
    /// Downward hysteresis over the blended host level (see [`HOST_DEMOTE_SUSTAIN`]).
    host_debounce: HostDebounce,
    /// `LIMINA_BALLOON_TRACE` decision journal: one JSON line per consumed report.
    trace: Option<std::fs::File>,
}

/// One armed elasticity observation: the free level and driver fill at the moment an inflation
/// step was sent, so the next tick can ask "did MemFree actually surrender those pages?".
struct ElasticityProbe {
    free_kib_at_send: u64,
    actual_at_send: u32,
}

/// Downward hysteresis over the blended host level (see [`HOST_DEMOTE_SUSTAIN`]): the level
/// the policy acts on rises instantly but only falls after the lower reading sustains.
struct HostDebounce {
    level: HostPressure,
    /// Since when raw has read below `level` (`None` = raw at/above it).
    below_since: Option<Instant>,
}

impl HostDebounce {
    fn new() -> Self {
        Self {
            level: HostPressure::Normal,
            below_since: None,
        }
    }

    /// Feed one raw blended sample; returns the level the policy acts on. A blip back to (or
    /// past) the held level cancels any pending demotion; a demotion that fires lands on the
    /// raw level read at fire time.
    fn observe(&mut self, raw: HostPressure, now: Instant) -> HostPressure {
        if raw >= self.level {
            self.level = raw;
            self.below_since = None;
        } else {
            match self.below_since {
                None => self.below_since = Some(now),
                Some(t) if now.duration_since(t) >= HOST_DEMOTE_SUSTAIN => {
                    self.level = raw;
                    self.below_since = None;
                }
                Some(_) => {}
            }
        }
        self.level
    }
}

/// The release baseline for an in-force inelastic verdict: the lowest free level observed
/// while held. See [`InelasticHold::observe`].
struct InelasticHold {
    free_kib_floor: u64,
}

impl InelasticHold {
    /// Feed one tick's free level: the baseline decays to the lowest level seen, and the hold
    /// releases (returns `true`) once free rises [`INELASTIC_FREE_RISE_KIB`] above that floor —
    /// a genuine free (a process exit, a dropped mapping), not the watermark wobble.
    fn observe(&mut self, free_kib: u64) -> bool {
        self.free_kib_floor = self.free_kib_floor.min(free_kib);
        free_kib >= self.free_kib_floor + INELASTIC_FREE_RISE_KIB
    }
}

/// The pure per-tick verdict on an armed [`ElasticityProbe`] (see [`elasticity_action`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Elasticity {
    /// The fill hasn't progressed enough to judge yet: keep the probe armed.
    Judging,
    /// Free tracked the fill: the balloon consumed genuinely free pages. Probe done.
    Elastic,
    /// The fill grew but free didn't drop to match: reclaim backfilled the free list from
    /// page cache — the step was a cache dig.
    Inelastic,
    /// The balloon deflated under the probe: no verdict possible, discard it.
    Stale,
}

/// One armed observation of a target>actual gap: when it started, and the fill level at arm
/// time so progress (a rising `actual`) re-arms instead of firing.
struct GapTrack {
    since: Instant,
    actual_at_arm: u32,
}

/// The pure per-tick verdict on a commanded-but-unfilled gap (see [`gap_action`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GapAction {
    /// Gap below the dead band (or actual caught up): clear any tracking.
    Clear,
    /// Gap present, no tracking yet (or fill progressed): (re)arm the timer at `now`.
    Arm,
    /// Gap present, timer running, nothing to do yet.
    Stay,
    /// Gap stuck past [`GAP_DECAY_AFTER`] with no progress: decay the target to `actual`.
    Fire,
}

/// One scrub cycle in flight (see [`scrub_due`] and [`BalloonPolicy::scrub_tick`]).
struct Scrub {
    phase: ScrubPhase,
    phase_since: Instant,
    /// What this cycle inflates to (pages) — the mode's depth decided it at start time.
    target_pages: u32,
    /// The pre-scrub target the deflate returns to (pages).
    resume_pages: u32,
    /// Monotonic id so the watchdog thread can tell whether the scrub it armed for is still the
    /// one in flight: a finished or *aborted* scrub must never be "restored" to `resume_pages` —
    /// after an abort the guest just proved it needs its memory.
    gen: u64,
    /// Progress tracking for the inflate stall detector.
    last_actual_bytes: u64,
    stall_ticks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrubPhase {
    Inflating,
    Holding,
    Deflating,
}

/// Verdict of one scrub tick (pure; see [`scrub_step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrubStep {
    Stay,
    ToHolding,
    ToDeflating,
    Done,
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
            scrub_enabled: std::env::var("LIMINA_BALLOON_SCRUB")
                .ok()
                .is_none_or(|v| v.trim() != "0"),
            sweep_enabled: std::env::var("LIMINA_LEDGER_SWEEP")
                .ok()
                .is_none_or(|v| v.trim() != "0"),
            state: Arc::new(Mutex::new(State {
                conn: None,
                target_pages: 0,
                last_change: None,
                cooldown_until: None,
                distress_until: None,
                scrub: None,
                scrub_gen: 0,
                last_scrub_end: Instant::now(),
                last_sweep: Instant::now(),
                gap: None,
                elastic_probe: None,
                inelastic: None,
                host_debounce: HostDebounce::new(),
                giveback_streak: 0,
                trace,
            })),
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
        let mut host = sample_host_pressure();
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        // Downward-debounce the blended level BEFORE anything acts on it: one stray Normal
        // sample must not dump a ramp (bench lever 7), while a worsening host acts this tick.
        // Everything downstream — the allowance ladder, the scrub trigger, the give-back's
        // Critical exclusion, the trace — sees one consistent notion of host level.
        host.blended = st.host_debounce.observe(host.blended, now);
        // A high-pressure or starvation report arms the re-inflation cooldown whether or not
        // there is a balloon to release: the guest just proved it needs its memory.
        let acute = p.some_avg10 >= PRESSURE_HIGH || guest_starved(p);
        if acute {
            st.cooldown_until = Some(now + RELEASE_COOLDOWN);
            st.distress_until = Some(now + RELEASE_COOLDOWN);
            // An in-flight scrub is abandoned, not paused: falling through lets the normal
            // release decision hand the memory back this same tick.
            abort_scrub(&mut st, now);
        }
        if st.scrub.is_some() {
            self.scrub_tick(&mut st, now);
            return;
        }
        // Worker stats now feed the policy, not just the journal: `actual` is the pacing
        // clamp's base, the gap tracker's progress signal, and the scrub due-gate's fill
        // measure. A failed query degrades softly — everything bases off `current` and gap
        // tracking freezes — never a stalled policy.
        let wstats = self.query_stats(&mut st);
        let actual_pages = wstats
            .as_ref()
            .map(|w| (w.actual_bytes >> 12).min(u32::MAX as u64) as u32);
        if self.scrub_enabled {
            let params = scrub_params(self.mode);
            let fill = actual_pages.unwrap_or(st.target_pages);
            let inflate_to = scrub_target_pages(
                params.depth,
                room,
                fill,
                p.mem_free_kib,
                free_margin_pages(self.mode),
            );
            if scrub_due(
                &params,
                host.blended,
                acute,
                inflate_to,
                fill,
                st.distress_until,
                st.last_scrub_end,
                now,
            ) {
                self.start_scrub(&mut st, now, inflate_to);
                return;
            }
            // Quiet-day settle: same cycle, Bounded depth regardless of mode (never dump an
            // idle guest's cache), on the long idle cadence. Covers the pressure trigger's
            // blind spot — a calm host otherwise never settles the dead share.
            let idle_to = scrub_target_pages(
                ScrubDepth::Bounded,
                room,
                fill,
                p.mem_free_kib,
                free_margin_pages(self.mode),
            );
            if idle_scrub_due(
                &params,
                host.blended,
                acute,
                p.some_avg10,
                p.mem_free_kib,
                idle_to,
                fill,
                st.distress_until,
                st.last_scrub_end,
                now,
            ) {
                log::info!(
                    "autoballoon: idle scrub starting (quiet-day settle, compressed={} MiB)",
                    wstats.as_ref().map_or(0, |w| w.compressed_bytes >> 20)
                );
                self.start_scrub(&mut st, now, idle_to);
                return;
            }
        }
        // Ledger settle sweep, on cadence. Guest-invisible and cheap for the worker (tens of
        // ms, on its own thread), so it doesn't consume the tick or care about pressure —
        // its only job is keeping the task ledger (Activity Monitor, jetsam) honest.
        if self.sweep_enabled
            && sweep_due(self.mode, st.last_sweep, now)
            && send_settle(&self.socket, &mut st)
        {
            st.last_sweep = now;
            log::info!("autoballoon: ledger settle sweep sent (cadence)");
        }
        // Free-elasticity: judge the last sent inflation step. A step whose pages the free
        // list didn't surrender was backfilled by reclaim — it came out of page cache, and the
        // MemFree clamp alone can't see that (kswapd holds MemFree at its watermark equilibrium,
        // above every mode margin — the 08-11 `out-clampgrade` run). The verdict holds inflation
        // at host-Normal until the guest's free list genuinely rises.
        if p.mem_free_kib != 0 {
            if let (Some(probe), Some(a)) = (st.elastic_probe.as_ref(), actual_pages) {
                match elasticity_action(probe, p.mem_free_kib, a) {
                    Elasticity::Judging => {}
                    Elasticity::Elastic | Elasticity::Stale => st.elastic_probe = None,
                    Elasticity::Inelastic => {
                        st.elastic_probe = None;
                        st.inelastic = Some(InelasticHold {
                            free_kib_floor: p.mem_free_kib,
                        });
                        log::info!(
                            "autoballoon: inflation judged inelastic (free did not track the \
                             fill — reclaim is feeding the balloon from cache); holding at \
                             host-Normal until MemFree rises"
                        );
                    }
                }
            }
            if let Some(h) = st.inelastic.as_mut() {
                if h.observe(p.mem_free_kib) {
                    st.inelastic = None; // the guest freed real memory: probe again
                }
            }
        }
        let inputs = DecideInputs {
            mode: self.mode,
            host: host.blended,
            current: st.target_pages,
            actual_pages,
            room,
            max_pages: self.max_pages,
            last_change: st.last_change,
            cooldown_until: st.cooldown_until,
            inelastic: st.inelastic.is_some(),
            giveback_streak: st.giveback_streak,
            now,
        };
        let mut decision = decide(p, &inputs);
        let mut sent = false;
        if let Decision::Set(new_target) | Decision::GiveBack(new_target) = decision {
            let old_target = st.target_pages;
            sent = send_target(&self.socket, &mut st, new_target);
            if sent {
                st.target_pages = new_target;
                st.last_change = Some(now);
                if matches!(decision, Decision::GiveBack(_)) {
                    // Like any release: the guest just proved (through io) that it needs its
                    // memory — don't take the give-back right back the moment io settles.
                    // Arms only the INFLATION cooldown, never `distress_until`: a give-back
                    // must not push the scrub cadence out (08-12 limit cycle, half B).
                    let until = now + RELEASE_COOLDOWN;
                    st.cooldown_until = Some(st.cooldown_until.map_or(until, |t| t.max(until)));
                    log::info!(
                        "autoballoon: pressure give-back — guest sustainedly hurting behind \
                         the balloon (io_full_avg10={}, some_avg60={}), target -> {new_target} \
                         pages",
                        p.io_full_avg10,
                        p.some_avg60
                    );
                }
                // Arm the elasticity probe on inflations sent from the low-free chase regime
                // (see ELASTIC_PROBE_REGIME_PAGES); a deflate invalidates any armed one.
                let regime_kib =
                    (free_margin_pages(self.mode) as u64 + ELASTIC_PROBE_REGIME_PAGES as u64) * 4;
                st.elastic_probe = if new_target > old_target
                    && p.mem_free_kib != 0
                    && p.mem_free_kib < regime_kib
                {
                    Some(ElasticityProbe {
                        free_kib_at_send: p.mem_free_kib,
                        actual_at_send: actual_pages.unwrap_or(old_target),
                    })
                } else {
                    None
                };
                log::debug!(
                    "autoballoon: target -> {new_target} pages (some_avg10={}, avail/total={}/{})",
                    p.some_avg10,
                    p.mem_available_kib,
                    p.mem_total_kib
                );
            }
        }
        // Gap decay: a held target the driver can't fill is its permanent 5 Hz retry loop
        // ("Out of puff" spam) — after GAP_DECAY_AFTER of no fill progress, trim the target to
        // what the driver actually reached and back off. Skipped entirely when `actual` is
        // unknown (frozen, not reset — see `State::gap`).
        if let Some(a) = actual_pages {
            match gap_action(st.gap.as_ref(), st.target_pages, a, sent, now) {
                GapAction::Clear => st.gap = None,
                GapAction::Arm => {
                    st.gap = Some(GapTrack {
                        since: now,
                        actual_at_arm: a,
                    })
                }
                GapAction::Stay => {}
                GapAction::Fire => {
                    if send_target(&self.socket, &mut st, a) {
                        let gap_mib = (st.target_pages.saturating_sub(a)) / PAGES_PER_MIB;
                        st.target_pages = a;
                        st.last_change = Some(now);
                        st.gap = None;
                        st.elastic_probe = None; // the decay is a deflate: any probe is stale
                        let backoff = now + GAP_BACKOFF;
                        st.cooldown_until =
                            Some(st.cooldown_until.map_or(backoff, |t| t.max(backoff)));
                        decision = Decision::Decay(a);
                        sent = true;
                        log::warn!(
                            "autoballoon: gap decay — target trimmed to actual ({a} pages, \
                             {gap_mib} MiB unfillable), inflation backed off {GAP_BACKOFF:?}"
                        );
                    }
                }
            }
        }
        st.giveback_streak = giveback_streak_next(st.giveback_streak, decision, sent);
        trace_decision(&mut st, p, &host, &inputs, decision, sent, wstats.as_ref());
    }

    /// Begin a scrub cycle: inflate to `inflate_to` (the mode's [`ScrubDepth`] decided how deep
    /// — [`scrub_target_pages`]), hold, deflate back to the pre-scrub target. Driven forward by
    /// [`Self::scrub_tick`] on subsequent reports; the watchdog thread is the only other exit.
    fn start_scrub(&self, st: &mut State, now: Instant, inflate_to: u32) {
        let resume_pages = st.target_pages;
        if !send_target(&self.socket, st, inflate_to) {
            return; // still due — retried on the next report
        }
        st.target_pages = inflate_to;
        st.last_change = Some(now);
        st.gap = None; // the scrub owns the target now; stale gap timing must not survive it
        st.elastic_probe = None; // scrub churn must not feed an elasticity verdict
        st.giveback_streak = 0; // same: an episode must not straddle a scrub cycle
        st.scrub_gen += 1;
        let gen = st.scrub_gen;
        st.scrub = Some(Scrub {
            phase: ScrubPhase::Inflating,
            phase_since: now,
            target_pages: inflate_to,
            resume_pages,
            gen,
            last_actual_bytes: 0,
            stall_ticks: 0,
        });
        trace_scrub(st, "start", gen, resume_pages, None, None);
        log::warn!(
            "autoballoon: scrub start (inflate to {inflate_to} pages, resume {resume_pages})"
        );
        self.spawn_scrub_watchdog(gen, resume_pages);
    }

    /// Advance an in-flight scrub one report-tick. Every phase advances on timeout alone —
    /// worker stats are the fast path, never load-bearing (a failed stats query must not stall
    /// the cycle with the balloon fully inflated).
    fn scrub_tick(&self, st: &mut State, now: Instant) {
        let actual = self.query_stats(st).map(|w| w.actual_bytes);
        let Some(scrub) = st.scrub.as_mut() else {
            return;
        };
        if scrub.phase == ScrubPhase::Inflating {
            match actual {
                Some(a) if a > scrub.last_actual_bytes => {
                    scrub.last_actual_bytes = a;
                    scrub.stall_ticks = 0;
                }
                Some(_) => scrub.stall_ticks += 1,
                None => {}
            }
        }
        let (phase, phase_since, gen, resume_pages, stall_ticks) = (
            scrub.phase,
            scrub.phase_since,
            scrub.gen,
            scrub.resume_pages,
            scrub.stall_ticks,
        );
        let target_bytes = (scrub.target_pages as u64) << 12;
        let step = scrub_step(
            phase,
            now.duration_since(phase_since),
            actual,
            target_bytes,
            (resume_pages as u64) << 12,
            stall_ticks,
        );
        match step {
            ScrubStep::Stay => {}
            ScrubStep::ToHolding => {
                if let Some(s) = st.scrub.as_mut() {
                    s.phase = ScrubPhase::Holding;
                    s.phase_since = now;
                }
                // The reached fraction distinguishes "completed" from "timed out at 60%" for a
                // field debugger without timestamp correlation.
                let pct =
                    actual.map(|a| a.min(target_bytes).saturating_mul(100) / target_bytes.max(1));
                trace_scrub(st, "hold", gen, resume_pages, actual, pct);
            }
            ScrubStep::ToDeflating => {
                // Only advance once the deflate command actually went out; a failed send leaves
                // the phase at Holding, whose elapsed timer retries this transition every tick
                // (and the watchdog remains the backstop).
                if send_target(&self.socket, st, resume_pages) {
                    st.target_pages = resume_pages;
                    st.last_change = Some(now);
                    if let Some(s) = st.scrub.as_mut() {
                        s.phase = ScrubPhase::Deflating;
                        s.phase_since = now;
                    }
                    trace_scrub(st, "deflate", gen, resume_pages, actual, None);
                }
            }
            ScrubStep::Done => {
                st.scrub = None;
                st.last_scrub_end = now;
                trace_scrub(st, "done", gen, resume_pages, actual, None);
                log::warn!("autoballoon: scrub done (resumed {resume_pages} pages)");
                // The release path just settled the pool's shares; sweep now to catch the
                // live side at its low point (and reset the cadence — it would be due soon
                // anyway, the scrub cooldown being the longer of the two).
                if self.sweep_enabled && send_settle(&self.socket, st) {
                    st.last_sweep = now;
                    log::info!("autoballoon: ledger settle sweep sent (post-scrub)");
                }
            }
        }
    }

    /// The scrub's independent exit: after [`SCRUB_WATCHDOG`], if *this* scrub (by generation)
    /// is somehow still in flight — the report stream died mid-scrub, so ticks stopped — deflate
    /// back unconditionally. A completed or aborted scrub bumped past the generation and makes
    /// this a no-op, so an abort's release is never undone by a stale re-inflate.
    fn spawn_scrub_watchdog(&self, gen: u64, resume_pages: u32) {
        let state = Arc::clone(&self.state);
        let socket = self.socket.clone();
        std::thread::spawn(move || {
            std::thread::sleep(SCRUB_WATCHDOG);
            let mut st = state.lock().unwrap();
            if st.scrub.as_ref().is_some_and(|s| s.gen == gen) {
                st.scrub = None;
                st.last_scrub_end = Instant::now();
                if send_target(&socket, &mut st, resume_pages) {
                    st.target_pages = resume_pages;
                }
                trace_scrub(&mut st, "watchdog", gen, resume_pages, None, None);
                log::warn!(
                    "autoballoon: scrub watchdog fired (pressure reports stalled mid-scrub) — \
                     deflated to {resume_pages} pages"
                );
            }
        });
    }

    /// One `stats` round-trip over the shared connection. Any failure — including a read
    /// timeout — drops the connection: a late reply left buffered would be read as the NEXT
    /// tick's answer, silently time-shifting the journal. Dropping instead keeps the
    /// invariant that an open connection never has a reply in flight.
    fn query_stats(&self, st: &mut State) -> Option<WorkerStats> {
        use std::io::BufRead;
        if st.conn.is_none() {
            st.conn = UnixStream::connect(&self.socket).ok();
        }
        let conn = st.conn.as_mut()?;
        if writeln!(conn, "stats").and_then(|()| conn.flush()).is_err() {
            st.conn = None;
            return None;
        }
        if conn
            .set_read_timeout(Some(Duration::from_millis(300)))
            .is_err()
        {
            st.conn = None;
            return None;
        }
        let mut line = String::new();
        let read = std::io::BufReader::new(&*conn).read_line(&mut line);
        if read.is_err() || !line.ends_with('\n') {
            st.conn = None;
            return None;
        }
        let mut s = WorkerStats {
            actual_bytes: 0,
            reclaimed_bytes: 0,
            heals: 0,
            released_bytes: 0,
            remapped_bytes: 0,
            stray_faults: 0,
            sweeps: 0,
            sweep_debited_bytes: 0,
            sweep_ms: 0,
            sweep_faults: 0,
            compressed_bytes: 0,
        };
        for tok in line.split_whitespace() {
            let Some((k, v)) = tok.split_once('=') else {
                continue;
            };
            let v: u64 = v.parse().unwrap_or(0);
            match k {
                "actual" => s.actual_bytes = v,
                "reclaimed" => s.reclaimed_bytes = v,
                "heals" => s.heals = v,
                "released" => s.released_bytes = v,
                "remapped" => s.remapped_bytes = v,
                "strays" => s.stray_faults = v,
                "sweeps" => s.sweeps = v,
                "sweep_debited" => s.sweep_debited_bytes = v,
                "sweep_ms" => s.sweep_ms = v,
                "sweep_faults" => s.sweep_faults = v,
                "compressed" => s.compressed_bytes = v,
                _ => {}
            }
        }
        Some(s)
    }
}

/// Write `target <bytes>` to the balloon socket, reconnecting once on failure. Returns whether
/// the command went out. A free function (not a method) so the scrub watchdog thread, which only
/// holds the state `Arc` and the socket path, can call it too.
fn send_target(socket: &Path, st: &mut State, pages: u32) -> bool {
    let bytes = (pages as u64) << 12;
    for attempt in 0..2 {
        if st.conn.is_none() {
            match UnixStream::connect(socket) {
                Ok(c) => st.conn = Some(c),
                Err(e) => {
                    if attempt == 1 {
                        log::warn!("autoballoon: connect {socket:?}: {e}");
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

/// Write `settle` (run a ledger settle sweep; no reply) to the balloon socket, reconnecting
/// once on failure. Returns whether the command went out. Same shape as [`send_target`].
fn send_settle(socket: &Path, st: &mut State) -> bool {
    for attempt in 0..2 {
        if st.conn.is_none() {
            match UnixStream::connect(socket) {
                Ok(c) => st.conn = Some(c),
                Err(e) => {
                    if attempt == 1 {
                        log::warn!("autoballoon: connect {socket:?}: {e}");
                    }
                    continue;
                }
            }
        }
        let conn = st.conn.as_mut().unwrap();
        if writeln!(conn, "settle").and_then(|()| conn.flush()).is_ok() {
            return true;
        }
        st.conn = None; // broken pipe — drop and retry once
    }
    false
}

/// The mode-keyed settle-sweep cadence (see [`SWEEP_COOLDOWN`]): the guest never notices a
/// sweep, but host re-touches between sweeps re-bill, so eager modes sweep more often.
fn sweep_cooldown(mode: ReclaimMode) -> Duration {
    match mode {
        ReclaimMode::Disabled | ReclaimMode::Light => 2 * SWEEP_COOLDOWN,
        ReclaimMode::Moderate => SWEEP_COOLDOWN,
        ReclaimMode::Aggressive => SWEEP_COOLDOWN / 2,
    }
}

/// Whether a cadence sweep is due (pure).
fn sweep_due(mode: ReclaimMode, last_sweep: Instant, now: Instant) -> bool {
    now.duration_since(last_sweep) >= sweep_cooldown(mode)
}

/// How deep a scrub inflates (see [`scrub_target_pages`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrubDepth {
    /// Inflate only as far as the guest's free list covers: captures the DEAD share of the
    /// retention pool (freed-but-unreported pages sitting in the guest's free lists — MemFree
    /// includes them) without forcing page-cache reclaim.
    Bounded,
    /// Eager full inflate to the whole room: also reaches the cold-cache share, at the cost of
    /// dumping the guest's page cache (the testbed-measured 66%-of-pool recovery).
    Full,
}

/// Per-mode scrub tuning: how bad the host must be doing before a cycle may run, how often,
/// and how deep. Keyed by [`ReclaimMode`] as the guest-comfort knob, like the allowances:
/// Light guests give up their cache only when the host is in real trouble; Aggressive guests
/// scrub eagerly and often.
struct ScrubParams {
    /// Minimum blended host level that can trigger a cycle.
    min_host: HostPressure,
    /// Minimum time between cycles (and, because the timer starts armed at construction, the
    /// minimum uptime before the first).
    cooldown: Duration,
    depth: ScrubDepth,
}

fn scrub_params(mode: ReclaimMode) -> ScrubParams {
    match mode {
        // Not reached: `on_pressure` returns early for Disabled. Values are the gentlest.
        ReclaimMode::Disabled | ReclaimMode::Light => ScrubParams {
            min_host: HostPressure::Critical,
            cooldown: 2 * SCRUB_COOLDOWN,
            depth: ScrubDepth::Bounded,
        },
        ReclaimMode::Moderate => ScrubParams {
            min_host: HostPressure::Warn,
            cooldown: SCRUB_COOLDOWN,
            depth: ScrubDepth::Bounded,
        },
        ReclaimMode::Aggressive => ScrubParams {
            min_host: HostPressure::Warn,
            cooldown: SCRUB_COOLDOWN / 2,
            depth: ScrubDepth::Full,
        },
    }
}

/// What a scrub starting now would inflate to (pages, pure). `Bounded` degrades to the full
/// room when the agent doesn't report MemFree (`mem_free_kib == 0` = absent, per the proto
/// contract) — an old agent gets the pre-tuning eager behavior, not a broken no-op scrub.
fn scrub_target_pages(
    depth: ScrubDepth,
    room: u32,
    fill: u32,
    mem_free_kib: u64,
    margin_pages: u32,
) -> u32 {
    match depth {
        ScrubDepth::Full => room,
        ScrubDepth::Bounded if mem_free_kib == 0 => room,
        ScrubDepth::Bounded => {
            let free_pages = (mem_free_kib / 4).min(u32::MAX as u64) as u32;
            fill.saturating_add(free_pages.saturating_sub(margin_pages))
                .min(room)
        }
    }
}

/// The idle scrub's cadence, as a multiple of the mode's pressure-scrub cooldown (Light 3 h /
/// Moderate 90 min / Aggressive 45 min). Idle scrubs exist because the pressure trigger has a
/// blind spot the 08-13 dogfood day exposed: a calm host never fires a scrub, so a quiet VM
/// sits on its dead share (guest-freed content the compressor keeps billing) indefinitely —
/// that day it took the research session's own memory demand to squeeze it out by accident.
const IDLE_SCRUB_COOLDOWN_MULT: u32 = 3;

/// Whether a quiet-day settle scrub may start (pure; unit-tested). The mirror image of
/// [`scrub_due`]'s trigger: host fully calm and the guest itself idle, on a much longer
/// cadence, and always at Bounded depth — an idle scrub only captures the guest's free list
/// (the dead share); it must never dump a healthy guest's page cache just because the day is
/// quiet. Requires a MemFree report (`mem_free_kib != 0`): without one, Bounded degrades to a
/// full-room inflate, which is exactly the cache dump idle mode forbids — an old agent gets
/// no idle scrubs (degraded, not broken). The `inflate_to - fill` gate doubles as the
/// evidence-of-residue test: Bounded targets past the fill only when the guest holds
/// substantial freed-but-uncaptured content.
#[allow(clippy::too_many_arguments)]
fn idle_scrub_due(
    params: &ScrubParams,
    host: HostPressure,
    acute: bool,
    some_avg10: u32,
    mem_free_kib: u64,
    inflate_to: u32,
    fill: u32,
    distress_until: Option<Instant>,
    last_scrub_end: Instant,
    now: Instant,
) -> bool {
    host == HostPressure::Normal
        && !acute
        && some_avg10 <= PRESSURE_LOW
        && mem_free_kib != 0
        && inflate_to.saturating_sub(fill) >= SCRUB_MIN_INFLATE
        && distress_until.is_none_or(|t| now >= t)
        && now.duration_since(last_scrub_end) >= params.cooldown * IDLE_SCRUB_COOLDOWN_MULT
}

/// Whether a pressure-triggered scrub may start (pure; unit-tested). The scrub is the only
/// measured lever that shrinks the retention pool's phys_footprint, but inflating also costs
/// the guest (cache, for a Full scrub) — so it runs only when the HOST is at/above the mode's
/// trigger level, from a guest that isn't in acute pressure or starving, outside both the
/// post-distress holdoff and the mode's scrub cadence, and only when the cycle would actually
/// inflate meaningfully past the driver's current fill (`inflate_to` is the depth-computed
/// target: a cache-full guest under a Bounded scrub has huge room but nothing free to capture
/// — not due). `distress_until` is the ACUTE holdoff only — deliberately not the inflation
/// cooldown, which give-backs re-arm every few minutes under a busy guest's own IO and would
/// starve the scrub exactly while the pool grows (the 08-12 dogfood episode).
#[allow(clippy::too_many_arguments)]
fn scrub_due(
    params: &ScrubParams,
    host: HostPressure,
    acute: bool,
    inflate_to: u32,
    fill: u32,
    distress_until: Option<Instant>,
    last_scrub_end: Instant,
    now: Instant,
) -> bool {
    host >= params.min_host
        && !acute
        && inflate_to.saturating_sub(fill) >= SCRUB_MIN_INFLATE
        && distress_until.is_none_or(|t| now >= t)
        && now.duration_since(last_scrub_end) >= params.cooldown
}

/// The pure per-tick phase-advance verdict for an in-flight scrub. Stats (`actual_bytes`) are
/// the fast path; every phase also advances on elapsed time alone, so a dead stats channel can
/// slow a scrub but never stall it inflated.
fn scrub_step(
    phase: ScrubPhase,
    in_phase: Duration,
    actual_bytes: Option<u64>,
    target_bytes: u64,
    resume_bytes: u64,
    stall_ticks: u32,
) -> ScrubStep {
    match phase {
        ScrubPhase::Inflating => {
            let reached = actual_bytes.is_some_and(|a| a >= target_bytes / 10 * 9);
            if reached || stall_ticks >= SCRUB_STALL_TICKS || in_phase >= SCRUB_INFLATE_TIMEOUT {
                ScrubStep::ToHolding
            } else {
                ScrubStep::Stay
            }
        }
        ScrubPhase::Holding => {
            if in_phase >= SCRUB_HOLD {
                ScrubStep::ToDeflating
            } else {
                ScrubStep::Stay
            }
        }
        ScrubPhase::Deflating => {
            let converged =
                actual_bytes.is_some_and(|a| a <= resume_bytes.saturating_add(SCRUB_DONE_SLACK));
            if converged || in_phase >= SCRUB_DEFLATE_TIMEOUT {
                ScrubStep::Done
            } else {
                ScrubStep::Stay
            }
        }
    }
}

/// The pure per-tick gap verdict (unit-tested): given the tracked gap state, the commanded
/// target, the driver's reported fill, and whether this tick already sent a target change,
/// decide what happens to the gap tracker. Progress (a rising `actual`) re-arms rather than
/// fires — a slow fill under the trickle clamp is legitimate; only a STUCK gap decays. A tick
/// that just sent a target change never fires (the driver deserves a chance to chase the new
/// target), but the timer keeps running — a rising target over a stuck `actual` is still stuck.
fn gap_action(
    track: Option<&GapTrack>,
    target: u32,
    actual: u32,
    sent_this_tick: bool,
    now: Instant,
) -> GapAction {
    if target.saturating_sub(actual) < GAP_EPS_PAGES {
        return GapAction::Clear;
    }
    match track {
        None => GapAction::Arm,
        Some(g) if actual >= g.actual_at_arm.saturating_add(GAP_EPS_PAGES) => GapAction::Arm,
        Some(g) if !sent_this_tick && now.duration_since(g.since) >= GAP_DECAY_AFTER => {
            GapAction::Fire
        }
        Some(_) => GapAction::Stay,
    }
}

/// The pure elasticity verdict (unit-tested): given the probe armed at the last sent inflation
/// step, the guest's current MemFree and the driver's current fill, decide whether that step
/// consumed free memory or was backfilled by reclaim. Free is compared page-for-page against
/// what the balloon absorbed: a drop under half the absorbed amount means the free list was
/// replenished from page cache while the balloon drained it.
fn elasticity_action(probe: &ElasticityProbe, free_kib: u64, actual: u32) -> Elasticity {
    if actual < probe.actual_at_send {
        return Elasticity::Stale; // deflated under the probe: no verdict possible
    }
    let absorbed = actual - probe.actual_at_send;
    if absorbed < ELASTIC_MIN_FILL_PAGES {
        return Elasticity::Judging;
    }
    let free_drop_pages =
        (probe.free_kib_at_send.saturating_sub(free_kib) / 4).min(u32::MAX as u64) as u32;
    if free_drop_pages < absorbed / 2 {
        Elasticity::Inelastic
    } else {
        Elasticity::Elastic
    }
}

/// Abandon an in-flight scrub (acute guest pressure or starvation). The caller's release
/// decision hands the memory back; re-arming `last_scrub_end` here keeps an abort-prone guest
/// from being re-squeezed the moment the acute report passes.
fn abort_scrub(st: &mut State, now: Instant) {
    let Some(scrub) = st.scrub.take() else {
        return;
    };
    st.last_scrub_end = now;
    trace_scrub(st, "abort", scrub.gen, scrub.resume_pages, None, None);
    log::warn!("autoballoon: scrub aborted (guest under pressure)");
}

/// Append one scrub event to the decision journal. The distinct `"scrub"` key (and no
/// `"decision"` key) keeps these lines invisible to the bench summarizer, which filters on the
/// decision key — additive, not a contract change.
fn trace_scrub(
    st: &mut State,
    event: &str,
    gen: u64,
    resume_pages: u32,
    actual_bytes: Option<u64>,
    reached_pct: Option<u64>,
) {
    let Some(f) = st.trace.as_mut() else {
        return;
    };
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let json_stat = |v: Option<u64>| v.map_or("null".to_string(), |v| v.to_string());
    let line = format!(
        "{{\"ts_ms\":{ts_ms},\"scrub\":\"{event}\",\"gen\":{gen},\"resume_pages\":{resume_pages},\
         \"actual_bytes\":{},\"reached_pct\":{}}}\n",
        json_stat(actual_bytes),
        json_stat(reached_pct),
    );
    if f.write_all(line.as_bytes()).is_err() {
        st.trace = None;
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
    wstats: Option<&WorkerStats>,
) {
    let Some(f) = st.trace.as_mut() else {
        return;
    };
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let json_opt = |v: Option<i32>| v.map_or("null".to_string(), |v| v.to_string());
    let json_stat = |v: Option<u64>| v.map_or("null".to_string(), |v| v.to_string());
    let new_target = decision
        .target()
        .map_or("null".to_string(), |t| t.to_string());
    let cooldown_active = i.cooldown_until.is_some_and(|t| i.now < t);
    let line = format!(
        concat!(
            "{{\"ts_ms\":{},\"mode\":\"{:?}\",",
            "\"some_avg10\":{},\"some_avg60\":{},\"full_avg10\":{},\"full_avg60\":{},",
            "\"avail_kib\":{},\"total_kib\":{},\"free_kib\":{},\"io_full_avg10\":{},",
            "\"host_raw_level\":{},\"host_avail_pct\":{},\"host\":\"{}\",\"host_injected\":{},",
            "\"current_pages\":{},\"decision\":\"{}\",\"new_target_pages\":{},",
            "\"cooldown_active\":{},\"sent\":{},",
            "\"actual_bytes\":{},\"reclaimed_bytes\":{},\"heals\":{},",
            "\"released_bytes\":{},\"remapped_bytes\":{},\"stray_faults\":{},",
            "\"sweeps\":{},\"sweep_debited_bytes\":{},\"sweep_ms\":{},",
            "\"sweep_faults\":{},\"compressed_bytes\":{}}}\n"
        ),
        ts_ms,
        i.mode,
        p.some_avg10,
        p.some_avg60,
        p.full_avg10,
        p.full_avg60,
        p.mem_available_kib,
        p.mem_total_kib,
        p.mem_free_kib,
        p.io_full_avg10,
        json_opt(host.raw_level),
        json_opt(host.available_percent),
        host.blended.label(),
        host.injected,
        i.current,
        decision.label(),
        new_target,
        cooldown_active,
        sent,
        json_stat(wstats.map(|w| w.actual_bytes)),
        json_stat(wstats.map(|w| w.reclaimed_bytes)),
        json_stat(wstats.map(|w| w.heals)),
        json_stat(wstats.map(|w| w.released_bytes)),
        json_stat(wstats.map(|w| w.remapped_bytes)),
        json_stat(wstats.map(|w| w.stray_faults)),
        json_stat(wstats.map(|w| w.sweeps)),
        json_stat(wstats.map(|w| w.sweep_debited_bytes)),
        json_stat(wstats.map(|w| w.sweep_ms)),
        json_stat(wstats.map(|w| w.sweep_faults)),
        json_stat(wstats.map(|w| w.compressed_bytes)),
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
    /// The driver's reported fill (pages), when the stats query succeeded. The pacing clamp's
    /// base: free-list headroom is measured from what the driver has actually taken, not from
    /// what we asked for. `None` falls back to `current`.
    actual_pages: Option<u32>,
    /// max − min: the most the balloon may hold (pages).
    room: u32,
    /// Total guest RAM libkrun allocated (pages) — the allowance percentages key off this.
    max_pages: u32,
    last_change: Option<Instant>,
    /// No inflation before this instant (armed by the caller on high-pressure reports).
    cooldown_until: Option<Instant>,
    /// An inelastic verdict is in force: the last judged inflation step was fed by reclaim
    /// (cache), not free memory (see [`elasticity_action`]). Gates inflation at host-Normal.
    inelastic: bool,
    /// Consecutive sent give-backs in the current episode (see [`GIVEBACK_MAX_SHIFT`]).
    giveback_streak: u32,
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

/// The free-list margin the pacing clamp preserves (pages): inflation may consume guest MemFree
/// down to this level without being considered "digging" — below it, allocation forces reclaim.
/// Keyed by mode as the guest-comfort knob: Light leaves the roomiest kernel working margin,
/// Aggressive shaves closest to the reclaim edge.
fn free_margin_pages(mode: ReclaimMode) -> u32 {
    match mode {
        ReclaimMode::Disabled => 0, // not reached: policy isn't constructed
        ReclaimMode::Light => 512 * PAGES_PER_MIB,
        ReclaimMode::Moderate => 256 * PAGES_PER_MIB,
        ReclaimMode::Aggressive => 128 * PAGES_PER_MIB,
    }
}

/// The smallest balloon the pressure give-back may blame (pages): max/16, never below one
/// inflation step. The give-back's triggers read the guest's PSI, which cannot tell
/// balloon-induced thrash from the guest's own workload IO — attribution comes from size.
/// Every measured incident it exists for held a multi-GB balloon (21 GiB in the 07-09
/// wedge, a 4 GiB dig on the S3 warn-dug point); a balloon under ~6% of the VM cannot
/// plausibly be what starves the guest's cache. Without this floor, a guest whose own
/// test suites held io-full over the bar knifed every first 256 MiB step for 2.3 h
/// (2026-08-12 dogfood): balloon pinned at ~0, every release lever starved, the
/// compressor retention pool grew unchecked under host Warn.
fn giveback_floor_pages(max_pages: u32) -> u32 {
    (max_pages / 16).max(INFLATE_STEP_PAGES)
}

/// A guest is *starved* when MemAvailable is critically low: catastrophic cache starvation
/// manifests as IO pressure (swap-in and refault storms), NOT as memory-PSI — a wedged guest
/// can sit at 263 MiB available / 2.3% memory-some / 44% io-full, stuck behind a
/// 21 GiB balloon the PSI gates never release. Available this low while we hold a balloon is
/// the balloon's fault by definition.
fn guest_starved(p: &MemPressure) -> bool {
    p.mem_total_kib > 0 && p.mem_available_kib < (256 * 1024).max(p.mem_total_kib / 64)
}

/// Advance the give-back escalation streak for one consumed report. A sent give-back extends
/// the episode; `Hold(Dwell)` preserves it (the only dwells that can coexist with a nonzero
/// streak are give-back-path dwells: every sent give-back arms [`RELEASE_COOLDOWN`], so the
/// inflation path answers `Cooldown`, not `Dwell`, for the next 300 s); anything else — an
/// allowance move, a decay, any other hold, or a give-back the socket failed to deliver —
/// ends the episode and the next one starts back at the small step.
fn giveback_streak_next(streak: u32, decision: Decision, sent: bool) -> u32 {
    match decision {
        Decision::GiveBack(_) if sent => streak.saturating_add(1),
        Decision::Hold(Hold::Dwell) => streak,
        _ => 0,
    }
}

/// The pure policy verdict: a new target, or a hold carrying the gate that held. The reason is
/// not decoration — "why didn't it move" is the question every balloon incident re-derived from
/// scattered logs, and the bench's decision trace records it per report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Command this new target (pages).
    Set(u32),
    /// Gap decay: the target was trimmed to the driver's actual fill (pages). Never returned
    /// by [`decide`] — `on_pressure` substitutes it when [`gap_action`] fires, so the trace
    /// distinguishes a decay from a policy release (`first_target_decrease_after` in the bench
    /// filters on `"set"`).
    Decay(u32),
    /// Pressure give-back: the guest is sustainedly hurting (io-full storm, or the NotCalm
    /// memory band held on avg60) behind a held balloon while every release threshold reads
    /// fine — deflate one step to this target (pages). Distinct from `Set` so the trace
    /// tells a give-back from an allowance deflate.
    GiveBack(u32),
    /// Leave the current target alone.
    Hold(Hold),
}

impl Decision {
    /// The commanded target, if any (test/trace convenience).
    fn target(self) -> Option<u32> {
        match self {
            Decision::Set(t) | Decision::Decay(t) | Decision::GiveBack(t) => Some(t),
            Decision::Hold(_) => None,
        }
    }

    /// Short stable label for the trace (`set`, `gap-decay`, `giveback`, or the hold gate).
    fn label(self) -> &'static str {
        match self {
            Decision::Set(_) => "set",
            Decision::Decay(_) => "gap-decay",
            Decision::GiveBack(_) => "giveback",
            Decision::Hold(Hold::Converged) => "converged",
            Decision::Hold(Hold::NotIdle) => "not-idle",
            Decision::Hold(Hold::DeadBand) => "dead-band",
            Decision::Hold(Hold::NotCalm) => "not-calm",
            Decision::Hold(Hold::Cooldown) => "cooldown",
            Decision::Hold(Hold::Dwell) => "dwell",
            Decision::Hold(Hold::FreeExhausted) => "free-exhausted",
            Decision::Hold(Hold::Inelastic) => "inelastic",
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
    /// Inflation gate: the guest's free list is at the mode's margin and the host doesn't need
    /// the memory urgently (Normal) — wait for the guest to free memory on its own instead of
    /// forcing cache reclaim.
    FreeExhausted,
    /// Inflation gate: the last judged step was fed by reclaim, not free memory (see
    /// [`elasticity_action`]) — at host-Normal further steps would keep eating page cache
    /// through a MemFree reading the kernel holds at its watermark equilibrium. Released when
    /// the guest's free list genuinely rises ([`INELASTIC_FREE_RISE_KIB`]).
    Inelastic,
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
/// push back before the squeeze overshoots — further paced by the MemFree clamp (steps sized to
/// the guest's free list; cache reclaim only ever forced at the trickle rate, and only when the
/// host needs the memory). Aggressive keeps its original shape (squeeze to the floor while ≥30%
/// is available, host pressure ignored) with the same inflation guards.
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

    // Pressure give-back: the guest is sustainedly hurting behind a balloon the allowance
    // math would keep (desired >= current) — the sticky-wedge class, a dug-down guest
    // thrashing at 5× while every release threshold reads "fine" and avail sits flat
    // (re-reads swap cache page for cache page, so the shortfall deflate never engages;
    // [`guest_starved`] only catches the catastrophic end). Cache starvation lands on
    // either PSI side, one measured incident each: io-full (44% in the 2026-07-09 wedge —
    // refaults past the workingset window count as plain io) or sustained memory-some
    // (7% on the S3 warn-dug point, io-full peaking 2% on host-cached-fast storage) —
    // hence the disjunction. The memory arm is the NotCalm band held sustainedly (avg60):
    // inflation needs avg60 <= PRESSURE_LOW and the give-back needs avg60 > PRESSURE_LOW,
    // so no state is both inflation-eligible and give-back-eligible — the band stops being
    // a parking orbit for a held balloon and becomes a slow, dwell-paced deflate. Never
    // over a real deflate (desired < current is bigger — let it through), at host Critical
    // (the host's need wins; the starvation release still floors it), or for Aggressive
    // (its contract ignores guest comfort). An old agent (io fields 0) lacks the io arm;
    // the memory arm rides fields every agent reports.
    // (never for a balloon below [`giveback_floor_pages`] — the PSI triggers can't tell
    // balloon-induced thrash from the guest's own workload IO, so a balloon too small to
    // plausibly cause the pain must not be knifed: that was the 08-12 limit cycle, every
    // first step given back the moment it landed and every release lever starved).
    if (p.io_full_avg10 >= IO_PRESSURE_HIGH || p.some_avg60 > PRESSURE_LOW)
        && i.current >= giveback_floor_pages(i.max_pages)
        && desired >= i.current
        && i.host != HostPressure::Critical
        && i.mode != ReclaimMode::Aggressive
    {
        if let Some(t) = i.last_change {
            if i.now.duration_since(t) < DWELL {
                return Decision::Hold(Hold::Dwell);
            }
        }
        // The step doubles per consecutive give-back (256 MiB → 512 MiB → 1 GiB): the first
        // step stays sensor-paced, but a trigger that survives a whole dwell after a step
        // means the shortfall is deep — walk it down in dwells-log time, not dwells-linear.
        let step = INFLATE_STEP_PAGES << i.giveback_streak.min(GIVEBACK_MAX_SHIFT);
        return Decision::GiveBack(i.current.saturating_sub(step));
    }

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
        // Self-preservation pacing clamp. The driver satisfies ANY target: past the guest's
        // free list it digs into page cache at full inflate speed (and toward guest death
        // beyond that) — the guest has no wall of its own, so this is it. The step is capped
        // by what the guest can hand over WITHOUT reclaim (MemFree minus the mode's margin,
        // measured from the driver's actual fill); past that it drops to the trickle when the
        // squeeze contract still owes memory, or holds when the host doesn't need it urgently.
        // `mem_free_kib == 0` = an agent predating the field: clamp off, pre-clamp behavior.
        let step = if p.mem_free_kib == 0 {
            INFLATE_STEP_PAGES
        } else {
            // Elasticity gate first: when the last judged step was fed by reclaim, a healthy
            // MemFree reading is a lie (kswapd backfills it from cache), so the headroom math
            // below must not run at host-Normal. When memory is owed (Warn/Critical) or the
            // mode ignores host pressure (Aggressive), digging cache is the contract — proceed.
            if i.inelastic && i.host == HostPressure::Normal && i.mode != ReclaimMode::Aggressive {
                return Decision::Hold(Hold::Inelastic);
            }
            // An inelastic dig in force at Warn/Critical proceeds (digging is the squeeze
            // contract), but paced by the guest's io pain: dropping cold cache is free, so
            // full speed stands while io-PSI is quiet — refaults pushing io-full up mean the
            // dig is taking pages the guest still reads, and the trickle lets its reclaim
            // keep up. (kswapd holds MemFree above the margin during a dig, so the headroom
            // clamp below never paces this case on its own — the out-clampgrade warn residual.)
            // Deliberately io-keyed only, no memory arm: NotCalm's avg10 gate above already
            // arrests a dig the moment memory-some rises, at every host level — this covers
            // the one shape NotCalm can't see, io-attributed pain with memory quiet.
            let max_step = if i.inelastic
                && i.mode != ReclaimMode::Aggressive
                && p.io_full_avg10 > IO_PRESSURE_LOW
            {
                TRICKLE_STEP_PAGES
            } else {
                INFLATE_STEP_PAGES
            };
            let free_pages = (p.mem_free_kib / 4).min(u32::MAX as u64) as u32;
            let headroom = free_pages.saturating_sub(free_margin_pages(i.mode));
            let cap = i.actual_pages.unwrap_or(i.current).saturating_add(headroom);
            let cap_step = cap.saturating_sub(i.current).min(max_step);
            if cap_step >= DEAD_BAND_PAGES {
                cap_step
            } else if i.mode == ReclaimMode::Aggressive || i.host != HostPressure::Normal {
                TRICKLE_STEP_PAGES
            } else {
                return Decision::Hold(Hold::FreeExhausted);
            }
        };
        i.current.saturating_add(step).min(desired)
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
            mem_available_kib: avail_kib,
            mem_total_kib: total_kib,
            ..Default::default()
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
            actual_pages: None,
            room: ROOM,
            max_pages: MAX,
            inelastic: false,
            giveback_streak: 0,
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

    /// `report_pages` with the guest's free list set (in pages) — engages the pacing clamp.
    fn report_with_free(
        some_avg10: u32,
        avail_pages: u32,
        free_pages: u32,
        total_pages: u32,
    ) -> MemPressure {
        let mut p = report_pages(some_avg10, avail_pages, total_pages);
        p.mem_free_kib = free_pages as u64 * 4;
        p
    }

    /// One stray improved sample must not demote the acted-on host level (Light dumps its
    /// whole ramp on it — bench lever 7); worsening acts immediately, and a sustained lower
    /// reading demotes after [`HOST_DEMOTE_SUSTAIN`].
    #[test]
    fn host_debounce_demotes_only_after_sustain() {
        let t0 = Instant::now();
        let s = Duration::from_secs(1);
        let mut d = HostDebounce::new();
        // Worsening is immediate, at every rung.
        assert_eq!(d.observe(HostPressure::Warn, t0), HostPressure::Warn);
        // One Normal blip holds Warn…
        assert_eq!(d.observe(HostPressure::Normal, t0 + s), HostPressure::Warn);
        // …and raw returning to the held level cancels the pending demotion entirely:
        // the sustain clock restarts from the next below-reading.
        assert_eq!(
            d.observe(HostPressure::Warn, t0 + 2 * s),
            HostPressure::Warn
        );
        assert_eq!(
            d.observe(HostPressure::Normal, t0 + 3 * s),
            HostPressure::Warn
        );
        assert_eq!(
            d.observe(HostPressure::Normal, t0 + 3 * s + HOST_DEMOTE_SUSTAIN - s),
            HostPressure::Warn
        );
        // The lower reading sustained: demote.
        assert_eq!(
            d.observe(HostPressure::Normal, t0 + 3 * s + HOST_DEMOTE_SUSTAIN),
            HostPressure::Normal
        );
        // Worsening past the held level during a pending demotion snaps up immediately.
        let mut d = HostDebounce::new();
        assert_eq!(d.observe(HostPressure::Warn, t0), HostPressure::Warn);
        assert_eq!(d.observe(HostPressure::Normal, t0 + s), HostPressure::Warn);
        assert_eq!(
            d.observe(HostPressure::Critical, t0 + 2 * s),
            HostPressure::Critical
        );
    }

    /// A demotion that fires lands on the raw level read at fire time — a Critical hold
    /// over oscillating Warn/Normal readings must not skip to a level the host isn't at.
    #[test]
    fn host_debounce_demotion_lands_on_the_current_raw_level() {
        let t0 = Instant::now();
        let s = Duration::from_secs(1);
        let mut d = HostDebounce::new();
        assert_eq!(
            d.observe(HostPressure::Critical, t0),
            HostPressure::Critical
        );
        assert_eq!(
            d.observe(HostPressure::Normal, t0 + s),
            HostPressure::Critical
        );
        assert_eq!(
            d.observe(HostPressure::Warn, t0 + 2 * s),
            HostPressure::Critical
        );
        // Below-the-held-level readings sustained: fire on the CURRENT raw (Warn).
        assert_eq!(
            d.observe(HostPressure::Warn, t0 + s + HOST_DEMOTE_SUSTAIN),
            HostPressure::Warn
        );
        // The next demotion (Warn -> Normal) needs its own sustain.
        assert_eq!(
            d.observe(HostPressure::Normal, t0 + 2 * s + HOST_DEMOTE_SUSTAIN),
            HostPressure::Warn
        );
        assert_eq!(
            d.observe(
                HostPressure::Normal,
                t0 + 2 * s + HOST_DEMOTE_SUSTAIN + HOST_DEMOTE_SUSTAIN
            ),
            HostPressure::Normal
        );
    }

    /// `report_pages` with io-PSI `full` avg10 set (hundredths of a %).
    fn report_with_io(
        some_avg10: u32,
        avail_pages: u32,
        total_pages: u32,
        io_full: u32,
    ) -> MemPressure {
        let mut p = report_pages(some_avg10, avail_pages, total_pages);
        p.io_full_avg10 = io_full;
        p
    }

    /// The pressure give-back: a guest sustainedly hurting behind a balloon the allowance
    /// math would keep deflates one step per dwell — the S3 sticky-wedge shape (thrashing
    /// at 5× while every release threshold reads fine). Either trigger arm suffices: an
    /// io-full storm (the 2026-07-09 attribution) or the NotCalm memory band held on avg60
    /// (the S3 warn-dug attribution).
    #[test]
    fn pressure_giveback_deflates_a_step_when_the_guest_sustainedly_hurts() {
        // Converged at the 1 GiB allowance (desired == current): io-full 10% fires it.
        let cur = 3 * GIB_PAGES;
        let p = report_with_io(0, GIB_PAGES, MAX, 1000);
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, cur);
        assert_eq!(decide(&p, &i), Decision::GiveBack(cur - INFLATE_STEP_PAGES));
        // Same at host Warn: the guest's sustained pain outranks a non-critical host's want.
        let i = inputs(ReclaimMode::Moderate, HostPressure::Warn, cur);
        assert_eq!(decide(&p, &i), Decision::GiveBack(cur - INFLATE_STEP_PAGES));
        // The memory arm: sustained NotCalm (avg60 past the calm bar) with io quiet — the
        // S3 warn-dug shape (reclaim stalls on host-cached-fast storage read ~7% here while
        // io-full peaked at 2%). Works from any agent (memory PSI is in every report).
        let mut p = report_pages(0, GIB_PAGES, MAX);
        p.some_avg60 = 707;
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, cur);
        assert_eq!(decide(&p, &i), Decision::GiveBack(cur - INFLATE_STEP_PAGES));
        // A 10 s blip (avg10 in the neutral band, avg60 still calm) is NOT sustained pain:
        // no give-back — converged holds, same as before the term existed.
        let p = report_pages(500, GIB_PAGES, MAX);
        assert_eq!(decide(&p, &i), Decision::Hold(Hold::Converged));
        // Dwell-paced: a fresh target change holds this tick (the PSI-EMA lag must not
        // dump the whole balloon at report rate).
        let p = report_with_io(0, GIB_PAGES, MAX, 1000);
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, cur);
        i.last_change = Some(i.now);
        assert_eq!(decide(&p, &i), Decision::Hold(Hold::Dwell));
        // Saturates at zero rather than underflowing (a floor-sized balloon, escalated
        // step bigger than it; the sub-floor case is the attribution test's business).
        let mut i = inputs(
            ReclaimMode::Moderate,
            HostPressure::Normal,
            giveback_floor_pages(MAX),
        );
        i.giveback_streak = GIVEBACK_MAX_SHIFT;
        assert_eq!(decide(&p, &i), Decision::GiveBack(0));
    }

    /// The give-back must never shadow a bigger release: a below-allowance shortfall deflate
    /// is immediate and unbounded — capping it at one give-back step would slow the very
    /// direction that is always safe.
    #[test]
    fn pressure_giveback_yields_to_the_allowance_deflate() {
        let cur = 3 * GIB_PAGES;
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, cur);
        i.last_change = Some(i.now); // deflation ignores the dwell
        let p = report_with_io(0, GIB_PAGES / 4, MAX, 2000);
        assert_eq!(
            decide(&p, &i),
            Decision::Set(cur - (GIB_PAGES - GIB_PAGES / 4))
        );
    }

    /// Where the give-back must NOT fire: host Critical (the host's need wins — the
    /// starvation release still floors it), Aggressive (its contract ignores guest
    /// comfort), and both trigger arms under their bars (an old agent's io fields read 0,
    /// so it simply lacks the io arm; the memory arm rides fields every agent reports).
    #[test]
    fn pressure_giveback_exclusions() {
        let cur = 3 * GIB_PAGES;
        // Critical, converged at the squeeze floor (512 MiB on this VM): the squeeze stands.
        let floor = GIB_PAGES / 2;
        let i = inputs(ReclaimMode::Moderate, HostPressure::Critical, cur);
        let p = report_with_io(0, floor, MAX, 4400);
        assert_eq!(decide(&p, &i), Decision::Hold(Hold::Converged));
        // Aggressive with an idle guest inflates through io pressure (the original shape).
        let i = inputs(ReclaimMode::Aggressive, HostPressure::Normal, cur);
        let p = report_with_io(0, MAX * 7 / 10, MAX, 4400);
        assert_eq!(decide(&p, &i), Decision::Set(cur + INFLATE_STEP_PAGES));
        // io just under the bar: converged hold, no give-back.
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, cur);
        let p = report_with_io(0, GIB_PAGES, MAX, IO_PRESSURE_HIGH - 1);
        assert_eq!(decide(&p, &i), Decision::Hold(Hold::Converged));
        // Old agent, guest calm: io fields absent read as 0 (io arm off, clamp contract)
        // and avg60 is calm — no give-back.
        let p = report_pages(0, GIB_PAGES, MAX);
        assert_eq!(decide(&p, &i), Decision::Hold(Hold::Converged));
    }

    /// The give-back step doubles per consecutive give-back (256 MiB → 512 MiB → 1 GiB cap):
    /// the first step stays sensor-paced, a trigger that survives whole dwells means the
    /// shortfall is deep. Saturation still floors the escalated step at zero.
    #[test]
    fn pressure_giveback_escalates_on_consecutive_fires() {
        let cur = 6 * GIB_PAGES;
        let p = report_with_io(0, GIB_PAGES, MAX, 1000);
        let ladder = [
            (0, INFLATE_STEP_PAGES),
            (1, 2 * INFLATE_STEP_PAGES),
            (2, 4 * INFLATE_STEP_PAGES),
            (3, 4 * INFLATE_STEP_PAGES), // capped at 1 GiB
            (9, 4 * INFLATE_STEP_PAGES),
        ];
        for (streak, step) in ladder {
            let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, cur);
            i.giveback_streak = streak;
            assert_eq!(decide(&p, &i), Decision::GiveBack(cur - step));
        }
        // An escalated step bigger than the balloon saturates at zero.
        let mut i = inputs(
            ReclaimMode::Moderate,
            HostPressure::Normal,
            3 * INFLATE_STEP_PAGES,
        );
        i.giveback_streak = 2;
        assert_eq!(decide(&p, &i), Decision::GiveBack(0));
    }

    /// The trigger needs a plausibly CAUSAL balloon (the 2026-08-12 dogfood limit cycle):
    /// a guest whose OWN workload held io-full over the bar knifed every first 256 MiB
    /// step the moment it landed — 201 sets / 46 give-backs over 2.3 h, balloon pinned at
    /// ~0, so no release ever flowed and the retention pool grew unchecked under host
    /// Warn. Below [`giveback_floor_pages`] the balloon cannot be what starves the
    /// guest's cache; the give-back stands aside and the normal flow proceeds.
    #[test]
    fn pressure_giveback_needs_a_plausibly_causal_balloon() {
        let floor = MAX / 16; // 512 MiB on this VM — above one step, so the ratio governs
        assert_eq!(giveback_floor_pages(MAX), floor);
        let p = report_with_io(0, MAX / 2, MAX, 4400); // io-full 44%, guest otherwise calm
                                                       // One standing step (the dogfood shape): not a plausible cause — no give-back.
        for cur in [INFLATE_STEP_PAGES, floor - 1] {
            let i = inputs(ReclaimMode::Moderate, HostPressure::Warn, cur);
            let d = decide(&p, &i);
            assert!(!matches!(d, Decision::GiveBack(_)), "cur={cur}: {d:?}");
        }
        // At the floor: plausibly causal — fires exactly as before.
        let i = inputs(ReclaimMode::Moderate, HostPressure::Warn, floor);
        assert_eq!(
            decide(&p, &i),
            Decision::GiveBack(floor - INFLATE_STEP_PAGES)
        );
        // On a tiny VM the ratio would undercut one inflation step: the floor never does
        // (a balloon smaller than the first step is definitionally that step — knifing it
        // recreates the limit cycle at any size).
        assert_eq!(giveback_floor_pages(GIB_PAGES), INFLATE_STEP_PAGES);
    }

    /// The streak advance: a sent give-back extends the episode, the give-back path's dwell
    /// hold preserves it (a nonzero streak can only coexist with give-back dwells — every
    /// sent give-back arms the release cooldown, so the inflation path answers Cooldown for
    /// 300 s), and everything else — an allowance move, an unsent give-back, any other hold —
    /// ends the episode.
    #[test]
    fn giveback_streak_advances_and_resets() {
        assert_eq!(giveback_streak_next(0, Decision::GiveBack(0), true), 1);
        assert_eq!(giveback_streak_next(2, Decision::GiveBack(0), true), 3);
        assert_eq!(giveback_streak_next(2, Decision::GiveBack(0), false), 0);
        assert_eq!(
            giveback_streak_next(2, Decision::Hold(Hold::Dwell), false),
            2
        );
        assert_eq!(giveback_streak_next(2, Decision::Set(0), true), 0);
        assert_eq!(giveback_streak_next(2, Decision::Decay(0), true), 0);
        assert_eq!(
            giveback_streak_next(2, Decision::Hold(Hold::Cooldown), false),
            0
        );
        assert_eq!(
            giveback_streak_next(2, Decision::Hold(Hold::Converged), false),
            0
        );
    }

    /// An inelastic dig at host Warn proceeds (digging is the squeeze contract) but paced by
    /// the guest's io pain: full speed while the evicted cache is cold, the trickle once
    /// refaults push io-full up — kswapd keeps MemFree above the margin during a dig, so the
    /// headroom clamp alone never paces this case (the out-clampgrade warn residual).
    #[test]
    fn inelastic_dig_paced_by_io_pressure() {
        let cur = GIB_PAGES;
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Warn, cur);
        i.inelastic = true;
        // Free at a deep 800 MiB (headroom > a full step): the clamp alone grants full speed.
        let mut p = report_with_free(0, 4 * GIB_PAGES, 800 * PAGES_PER_MIB, MAX);
        p.io_full_avg10 = 300; // refaults showing: pace to the trickle
        assert_eq!(decide(&p, &i), Decision::Set(cur + TRICKLE_STEP_PAGES));
        p.io_full_avg10 = 0; // cold-cache dig: full speed stands
        assert_eq!(decide(&p, &i), Decision::Set(cur + INFLATE_STEP_PAGES));
        // Aggressive is exempt from the pacing, like the rest of the elasticity machinery.
        let mut i = inputs(ReclaimMode::Aggressive, HostPressure::Warn, cur);
        i.inelastic = true;
        let mut p = report_with_free(0, 4 * GIB_PAGES, 800 * PAGES_PER_MIB, MAX);
        p.io_full_avg10 = 300;
        assert_eq!(decide(&p, &i), Decision::Set(cur + INFLATE_STEP_PAGES));
    }

    /// The pacing clamp: a cache-heavy guest (avail huge, free small) only yields what its free
    /// list holds beyond the mode's margin — never a full step dug out of page cache.
    #[test]
    fn clamp_paces_inflation_to_the_free_list() {
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        // 7 GiB available but only margin + 100 MiB free: the step is 100 MiB, not 256.
        let free = free_margin_pages(ReclaimMode::Moderate) + 100 * PAGES_PER_MIB;
        let next = decide(&report_with_free(0, 7 * GIB_PAGES, free, MAX), &i);
        assert_eq!(next, Decision::Set(100 * PAGES_PER_MIB));
    }

    /// The free≈0 row at host-Normal (where the dogfood guest lives): the free list is at the
    /// margin and the host doesn't need the memory — hold, don't force cache reclaim.
    #[test]
    fn clamp_holds_at_host_normal_when_free_is_exhausted() {
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        let free = free_margin_pages(ReclaimMode::Moderate); // headroom exactly 0
        assert_eq!(
            decide(&report_with_free(0, 7 * GIB_PAGES, free, MAX), &i),
            Decision::Hold(Hold::FreeExhausted)
        );
    }

    /// The free≈0 row when the squeeze contract still owes memory (host Warn/Critical, or
    /// Aggressive anywhere): inflation continues at the trickle, never at full sprint.
    #[test]
    fn clamp_trickles_when_free_is_exhausted_but_memory_is_owed() {
        let tiny_free = 4; // pages; nonzero so the clamp is engaged, far below every margin
        for (mode, host) in [
            (ReclaimMode::Moderate, HostPressure::Warn),
            (ReclaimMode::Moderate, HostPressure::Critical),
            (ReclaimMode::Light, HostPressure::Warn),
            (ReclaimMode::Aggressive, HostPressure::Normal),
        ] {
            let i = inputs(mode, host, 0);
            assert_eq!(
                decide(&report_with_free(0, 7 * GIB_PAGES, tiny_free, MAX), &i),
                Decision::Set(TRICKLE_STEP_PAGES),
                "{mode:?}/{host:?}"
            );
        }
    }

    /// The elasticity verdicts: free tracking the fill is elastic; flat free under a grown fill
    /// is reclaim backfilling from cache (inelastic); an unmoved fill is still judging; a
    /// deflate under the probe is stale. This is the detector for the clampgrade finding: on a
    /// cache-warm guest kswapd holds MemFree at its watermark equilibrium above every margin,
    /// so the free-level clamp alone never binds while cache is being eaten.
    #[test]
    fn elasticity_action_judges_free_against_fill() {
        let free0: u64 = 550 * 1024; // KiB (~the observed 12G-guest watermark equilibrium)
        let probe = ElasticityProbe {
            free_kib_at_send: free0,
            actual_at_send: GIB_PAGES,
        };
        let absorbed = 64 * PAGES_PER_MIB; // one judged fill increment
        let grown = GIB_PAGES + absorbed;
        // Fill barely moved: no verdict yet.
        assert_eq!(
            elasticity_action(&probe, free0, GIB_PAGES + ELASTIC_MIN_FILL_PAGES - 1),
            Elasticity::Judging
        );
        // Free surrendered the full 64 MiB the balloon absorbed: elastic.
        assert_eq!(
            elasticity_action(&probe, free0 - 64 * 1024, grown),
            Elasticity::Elastic
        );
        // Free surrendered exactly half: still elastic (the threshold is strict).
        assert_eq!(
            elasticity_action(&probe, free0 - 32 * 1024, grown),
            Elasticity::Elastic
        );
        // Free flat while the fill grew: reclaim fed the balloon from cache.
        assert_eq!(
            elasticity_action(&probe, free0, grown),
            Elasticity::Inelastic
        );
        // Free ROSE while the fill grew (guest freeing concurrently): conservatively inelastic —
        // the balloon demonstrably wasn't drawing the free list down.
        assert_eq!(
            elasticity_action(&probe, free0 + 128 * 1024, grown),
            Elasticity::Inelastic
        );
        // The balloon deflated under the probe: no verdict possible.
        assert_eq!(
            elasticity_action(&probe, free0, GIB_PAGES - 1),
            Elasticity::Stale
        );
    }

    /// The release baseline decays to the lowest free level observed while held: a verdict
    /// struck on a falling free list (out-clampgrade2: 8 GiB mid-mix, all of it then
    /// legitimately becoming cache) must not leave an unreachable release bar. Release fires
    /// on a genuine rise off the floor; the watermark wobble stays held.
    #[test]
    fn inelastic_hold_releases_from_the_floor_not_the_verdict() {
        let mut h = InelasticHold {
            free_kib_floor: 8 * 1024 * 1024, // verdict struck at a transient 8 GiB
        };
        assert!(!h.observe(2 * 1024 * 1024)); // free falls: floor decays, still held
        assert!(!h.observe(550 * 1024)); // down to the watermark equilibrium
        assert!(!h.observe(600 * 1024)); // wobble above the floor: held
        assert!(h.observe(550 * 1024 + INELASTIC_FREE_RISE_KIB)); // a genuine free: released
    }

    /// An in-force inelastic verdict gates inflation at host-Normal for the cache-respecting
    /// modes — and ONLY there: when memory is owed (Warn) or the mode's contract is the squeeze
    /// (Aggressive), digging cache is intended and inflation proceeds. An old agent
    /// (mem_free_kib == 0) can never reach the gate.
    #[test]
    fn inelastic_holds_at_host_normal_and_yields_when_memory_is_owed() {
        // Free far above the margin so ONLY the elasticity gate can be the reason to hold.
        let free = free_margin_pages(ReclaimMode::Moderate) + 512 * PAGES_PER_MIB;
        let p = report_with_free(0, 7 * GIB_PAGES, free, MAX);
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        i.inelastic = true;
        assert_eq!(decide(&p, &i), Decision::Hold(Hold::Inelastic));
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Warn, 0);
        i.inelastic = true;
        assert_eq!(decide(&p, &i), Decision::Set(INFLATE_STEP_PAGES));
        let mut i = inputs(ReclaimMode::Aggressive, HostPressure::Normal, 0);
        i.inelastic = true;
        assert_eq!(decide(&p, &i), Decision::Set(INFLATE_STEP_PAGES));
        // Old agent: the clamp block (and its gate) is bypassed entirely.
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        i.inelastic = true;
        assert_eq!(
            decide(&report_pages(0, 7 * GIB_PAGES, MAX), &i),
            Decision::Set(INFLATE_STEP_PAGES)
        );
    }

    /// Free-list headroom is measured from the driver's ACTUAL fill when stats are available:
    /// an outstanding commanded-but-unfilled gap consumes the headroom.
    #[test]
    fn clamp_measures_headroom_from_actual() {
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, GIB_PAGES);
        i.actual_pages = Some(GIB_PAGES / 2); // 512 MiB still unfilled
        let free = free_margin_pages(ReclaimMode::Moderate) + 600 * PAGES_PER_MIB;
        let next = decide(&report_with_free(0, 7 * GIB_PAGES, free, MAX), &i);
        // cap = actual (512 MiB) + headroom (600 MiB) = 1112 MiB; current 1024 → step 88 MiB.
        assert_eq!(next, Decision::Set(GIB_PAGES + 88 * PAGES_PER_MIB));
    }

    /// An old agent (mem_free_kib 0 = field absent) disables the clamp entirely — pre-clamp
    /// behavior, exactly as the proto contract requires (0 is never "an empty free pool").
    #[test]
    fn clamp_off_when_the_report_lacks_mem_free() {
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        assert_eq!(
            decide(&report_pages(0, 7 * GIB_PAGES, MAX), &i),
            Decision::Set(INFLATE_STEP_PAGES)
        );
    }

    #[test]
    fn gap_action_tracks_arms_and_fires() {
        let now = Instant::now();
        let target = 2 * GIB_PAGES;
        let track = |since: Instant, at_arm: u32| GapTrack {
            since,
            actual_at_arm: at_arm,
        };
        // Gap below the dead band: cleared, even with a stale track armed.
        let filled = target - (GAP_EPS_PAGES - 1);
        assert_eq!(
            gap_action(
                Some(&track(now - GAP_DECAY_AFTER, 0)),
                target,
                filled,
                false,
                now
            ),
            GapAction::Clear
        );
        // A real gap with no tracking arms the timer.
        let stuck = target - GIB_PAGES;
        assert_eq!(gap_action(None, target, stuck, false, now), GapAction::Arm);
        // Fill progress since arming re-arms (a slow trickle fill is legitimate)...
        let old = now - GAP_DECAY_AFTER - Duration::from_secs(1);
        assert_eq!(
            gap_action(
                Some(&track(old, stuck - GAP_EPS_PAGES)),
                target,
                stuck,
                false,
                now
            ),
            GapAction::Arm
        );
        // ...but a STUCK gap past the deadline fires,
        assert_eq!(
            gap_action(Some(&track(old, stuck)), target, stuck, false, now),
            GapAction::Fire
        );
        // never on a tick that just moved the target,
        assert_eq!(
            gap_action(Some(&track(old, stuck)), target, stuck, true, now),
            GapAction::Stay
        );
        // and never before the deadline.
        assert_eq!(
            gap_action(Some(&track(now, stuck)), target, stuck, false, now),
            GapAction::Stay
        );
    }

    #[test]
    fn new_decision_labels_are_stable() {
        // The bench summarizer keys off these strings; a rename is a contract change.
        assert_eq!(Decision::Decay(5).label(), "gap-decay");
        assert_eq!(
            Decision::Hold(Hold::FreeExhausted).label(),
            "free-exhausted"
        );
        assert_eq!(Decision::Hold(Hold::Inelastic).label(), "inelastic");
        assert_eq!(Decision::Decay(5).target(), Some(5));
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
                ..Default::default()
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
            distress_until: None,
            scrub: None,
            scrub_gen: 0,
            last_scrub_end: Instant::now(),
            last_sweep: Instant::now(),
            gap: None,
            elastic_probe: None,
            inelastic: None,
            host_debounce: HostDebounce::new(),
            giveback_streak: 0,
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
        let wstats = WorkerStats {
            actual_bytes: 1 << 30,
            reclaimed_bytes: 2 << 30,
            heals: 7,
            released_bytes: 3 << 30,
            remapped_bytes: 1 << 20,
            stray_faults: 0,
            sweeps: 2,
            sweep_debited_bytes: 4 << 30,
            sweep_ms: 41,
            sweep_faults: 5,
            compressed_bytes: 3 << 29,
        };
        trace_decision(
            &mut st,
            &report_pages(0, 4 * GIB_PAGES, MAX),
            &host,
            &i,
            Decision::Set(INFLATE_STEP_PAGES),
            true,
            Some(&wstats),
        );
        i.cooldown_until = Some(i.now + Duration::from_secs(100));
        trace_decision(
            &mut st,
            &report_pages(0, 4 * GIB_PAGES, MAX),
            &host,
            &i,
            Decision::Hold(Hold::Cooldown),
            false,
            None,
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
                && lines[0].contains("\"host_raw_level\":2")
                && lines[0].contains("\"free_kib\":0")
                && lines[0].contains("\"io_full_avg10\":0")
                && lines[0].contains(&format!("\"actual_bytes\":{}", 1u64 << 30))
                && lines[0].contains("\"heals\":7")
                && lines[0].contains("\"stray_faults\":0")
                && lines[0].contains("\"sweeps\":2")
                && lines[0].contains("\"sweep_debited_bytes\":4294967296")
                && lines[0].contains("\"sweep_faults\":5")
                && lines[0].contains("\"compressed_bytes\":1610612736"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("\"decision\":\"cooldown\"")
                && lines[1].contains("\"new_target_pages\":null")
                && lines[1].contains("\"cooldown_active\":true")
                && lines[1].contains("\"sent\":false")
                && lines[1].contains("\"heals\":null"),
            "{}",
            lines[1]
        );
    }

    /// The settle-sweep cadence: armed at construction (no boot-time sweep), mode-keyed
    /// interval, fires exactly at the boundary.
    #[test]
    fn sweep_due_on_mode_keyed_cadence() {
        let base = Instant::now();
        for (mode, cooldown) in [
            (ReclaimMode::Light, 2 * SWEEP_COOLDOWN),
            (ReclaimMode::Moderate, SWEEP_COOLDOWN),
            (ReclaimMode::Aggressive, SWEEP_COOLDOWN / 2),
        ] {
            assert_eq!(sweep_cooldown(mode), cooldown);
            assert!(
                !sweep_due(mode, base, base + cooldown - Duration::from_secs(1)),
                "{mode:?} swept before its cooldown"
            );
            assert!(
                sweep_due(mode, base, base + cooldown),
                "{mode:?} did not sweep at its cooldown"
            );
        }
    }

    /// The scrub trigger matrix: host level × acute × cooldowns × meaningful inflate. The
    /// armed-at-construction scrub cooldown is what keeps bench scenarios (injected
    /// Warn/Critical, calm guest — exactly the trigger shape) scrub-free, so it's load-bearing,
    /// not a nicety.
    #[test]
    fn scrub_due_only_under_host_pressure_calm_and_out_of_cooldowns() {
        let m = scrub_params(ReclaimMode::Moderate);
        let base = Instant::now();
        let now = base + m.cooldown; // base = construction/last scrub; cooldown just elapsed
        assert!(scrub_due(
            &m,
            HostPressure::Warn,
            false,
            ROOM,
            0,
            None,
            base,
            now
        ));
        assert!(scrub_due(
            &m,
            HostPressure::Critical,
            false,
            ROOM,
            0,
            None,
            base,
            now
        ));
        // Host fine: never — the scrub's cost buys nothing the host needs.
        assert!(!scrub_due(
            &m,
            HostPressure::Normal,
            false,
            ROOM,
            0,
            None,
            base,
            now
        ));
        // Acute guest pressure or starvation: never.
        assert!(!scrub_due(
            &m,
            HostPressure::Warn,
            true,
            ROOM,
            0,
            None,
            base,
            now
        ));
        // Inside the acute-distress holdoff: the guest just proved it needs its memory.
        // (Only acute reports arm this — a give-back arms the inflation cooldown alone, so
        // an io-busy guest's give-back cadence can't starve the scrub: 08-12, half B.)
        assert!(!scrub_due(
            &m,
            HostPressure::Warn,
            false,
            ROOM,
            0,
            Some(now + DWELL),
            base,
            now
        ));
        // An expired holdoff no longer blocks.
        assert!(scrub_due(
            &m,
            HostPressure::Warn,
            false,
            ROOM,
            0,
            Some(now - DWELL),
            base,
            now
        ));
        // Scrub cooldown: fresh construction (last_scrub_end = now) blocks the first cycle.
        assert!(!scrub_due(
            &m,
            HostPressure::Warn,
            false,
            ROOM,
            0,
            None,
            now,
            now
        ));
        // Balloon already nearly full: the freed pages were already released — nothing to scrub.
        assert!(!scrub_due(
            &m,
            HostPressure::Warn,
            false,
            ROOM,
            ROOM - SCRUB_MIN_INFLATE + 1,
            None,
            base,
            now
        ));
    }

    /// The idle-scrub trigger matrix: calm host + idle guest + MemFree reported + meaningful
    /// bounded inflate, on the long idle cadence. The blind spot it closes (a calm host never
    /// settles the dead share) and the cache-dump it must never cause (no MemFree report ⇒
    /// Bounded degrades to full room ⇒ not due) are both load-bearing.
    #[test]
    fn idle_scrub_due_only_when_everything_is_quiet() {
        let m = scrub_params(ReclaimMode::Moderate);
        let base = Instant::now();
        let idle = m.cooldown * IDLE_SCRUB_COOLDOWN_MULT;
        let now = base + idle;
        let free = 4 << 20; // 4 GiB of guest MemFree, in KiB
        let due = |host, acute, some, free_kib, to, fill, hold, last, at| {
            idle_scrub_due(&m, host, acute, some, free_kib, to, fill, hold, last, at)
        };
        assert!(due(
            HostPressure::Normal,
            false,
            PRESSURE_LOW,
            free,
            ROOM,
            0,
            None,
            base,
            now
        ));
        // Host not fully calm: the pressure trigger owns anything above Normal.
        assert!(!due(
            HostPressure::Warn,
            false,
            PRESSURE_LOW,
            free,
            ROOM,
            0,
            None,
            base,
            now
        ));
        // Guest busy (acute, or PSI above the idle line): leave it alone.
        assert!(!due(
            HostPressure::Normal,
            true,
            PRESSURE_LOW,
            free,
            ROOM,
            0,
            None,
            base,
            now
        ));
        assert!(!due(
            HostPressure::Normal,
            false,
            PRESSURE_LOW + 1,
            free,
            ROOM,
            0,
            None,
            base,
            now
        ));
        // No MemFree report: Bounded would degrade to a full-room cache dump — never idle.
        assert!(!due(
            HostPressure::Normal,
            false,
            PRESSURE_LOW,
            0,
            ROOM,
            0,
            None,
            base,
            now
        ));
        // Nothing meaningful to capture, distress holdoff, and the long cadence all block.
        assert!(!due(
            HostPressure::Normal,
            false,
            PRESSURE_LOW,
            free,
            ROOM,
            ROOM - SCRUB_MIN_INFLATE + 1,
            None,
            base,
            now
        ));
        assert!(!due(
            HostPressure::Normal,
            false,
            PRESSURE_LOW,
            free,
            ROOM,
            0,
            Some(now + DWELL),
            base,
            now
        ));
        assert!(!due(
            HostPressure::Normal,
            false,
            PRESSURE_LOW,
            free,
            ROOM,
            0,
            None,
            base,
            now - Duration::from_secs(1)
        ));
        // The pressure cooldown alone elapsing is NOT enough — idle waits the multiple out.
        assert!(!due(
            HostPressure::Normal,
            false,
            PRESSURE_LOW,
            free,
            ROOM,
            0,
            None,
            base,
            base + m.cooldown
        ));
    }

    /// The idle scrub end-to-end, not just the predicate: a calm report through the real
    /// `on_pressure` must reach the idle branch with sane inputs (fill from a live stats
    /// round-trip, Bounded target from the report's MemFree) and actually command the
    /// inflate on the worker socket. A wiring slip anywhere on that path — wrong `room`,
    /// a bad `acute` derivation, an early return upstream — passes the pure gate test and
    /// only fails here.
    #[test]
    fn on_pressure_wires_the_idle_scrub_through_to_the_worker_socket() {
        use std::io::{BufRead, BufReader, Write as _};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;

        // The idle cadence is real wall-clock (90 min for Moderate); the test time-travels by
        // backdating `last_scrub_end` instead of waiting. `Instant` can't represent times
        // before an OS-dependent epoch, so a just-booted host may not have the headroom.
        let params = scrub_params(ReclaimMode::Moderate);
        let Some(backdated) =
            Instant::now().checked_sub(params.cooldown * IDLE_SCRUB_COOLDOWN_MULT)
        else {
            eprintln!("skipping: host not up long enough to backdate the idle cadence");
            return;
        };
        // Pin the host sample: the real sysctls could read Warn on a loaded dev machine and
        // legitimately veto the idle scrub. Only `on_pressure` reads this variable, and this
        // is the sole test driving it.
        std::env::set_var("LIMINA_HOST_PRESSURE", "normal");

        // A fake worker on a real socket: answers `stats` with a fixed fill, records every
        // other command. The policy keeps one connection open across commands.
        let sock = std::env::temp_dir().join(format!(
            "limina-idle-scrub-wiring-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let fill_bytes = (GIB_PAGES as u64) << 12; // the driver reports a 1 GiB fill
        std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut writer = conn;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                if line.trim() == "stats" {
                    if writeln!(writer, "actual={fill_bytes} compressed=1610612736").is_err() {
                        return;
                    }
                } else {
                    let _ = tx.send(line.trim().to_string());
                }
            }
        });

        let pol = BalloonPolicy::new(GIB_PAGES, MAX, ReclaimMode::Moderate, sock.clone(), None);
        {
            let mut st = pol.state.lock().unwrap();
            st.last_scrub_end = backdated;
            st.target_pages = GIB_PAGES; // matches the fill the fake worker reports
        }

        // A genuinely idle guest: zero PSI, half of RAM available, 2 GiB truly free.
        let mut p = report_pages(0, MAX / 2, MAX);
        p.mem_free_kib = 2 * GIB_PAGES as u64 * 4;
        pol.on_pressure(&p);

        let cmd = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no target command reached the worker socket");
        let expected = scrub_target_pages(
            ScrubDepth::Bounded,
            ROOM,
            GIB_PAGES,
            p.mem_free_kib,
            free_margin_pages(ReclaimMode::Moderate),
        );
        assert_eq!(cmd, format!("target {}", (expected as u64) << 12));
        let st = pol.state.lock().unwrap();
        let scrub = st.scrub.as_ref().expect("scrub cycle armed in state");
        assert_eq!(scrub.phase, ScrubPhase::Inflating);
        assert_eq!(scrub.resume_pages, GIB_PAGES);
        drop(st);
        let _ = std::fs::remove_file(&sock);
    }

    /// The mode key: Light scrubs only under Critical and half as often; Aggressive scrubs
    /// under Warn twice as often as Moderate. (Depth is covered by `scrub_targets_by_depth`.)
    #[test]
    fn scrub_params_key_trigger_and_cadence_by_mode() {
        let (l, m, a) = (
            scrub_params(ReclaimMode::Light),
            scrub_params(ReclaimMode::Moderate),
            scrub_params(ReclaimMode::Aggressive),
        );
        let base = Instant::now();
        let now = base + l.cooldown; // long enough for every mode
                                     // Light: Warn is not enough; Critical is.
        assert!(!scrub_due(
            &l,
            HostPressure::Warn,
            false,
            ROOM,
            0,
            None,
            base,
            now
        ));
        assert!(scrub_due(
            &l,
            HostPressure::Critical,
            false,
            ROOM,
            0,
            None,
            base,
            now
        ));
        // Cadence ordering: Aggressive < Moderate < Light.
        assert!(a.cooldown < m.cooldown && m.cooldown < l.cooldown);
        // At a point where Moderate's cooldown has elapsed but Light's hasn't, only the
        // faster modes fire.
        let mid = base + m.cooldown;
        assert!(!scrub_due(
            &l,
            HostPressure::Critical,
            false,
            ROOM,
            0,
            None,
            base,
            mid
        ));
        assert!(scrub_due(
            &m,
            HostPressure::Warn,
            false,
            ROOM,
            0,
            None,
            base,
            mid
        ));
        // Depths: bounded for the host-considerate modes, full for Aggressive.
        assert_eq!(l.depth, ScrubDepth::Bounded);
        assert_eq!(m.depth, ScrubDepth::Bounded);
        assert_eq!(a.depth, ScrubDepth::Full);
    }

    /// Bounded depth inflates over the free list only; Full takes the room; Bounded without
    /// MemFree (old agent) degrades to Full rather than a no-op.
    #[test]
    fn scrub_targets_by_depth() {
        let margin = free_margin_pages(ReclaimMode::Moderate);
        let free_kib = (margin as u64 + 2048 * PAGES_PER_MIB as u64) * 4; // margin + 2 GiB free
        assert_eq!(
            scrub_target_pages(ScrubDepth::Full, ROOM, 0, free_kib, margin),
            ROOM
        );
        assert_eq!(
            scrub_target_pages(ScrubDepth::Bounded, ROOM, 0, 0, margin),
            ROOM
        );
        // fill 1 GiB + (free − margin) 2 GiB = 3 GiB.
        assert_eq!(
            scrub_target_pages(ScrubDepth::Bounded, ROOM, GIB_PAGES, free_kib, margin),
            3 * GIB_PAGES
        );
        // Never past the room.
        assert_eq!(
            scrub_target_pages(
                ScrubDepth::Bounded,
                2 * GIB_PAGES,
                GIB_PAGES,
                free_kib,
                margin
            ),
            2 * GIB_PAGES
        );
        // The due-gate composes with the bounded target: a cache-full guest (huge room, tiny
        // free list) has nothing a bounded scrub could capture — not due, whatever the room.
        let m = scrub_params(ReclaimMode::Moderate);
        let base = Instant::now();
        let now = base + m.cooldown;
        let fill = GIB_PAGES;
        let tiny = scrub_target_pages(ScrubDepth::Bounded, ROOM, fill, (margin as u64) * 4, margin);
        assert!(!scrub_due(
            &m,
            HostPressure::Warn,
            false,
            tiny,
            fill,
            None,
            base,
            now
        ));
    }

    #[test]
    fn scrub_step_phases_advance_on_completion_stall_or_timeout() {
        use ScrubPhase::*;
        use ScrubStep::*;
        let t = 10u64 << 30; // 10 GiB inflate target
        let short = Duration::from_secs(5);
        // Inflating: keep going while below 90% and progressing.
        assert_eq!(scrub_step(Inflating, short, Some(t / 2), t, 0, 0), Stay);
        // Stats unavailable: elapsed time is the only advance (stats are never load-bearing).
        assert_eq!(scrub_step(Inflating, short, None, t, 0, 0), Stay);
        assert_eq!(
            scrub_step(Inflating, SCRUB_INFLATE_TIMEOUT, None, t, 0, 0),
            ToHolding
        );
        // Reached ≥90% of target: advance early.
        assert_eq!(
            scrub_step(Inflating, short, Some(t / 10 * 9), t, 0, 0),
            ToHolding
        );
        // Stalled short of target: advance without burning the full timeout.
        assert_eq!(
            scrub_step(Inflating, short, Some(t / 2), t, 0, SCRUB_STALL_TICKS),
            ToHolding
        );
        // Holding is a pure timer.
        assert_eq!(scrub_step(Holding, short, Some(t), t, 0, 0), Stay);
        assert_eq!(
            scrub_step(Holding, SCRUB_HOLD, Some(t), t, 0, 0),
            ToDeflating
        );
        // Deflating: done when actual is back within slack of the resume target…
        let resume = 2u64 << 30;
        assert_eq!(
            scrub_step(Deflating, short, Some(t / 2), t, resume, 0),
            Stay
        );
        assert_eq!(
            scrub_step(Deflating, short, Some(resume + (1 << 20)), t, resume, 0),
            Done
        );
        // …or on timeout, stats or not.
        assert_eq!(
            scrub_step(Deflating, SCRUB_DEFLATE_TIMEOUT, None, t, resume, 0),
            Done
        );
    }

    /// An abort must clear the scrub, re-arm the scrub cooldown (an abort-prone guest is not
    /// re-squeezed the moment the acute report passes), and trace a summarizer-invisible line.
    #[test]
    fn scrub_abort_arms_the_scrub_cooldown_and_traces() {
        let path = std::env::temp_dir().join(format!("scrub-abort-test-{}", std::process::id()));
        let now = Instant::now();
        let mut st = State {
            conn: None,
            target_pages: ROOM,
            last_change: None,
            cooldown_until: None,
            distress_until: None,
            scrub: Some(Scrub {
                phase: ScrubPhase::Inflating,
                phase_since: now,
                target_pages: ROOM,
                resume_pages: GIB_PAGES,
                gen: 3,
                last_actual_bytes: 0,
                stall_ticks: 0,
            }),
            scrub_gen: 3,
            last_scrub_end: now,
            last_sweep: now,
            gap: None,
            elastic_probe: None,
            inelastic: None,
            host_debounce: HostDebounce::new(),
            giveback_streak: 0,
            trace: Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap(),
            ),
        };
        abort_scrub(&mut st, now);
        assert!(st.scrub.is_none());
        assert!(!scrub_due(
            &scrub_params(ReclaimMode::Moderate),
            HostPressure::Warn,
            false,
            ROOM,
            0,
            None,
            st.last_scrub_end,
            now
        ));
        // A second abort with no scrub in flight is a no-op (no duplicate trace line).
        abort_scrub(&mut st, now);
        let out = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1, "{out}");
        assert!(
            lines[0].contains("\"scrub\":\"abort\"")
                && lines[0].contains("\"gen\":3")
                && lines[0].contains(&format!("\"resume_pages\":{GIB_PAGES}")),
            "{}",
            lines[0]
        );
        // Scrub lines must stay invisible to the bench summarizer, which keys on "decision".
        assert!(!out.contains("\"decision\""), "{out}");
    }

    /// The hold-transition line carries the reached fraction — "completed" vs "timed out at
    /// 60%" must be readable from the journal alone.
    #[test]
    fn scrub_trace_lines_carry_progress() {
        let path = std::env::temp_dir().join(format!("scrub-trace-test-{}", std::process::id()));
        let mut st = State {
            conn: None,
            target_pages: 0,
            last_change: None,
            cooldown_until: None,
            distress_until: None,
            scrub: None,
            scrub_gen: 0,
            last_scrub_end: Instant::now(),
            last_sweep: Instant::now(),
            gap: None,
            elastic_probe: None,
            inelastic: None,
            host_debounce: HostDebounce::new(),
            giveback_streak: 0,
            trace: Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap(),
            ),
        };
        trace_scrub(&mut st, "hold", 1, 0, Some(5 << 30), Some(93));
        let out = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(
            out.contains("\"scrub\":\"hold\"")
                && out.contains(&format!("\"actual_bytes\":{}", 5u64 << 30))
                && out.contains("\"reached_pct\":93"),
            "{out}"
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
