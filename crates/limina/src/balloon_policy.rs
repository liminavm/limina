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
const DWELL: Duration = Duration::from_millis(800);
/// Ignore target changes smaller than this (pages; 16 MiB) — anti-dribble dead band.
const DEAD_BAND_PAGES: u32 = 4096;

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
    /// warn leave the guest 25% of max as cache; full squeeze only when the host is critical.
    Light,
    /// Host-pressure-driven (the default): while the host is fine leave the guest 12.5% of max
    /// (min 1 GiB) as cache; under host warn/critical squeeze to the floor.
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

/// Read the host's memory-pressure level (1 = normal, 2 = warn, 4 = critical). Errors read as
/// Normal: the host kernel manages its own pressure, and "don't squeeze the guest" is the safe
/// default for guest performance.
pub fn read_host_pressure() -> HostPressure {
    let mut level: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let name = c"kern.memorystatus_vm_pressure_level";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut level as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    match (rc, level) {
        (0, l) if l >= 4 => HostPressure::Critical,
        (0, 2..=3) => HostPressure::Warn,
        _ => HostPressure::Normal,
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
}

impl BalloonPolicy {
    pub fn new(min_pages: u32, max_pages: u32, mode: ReclaimMode, socket: PathBuf) -> Self {
        Self {
            min_pages,
            max_pages,
            mode,
            socket,
            state: Mutex::new(State {
                conn: None,
                target_pages: 0,
                last_change: None,
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
        let host = read_host_pressure();
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        let inputs = DecideInputs {
            mode: self.mode,
            host,
            current: st.target_pages,
            room,
            max_pages: self.max_pages,
            last_change: st.last_change,
            now,
        };
        let Some(new_target) = decide(p, &inputs) else {
            return;
        };
        if self.send_target(&mut st, new_target) {
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
    now: Instant,
}

/// The cache allowance for a mode/host-pressure pair: how much of the guest's memory the policy
/// must leave available (as page cache) when inflating. `None` = do not hold any balloon at all
/// in this state (drift the target to 0). Numbers from `spikes/mem-overhead-2026-07-02` Run D:
/// even a few hundred MiB of cache keeps small hot sets at ~1 µs hits; a full squeeze costs 64×
/// on warm random reads.
fn allowance_pages(mode: ReclaimMode, host: HostPressure, max_pages: u32) -> Option<u32> {
    const GIB: u32 = 1024 * PAGES_PER_MIB;
    match (mode, host) {
        (ReclaimMode::Disabled, _) => None, // not reached: policy isn't constructed
        (ReclaimMode::Light, HostPressure::Normal) => None,
        (ReclaimMode::Light, HostPressure::Warn) => Some((max_pages / 4).max(2 * GIB)),
        (ReclaimMode::Light, HostPressure::Critical) => Some(0),
        (ReclaimMode::Moderate, HostPressure::Normal) => Some((max_pages / 8).max(GIB)),
        (ReclaimMode::Moderate, _) => Some(0),
        (ReclaimMode::Aggressive, _) => Some(0),
    }
}

/// The pure policy decision (unit-tested): given a pressure report and the current state, return
/// the next target in pages, or `None` to hold.
///
/// All modes release to 0 immediately when the *guest* is under pressure. Inflation requires an
/// idle guest and is bounded by the mode's cache [`allowance_pages`]: the balloon may only take
/// what the guest has available *beyond* the allowance, so the guest keeps that much room for
/// page cache. Aggressive keeps the original M6 shape exactly (squeeze to the floor while ≥30%
/// is available, host pressure ignored). Deflation (target shrinks) is immediate; inflation is
/// stepped (¼ of room) and dwell-limited; sub-dead-band changes are held to avoid dribble.
fn decide(p: &MemPressure, i: &DecideInputs) -> Option<u32> {
    // Guest under pressure: hand memory back, now. All modes.
    if p.some_avg10 >= PRESSURE_HIGH {
        return (i.current != 0).then_some(0);
    }
    // Inflating (or trimming toward an allowance) needs an idle guest.
    if p.some_avg10 > PRESSURE_LOW {
        return None; // neutral band: hold (hysteresis)
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
                return None;
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
        return None;
    }
    // Deflation is immediate; inflation is gradual and rate-limited.
    let next = if desired <= i.current {
        desired
    } else {
        if let Some(t) = i.last_change {
            if i.now.duration_since(t) < DWELL {
                return None;
            }
        }
        let step = (i.room / 4).max(1);
        i.current.saturating_add(step).min(desired)
    };
    (next != i.current).then_some(next)
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
                assert_eq!(next, Some(0), "{mode:?}/{host:?}");
            }
        }
    }

    #[test]
    fn aggressive_keeps_the_original_shape() {
        // Idle with ≥30% available: one ¼-room step toward full, regardless of host pressure.
        let i = inputs(ReclaimMode::Aggressive, HostPressure::Normal, 0);
        let next = decide(&report_pages(0, MAX * 7 / 10, MAX), &i);
        assert_eq!(next, Some(ROOM / 4));
        // Idle but <30% available: hold.
        let next = decide(&report_pages(0, MAX / 10, MAX), &i);
        assert_eq!(next, None);
        // Dwell gates inflation.
        let mut i = inputs(ReclaimMode::Aggressive, HostPressure::Critical, ROOM / 4);
        i.last_change = Some(i.now);
        let next = decide(&report_pages(0, MAX * 7 / 10, MAX), &i);
        assert_eq!(next, None);
    }

    #[test]
    fn neutral_band_holds() {
        // some=5% is between LOW(2%) and HIGH(10%) -> hold, every mode.
        for mode in [
            ReclaimMode::Light,
            ReclaimMode::Moderate,
            ReclaimMode::Aggressive,
        ] {
            let i = inputs(mode, HostPressure::Normal, ROOM / 4);
            let next = decide(&report_pages(500, MAX / 2, MAX), &i);
            assert_eq!(next, None, "{mode:?}");
        }
    }

    #[test]
    fn moderate_normal_leaves_the_cache_allowance() {
        // Guest idle with 4 GiB available; allowance = max/8 = 1 GiB. The balloon may take
        // avail − allowance = 3 GiB, stepped by ¼ room.
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 0);
        let next = decide(&report_pages(0, 4 * GIB_PAGES, MAX), &i);
        let desired = 3 * GIB_PAGES;
        assert_eq!(next, Some(desired.min(ROOM / 4)));
        // Fully converged: current already at avail − allowance → hold (sub-dead-band).
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 3 * GIB_PAGES);
        let next = decide(&report_pages(0, GIB_PAGES, MAX), &i);
        assert_eq!(next, None);
    }

    #[test]
    fn moderate_gives_cache_back_when_guest_dips_below_allowance() {
        // Guest available fell to 256 MiB (< 1 GiB allowance) while idle: deflate by the
        // shortfall, immediately (no dwell on deflation).
        let mut i = inputs(ReclaimMode::Moderate, HostPressure::Normal, 3 * GIB_PAGES);
        i.last_change = Some(i.now);
        let next = decide(&report_pages(0, GIB_PAGES / 4, MAX), &i);
        assert_eq!(next, Some(3 * GIB_PAGES - (GIB_PAGES - GIB_PAGES / 4)));
    }

    #[test]
    fn moderate_squeezes_fully_under_host_pressure() {
        // Host warn → allowance 0: with 4 GiB available the desired target is current+avail,
        // stepped; from 0 that's one ¼-room step.
        let i = inputs(ReclaimMode::Moderate, HostPressure::Warn, 0);
        let next = decide(&report_pages(0, 4 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Some(ROOM / 4));
    }

    #[test]
    fn light_normal_holds_no_balloon() {
        // Host fine → Light drifts the target to 0 (give the guest its cache back)…
        let i = inputs(ReclaimMode::Light, HostPressure::Normal, 2 * GIB_PAGES);
        let next = decide(&report_pages(0, 2 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Some(0));
        // …and stays at 0.
        let i = inputs(ReclaimMode::Light, HostPressure::Normal, 0);
        let next = decide(&report_pages(0, 4 * GIB_PAGES, MAX), &i);
        assert_eq!(next, None);
    }

    #[test]
    fn light_engages_under_host_warn_with_a_generous_allowance() {
        // Host warn → allowance max/4 = 2 GiB; guest has 4 GiB available → may take 2 GiB.
        let i = inputs(ReclaimMode::Light, HostPressure::Warn, 0);
        let next = decide(&report_pages(0, 4 * GIB_PAGES, MAX), &i);
        assert_eq!(next, Some((2 * GIB_PAGES).min(ROOM / 4)));
    }

    #[test]
    fn light_critical_squeezes_to_the_floor() {
        let i = inputs(ReclaimMode::Light, HostPressure::Critical, ROOM - ROOM / 4);
        let next = decide(&report_pages(0, 2 * GIB_PAGES, MAX), &i);
        // allowance 0: desired = current + avail, capped at room; one step away.
        assert_eq!(next, Some(ROOM));
    }

    #[test]
    fn dead_band_swallows_dribble() {
        // A 8 MiB adjustment (< 16 MiB dead band) is held.
        let cur = 2 * GIB_PAGES;
        let i = inputs(ReclaimMode::Moderate, HostPressure::Normal, cur);
        let next = decide(&report_pages(0, GIB_PAGES + 2048, MAX), &i);
        assert_eq!(next, None);
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
    }
}
