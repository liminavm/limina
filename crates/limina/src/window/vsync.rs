// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The supervisor's half of the host-driven vblank (`docs/design/host-vblank.md`): each guest
//! window measures the vsync grid of the screen it is on and states it to the worker as
//! `vsync id=<slot> …`, which puts it on the timing page the guest's driver ticks from.
//!
//! The window's display link (`NSWindow.displayLink`, which follows the window from screen to
//! screen) runs only in short bursts: the grid of a fixed-rate screen is a phase and a period, and
//! the period is pinned down far better by a long baseline between bursts than by watching every
//! frame. The baseline grows geometrically ([`GridEstimator`]) so that counting the frames between
//! two bursts can never round to the wrong number. Between updates nothing flows; a grid is sent
//! again only when the one the worker has would drift off the screen's by [`DRIFT_NS`], when the
//! window lands on another slot, or when a fresh worker has the device.
//!
//! The lead the guest's vblank goes ahead of the vsync by is measured ([`LeadEstimator`]): the
//! host's own processing of a frame, from the worker receiving the guest's flush to this window
//! committing it, plus the window server's composite deadline and a margin. Until a window has
//! committed frames it is [`DEFAULT_LEAD_NS`].
//!
//! `LIMINA_VSYNC_SOURCE=0` keeps the source quiet, for runs that state a grid of their own (the L2
//! test does).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use limina_displayctl::{DisplayCommand, VsyncGrid};
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::NSWindow;
use objc2_foundation::{NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes};
use objc2_quartz_core::CADisplayLink;

/// How far ahead of the host vsync the guest's vblank goes, until the lead is measured.
pub(crate) const DEFAULT_LEAD_NS: u32 = 4_000_000;
/// How long before the display link's target a layer commit must land to be shown at it, about
/// 1 ms clear of where commits start to miss (`spikes/composite-deadline`, macOS 26.6): a window…
const COMPOSITE_DEADLINE_NS: u64 = 1_000_000;
/// …and one covering its screen, which goes another way through the window server and misses
/// ~0.8 ms earlier. Measured natively fullscreen; the borderless cover is taken to be the same.
const COMPOSITE_DEADLINE_COVERING_NS: u64 = 2_000_000;
/// On top of the measured parts, for what the p95 does not see.
const LEAD_MARGIN_NS: u64 = 1_000_000;
/// A new lead moves every guest vblank. A longer one is stated once it is this much longer…
const LEAD_RISE_NS: u64 = 500_000;
/// …a shorter one only once it is this much shorter: a vsync missed costs a refresh, a lead too
/// long only latency.
const LEAD_FALL_NS: u64 = 1_500_000;
/// The host time statistic is taken over windows this long, once as each one ends. Re-read as
/// frames come and go, a p95 over a couple of seconds is the third-largest of sixty frames at
/// 30 fps and moves with every stall: measured, it restated the lead ~8 times a second.
const HOST_WINDOW: Duration = Duration::from_secs(10);
/// A window with fewer frames than this says nothing, and the last lead stands.
const HOST_MIN_SAMPLES: usize = 50;
/// A lead past this is a stalled host, not a deadline worth scheduling a guest around.
const LEAD_MAX_NS: u64 = 33_000_000;
/// How far the worker's grid may drift from the screen's before it is restated.
const DRIFT_NS: u64 = 50_000;
/// When bursts start, counted from the last reset: the baseline grows by at most ×4 a step until
/// the steady cadence, which keeps every step's frame count unambiguous (see [`GridEstimator`]).
const BURST_SCHEDULE: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(3),
    Duration::from_secs(12),
];
/// The steady cadence after the schedule.
const BURST_EVERY: Duration = Duration::from_secs(30);
/// A burst ends when it has this many vsyncs…
const BURST_SAMPLES: usize = 60;
/// …or after this long, whatever it has: an occluded window or a sleeping screen stops the link,
/// and then the last grid stands.
const BURST_TIMEOUT: Duration = Duration::from_secs(2);
/// Fewer vsyncs than this and a burst says nothing.
const BURST_MIN_SAMPLES: usize = 20;

/// `mach_absolute_time` ticks ↔ nanoseconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timebase {
    numer: u32,
    denom: u32,
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    fn mach_absolute_time() -> u64;
}

/// The host clock the display link and the worker's frame stamps are on.
pub(crate) fn mach_now() -> u64 {
    // SAFETY: no arguments, no state.
    unsafe { mach_absolute_time() }
}

/// The lead a window states with its grid (`docs/design/host-vblank.md`, §lead estimator):
/// `h + w + margin`, where `h` is the p95 of the host's own processing of a frame over the last
/// [`HOST_WINDOW`] and `w` the window server's composite deadline. Never a statistic of how early
/// frames arrive before the vsync they make: once the guest is locked on, that is the published
/// lead plus the compositor's own margin, and feeding it back ratchets the lead up.
#[derive(Debug)]
struct LeadEstimator {
    window_start: Option<Instant>,
    /// This window's host times, ns.
    samples: Vec<u64>,
    /// The p95 of the last window that had enough frames to say anything.
    host_p95: Option<u64>,
    published: u64,
    /// Whether `published` came from a measurement: the default gives way either way.
    measured: bool,
}

impl Default for LeadEstimator {
    fn default() -> Self {
        Self {
            window_start: None,
            samples: Vec::new(),
            host_p95: None,
            published: DEFAULT_LEAD_NS as u64,
            measured: false,
        }
    }
}

impl LeadEstimator {
    fn note(&mut self, now: Instant, host_ns: u64) {
        self.roll(now);
        if self.samples.len() < 8192 {
            self.samples.push(host_ns);
        }
    }

    /// Close the window if it has run its length.
    fn roll(&mut self, now: Instant) {
        let start = *self.window_start.get_or_insert(now);
        if now.duration_since(start) < HOST_WINDOW {
            return;
        }
        if self.samples.len() >= HOST_MIN_SAMPLES {
            self.samples.sort_unstable();
            self.host_p95 = Some(self.samples[(self.samples.len() * 95).div_ceil(100) - 1]);
        }
        self.samples.clear();
        self.window_start = Some(now);
    }

    /// The lead to state now, in ns: the default until a window has said something, then the
    /// last one's, moved only past [`LEAD_RISE_NS`] / [`LEAD_FALL_NS`].
    fn lead_ns(&mut self, now: Instant, covering: bool) -> u64 {
        self.roll(now);
        if let Some(h) = self.host_p95 {
            let w = if covering {
                COMPOSITE_DEADLINE_COVERING_NS
            } else {
                COMPOSITE_DEADLINE_NS
            };
            let lead = (h + w + LEAD_MARGIN_NS).min(LEAD_MAX_NS);
            if !self.measured
                || lead > self.published + LEAD_RISE_NS
                || lead + LEAD_FALL_NS < self.published
            {
                self.published = lead;
                self.measured = true;
            }
        }
        self.published
    }
}

impl Timebase {
    pub(crate) fn ns_from_ticks(self, ticks: u64) -> u64 {
        (ticks as u128 * self.numer as u128 / self.denom as u128) as u64
    }

    pub(crate) fn host() -> Self {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: fills the struct it is given.
        let ok = unsafe { mach_timebase_info(&mut info) } == 0 && info.numer > 0 && info.denom > 0;
        if ok {
            Self {
                numer: info.numer,
                denom: info.denom,
            }
        } else {
            // Apple silicon's: 24 MHz.
            Self {
                numer: 125,
                denom: 3,
            }
        }
    }

    pub(crate) fn ticks_from_ns(self, ns: u64) -> u64 {
        (ns as u128 * self.denom as u128 / self.numer as u128) as u64
    }

    /// A `CACurrentMediaTime`-style time (seconds of `mach_absolute_time`) in ticks.
    pub(crate) fn ticks_from_secs(self, secs: f64) -> u64 {
        (secs * 1e9 * self.denom as f64 / self.numer as f64).round() as u64
    }
}

/// One burst's reading: a vsync in it, de-jittered, and its period.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Reading {
    at: u64,
    period_fp: u64,
}

/// A burst's vsyncs (ticks, in order) as one de-jittered vsync and a period. `None` for a burst
/// too thin to trust, or whose period is off the link's nominal one by more than 1%.
fn read_burst(targets: &[u64], nominal: Option<u64>) -> Option<Reading> {
    if targets.len() < BURST_MIN_SAMPLES {
        return None;
    }
    let mut gaps: Vec<u64> = targets
        .windows(2)
        .filter_map(|w| w[1].checked_sub(w[0]))
        .filter(|&g| g > 0)
        .collect();
    if gaps.len() + 1 < BURST_MIN_SAMPLES {
        return None;
    }
    gaps.sort_unstable();
    let median = gaps[gaps.len() / 2] as f64;
    let first = targets[0];
    // Each callback's frame number from the median gap (coalesced callbacks show as 2× gaps and
    // simply skip a number), then the period as the least-squares slope of time over frame: every
    // sample's jitter counts, not just the two ends'.
    let points: Vec<(f64, f64)> = targets
        .iter()
        .map(|&t| {
            let dt = (t - first) as f64;
            ((dt / median).round(), dt)
        })
        .collect();
    let n = points.len() as f64;
    let (mk, mt) = points
        .iter()
        .fold((0.0, 0.0), |(a, b), &(k, t)| (a + k / n, b + t / n));
    let (sxy, sxx) = points.iter().fold((0.0, 0.0), |(a, b), &(k, t)| {
        (a + (k - mk) * (t - mt), b + (k - mk) * (k - mk))
    });
    if sxx <= 0.0 {
        return None;
    }
    let period_fp = (sxy / sxx * 4_294_967_296.0).round() as u64;
    if let Some(n) = nominal {
        let p = period_fp >> 32;
        if p.abs_diff(n) > n / 100 {
            return None;
        }
    }
    // A vsync at the burst's centroid, where the fit is exact whatever its slope's error (taken at
    // either end it would carry that error times half the burst): the frame nearest the mean.
    let at = first + (mt + (mk.round() - mk) * sxy / sxx).round() as u64;
    Some(Reading { at, period_fp })
}

/// A screen's vsync grid, refined across bursts.
///
/// The first burst gives the period to a frame's jitter over its own span (~1 s). Later bursts
/// refine it over the span from the epoch's first reading: the frames between the two are counted
/// with the period known so far, which is only safe while that period's error, times the frames
/// counted, stays well under half a period. The baseline therefore grows by at most
/// [`MAX_GROWTH`]× per refinement, and a count that leaves more than a quarter period unexplained
/// starts a new epoch instead (another screen, a rate change).
#[derive(Debug, Default)]
pub(crate) struct GridEstimator {
    epoch: Option<Reading>,
    period_fp: u64,
    /// The span the current period was refined over, in ticks.
    baseline: u64,
    latest: Option<u64>,
}

const MAX_GROWTH: u64 = 16;

impl GridEstimator {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Fold in one burst's vsyncs. Returns the grid as now known.
    fn burst(&mut self, targets: &[u64], nominal: Option<u64>) -> Option<(u64, u64)> {
        let reading = read_burst(targets, nominal)?;
        let span = targets.last().unwrap() - targets[0];
        let Some(epoch) = self.epoch else {
            self.start(reading, span);
            return self.grid();
        };
        let p = self.period_fp as u128;
        let base = reading.at.saturating_sub(epoch.at);
        let frames = ((base as u128) << 32) as f64 / p as f64;
        let n = frames.round() as u128;
        let residual = (((base as u128) << 32) as i128 - (n * p) as i128).unsigned_abs();
        let rate_moved = (reading.period_fp as u128).abs_diff(p) > p / 1000;
        if n == 0 || rate_moved || residual > p / 4 || base > self.baseline * MAX_GROWTH {
            if base > self.baseline * MAX_GROWTH && !rate_moved && residual <= p / 4 {
                // Too far to count safely, but still on the grid: keep the period, take the phase.
                self.latest = Some(reading.at);
                return self.grid();
            }
            self.start(reading, span);
            return self.grid();
        }
        self.period_fp = (((base as u128) << 32) / n) as u64;
        self.baseline = base;
        self.latest = Some(reading.at);
        self.grid()
    }

    fn start(&mut self, reading: Reading, span: u64) {
        self.epoch = Some(reading);
        self.period_fp = reading.period_fp;
        self.baseline = span.max(1);
        self.latest = Some(reading.at);
    }

    /// (a vsync, the period as 32.32 ticks), once a burst has been read.
    pub(crate) fn grid(&self) -> Option<(u64, u64)> {
        Some((self.latest?, self.period_fp))
    }
}

/// What the worker was last told for this window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sent {
    slot: usize,
    grid: VsyncGrid,
}

/// Whether the worker needs `grid` for `slot`, given what it was last told: yes for a new slot or
/// lead, or when its grid is `tol` ticks off this one now or would be within `horizon` ticks.
fn needs_send(sent: Option<Sent>, slot: usize, grid: VsyncGrid, tol: u64, horizon: u64) -> bool {
    let Some(s) = sent else {
        return true;
    };
    if s.slot != slot || s.grid.lead != grid.lead {
        return true;
    }
    let p = s.grid.period_fp as u128;
    let d = grid.at.abs_diff(s.grid.at) as u128;
    let k = (((d << 32) as f64) / p as f64).round() as u128;
    let phase = ((d << 32) as i128 - (k * p) as i128).unsigned_abs() >> 32;
    // A period difference becomes phase drift: the relative difference times the horizon.
    let drift = grid.period_fp.abs_diff(s.grid.period_fp) as u128 * horizon as u128 / p.max(1);
    phase as u64 > tol || drift as u64 > tol
}

pub(crate) struct VsyncIvars {
    targets: RefCell<Vec<f64>>,
    duration: RefCell<f64>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop; `step:` matches CADisplayLink's
    // target/selector convention (one argument, the link).
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaVsyncTarget"]
    #[ivars = VsyncIvars]
    pub(crate) struct VsyncTarget;

    unsafe impl NSObjectProtocol for VsyncTarget {}

    impl VsyncTarget {
        #[unsafe(method(step:))]
        fn step(&self, link: &CADisplayLink) {
            self.ivars().targets.borrow_mut().push(link.targetTimestamp());
            *self.ivars().duration.borrow_mut() = link.duration();
        }
    }
);

/// One guest window's vsync source.
pub(crate) struct VsyncSource {
    link: Option<Retained<CADisplayLink>>,
    target: Retained<VsyncTarget>,
    timebase: Timebase,
    estimator: GridEstimator,
    lead: LeadEstimator,
    sent: Option<Sent>,
    panel: Option<u64>,
    reset_at: Instant,
    bursts: usize,
    burst_started: Option<Instant>,
}

impl VsyncSource {
    pub(crate) fn new(window: &NSWindow, mtm: MainThreadMarker) -> Self {
        let target = VsyncTarget::alloc(mtm).set_ivars(VsyncIvars {
            targets: RefCell::new(Vec::new()),
            duration: RefCell::new(0.0),
        });
        // SAFETY: NSObject's init.
        let target: Retained<VsyncTarget> = unsafe { msg_send![super(target), init] };
        let enabled = std::env::var("LIMINA_VSYNC_SOURCE").map_or(true, |v| v != "0");
        let link = enabled.then(|| {
            // SAFETY: `target` implements `step:` taking the link.
            let link = unsafe { window.displayLinkWithTarget_selector(&target, sel!(step:)) };
            link.setPaused(true);
            // Common modes, like the render timer: the default mode alone stalls through live
            // resizes and menu tracking, and a burst that never ends is a grid never sent.
            // SAFETY: the main run loop, on the main thread.
            unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
            link
        });
        Self {
            link,
            target,
            timebase: Timebase::host(),
            estimator: GridEstimator::default(),
            lead: LeadEstimator::default(),
            sent: None,
            panel: None,
            reset_at: Instant::now(),
            bursts: 0,
            burst_started: None,
        }
    }

    /// A fresh worker has the device and knows no grid: say it again.
    pub(crate) fn forget_sent(&mut self) {
        self.sent = None;
    }

    /// This window committed, at `committing` (mach ticks, taken just before the layer commit),
    /// a frame that reached the worker at `arrived`. The commit call itself is the composite
    /// deadline's, which the spike measured from the same point.
    pub(crate) fn note_commit(&mut self, arrived: u64, committing: u64, now: Instant) {
        let Some(ticks) = committing.checked_sub(arrived) else {
            return;
        };
        let host_ns = self.timebase.ns_from_ticks(ticks);
        if log::log_enabled!(log::Level::Trace) {
            log::trace!(
                "vsync: frame committing {} µs after it reached the worker, the commit took {} µs",
                host_ns / 1000,
                self.timebase
                    .ns_from_ticks(mach_now().saturating_sub(committing))
                    / 1000
            );
        }
        self.lead.note(now, host_ns);
    }

    /// Advance this window's bursts; the command to send, if the worker needs one. `slot` is the
    /// guest display the window shows, `panel` the screen it is on, `covering` whether the window
    /// covers that screen.
    pub(crate) fn poll(
        &mut self,
        slot: usize,
        panel: Option<u64>,
        covering: bool,
        now: Instant,
    ) -> Option<DisplayCommand> {
        let link = self.link.as_ref()?;
        if panel != self.panel {
            self.panel = panel;
            self.estimator.reset();
            self.reset_at = now;
            self.bursts = 0;
        }
        match self.burst_started {
            None if now >= self.next_burst() && panel.is_some() => {
                self.target.ivars().targets.borrow_mut().clear();
                link.setPaused(false);
                self.burst_started = Some(now);
            }
            Some(started) => {
                let have = self.target.ivars().targets.borrow().len();
                if have >= BURST_SAMPLES || now.duration_since(started) >= BURST_TIMEOUT {
                    link.setPaused(true);
                    self.burst_started = None;
                    self.bursts += 1;
                    let tb = self.timebase;
                    let targets: Vec<u64> = self
                        .target
                        .ivars()
                        .targets
                        .borrow()
                        .iter()
                        .map(|&s| tb.ticks_from_secs(s))
                        .collect();
                    let duration = *self.target.ivars().duration.borrow();
                    let nominal = (duration > 0.0).then(|| tb.ticks_from_secs(duration));
                    if self.estimator.burst(&targets, nominal).is_none() {
                        log::debug!("vsync: burst of {} vsyncs read nothing", targets.len());
                    }
                }
            }
            None => {}
        }
        let (at, period_fp) = self.estimator.grid()?;
        let lead_ns = self.lead.lead_ns(now, covering);
        let grid = VsyncGrid {
            at,
            period_fp,
            lead: self.timebase.ticks_from_ns(lead_ns),
            lead_ns: lead_ns as u32,
        };
        let tol = self.timebase.ticks_from_ns(DRIFT_NS);
        let minute = self.timebase.ticks_from_ns(60_000_000_000);
        if !needs_send(self.sent, slot, grid, tol, minute) {
            return None;
        }
        log::info!(
            "vsync: guest display {slot} on the host grid, {:.4} Hz, lead {} µs",
            1e9 / (self.timebase.ns_from_ticks(period_fp >> 32) as f64),
            lead_ns / 1000
        );
        self.sent = Some(Sent { slot, grid });
        Some(DisplayCommand::Vsync {
            display_id: slot as u32,
            grid: Some(grid),
        })
    }

    fn next_burst(&self) -> Instant {
        match BURST_SCHEDULE.get(self.bursts) {
            Some(&after) => self.reset_at + after,
            None => {
                self.reset_at
                    + BURST_SCHEDULE[BURST_SCHEDULE.len() - 1]
                    + BURST_EVERY * (self.bursts + 1 - BURST_SCHEDULE.len()) as u32
            }
        }
    }
}

impl Drop for VsyncSource {
    fn drop(&mut self) {
        // The link holds its target strongly until it is invalidated.
        if let Some(link) = &self.link {
            link.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// Frames at 30 fps from `from` for `secs`, each with host time `h(i)`.
    fn frames(
        l: &mut LeadEstimator,
        t0: Instant,
        from: Duration,
        secs: u64,
        h: impl Fn(u64) -> u64,
    ) {
        for i in 0..secs * 30 {
            l.note(t0 + from + Duration::from_micros(i * 33_333), h(i));
        }
    }

    #[test]
    fn the_lead_is_the_default_until_a_window_says_otherwise() {
        let mut l = LeadEstimator::default();
        let t0 = Instant::now();
        frames(&mut l, t0, Duration::ZERO, 9, |_| 9 * MS);
        assert_eq!(
            l.lead_ns(t0 + Duration::from_secs(9), false),
            DEFAULT_LEAD_NS as u64,
            "the first window has not ended"
        );
        let mut l = LeadEstimator::default();
        for i in 0..HOST_MIN_SAMPLES as u64 - 1 {
            l.note(t0 + Duration::from_millis(i * 100), 9 * MS);
        }
        assert_eq!(
            l.lead_ns(t0 + HOST_WINDOW, false),
            DEFAULT_LEAD_NS as u64,
            "too few frames in the window"
        );
    }

    #[test]
    fn the_lead_is_the_host_p95_plus_the_deadline_and_margin() {
        let mut l = LeadEstimator::default();
        let t0 = Instant::now();
        // Every 17th frame at 3 ms, the rest at 0.5 ms: 18 of 300, so the p95 is a slow one.
        frames(&mut l, t0, Duration::ZERO, 10, |i| {
            if i % 17 == 0 { 3 * MS } else { MS / 2 }
        });
        let now = t0 + HOST_WINDOW;
        assert_eq!(
            l.lead_ns(now, false),
            3 * MS + COMPOSITE_DEADLINE_NS + LEAD_MARGIN_NS
        );
        assert_eq!(
            l.lead_ns(now, true),
            3 * MS + COMPOSITE_DEADLINE_COVERING_NS + LEAD_MARGIN_NS,
            "covering the screen moves the deadline by more than a rise"
        );
    }

    #[test]
    fn the_lead_rises_readily_falls_reluctantly_and_holds_when_idle() {
        let mut l = LeadEstimator::default();
        let t0 = Instant::now();
        let window = |l: &mut LeadEstimator, n: u32, h: u64| {
            frames(l, t0, HOST_WINDOW * n, 10, |_| h);
            l.lead_ns(t0 + HOST_WINDOW * (n + 1), false)
        };
        let first = window(&mut l, 0, MS);
        assert_eq!(first, 3 * MS);
        assert_eq!(
            window(&mut l, 1, MS + 4 * MS / 10),
            first,
            "0.4 ms longer: stays"
        );
        assert_eq!(
            window(&mut l, 2, MS + 6 * MS / 10),
            first + 6 * MS / 10,
            "0.6 ms longer"
        );
        assert_eq!(
            window(&mut l, 3, MS / 2),
            first + 6 * MS / 10,
            "1.1 ms shorter: stays"
        );
        assert_eq!(window(&mut l, 4, 0), 2 * MS, "1.6 ms shorter");
        // No frames for a while: the last lead stands.
        assert_eq!(l.lead_ns(t0 + HOST_WINDOW * 20, false), 2 * MS);
    }

    /// The measured host times have a heavy tail (p50 1.6 ms, p99 7 ms, max 18 ms with the
    /// aquarium): polled at 60 Hz for a minute, the lead must not follow it around.
    #[test]
    fn a_noisy_tail_restates_the_lead_rarely() {
        let mut l = LeadEstimator::default();
        let t0 = Instant::now();
        let mut published = vec![l.published];
        let mut x = 12345u64;
        for tick in 0..60 * 60u64 {
            let now = t0 + Duration::from_micros(tick * 16_667);
            if tick % 2 == 0 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let r = x >> 33;
                let h = match r % 100 {
                    0 => 18 * MS,
                    1..=4 => 4 * MS + (r % 3_000_000),
                    _ => MS / 2 + (r % 2_000_000),
                };
                l.note(now, h);
            }
            let lead = l.lead_ns(now, false);
            if *published.last().unwrap() != lead {
                published.push(lead);
            }
        }
        assert!(
            published.len() <= 4,
            "the lead was restated as {published:?}"
        );
    }

    #[test]
    fn a_stalled_host_does_not_set_a_lead_past_the_cap() {
        let mut l = LeadEstimator::default();
        let t0 = Instant::now();
        frames(&mut l, t0, Duration::ZERO, 10, |_| 500 * MS);
        assert_eq!(l.lead_ns(t0 + HOST_WINDOW, false), LEAD_MAX_NS);
    }

    /// 59.9502 Hz at 24 MHz.
    const TRUE_PERIOD: f64 = 24_000_000.0 / 59.9502;
    const MINUTE: u64 = 60 * 24_000_000;

    fn fp(p: f64) -> u64 {
        (p * 4_294_967_296.0).round() as u64
    }

    /// A burst of `n` link callbacks from frame `k0`, with deterministic jitter up to ±720 ticks
    /// (30 µs) and every 7th callback coalesced into the next.
    fn burst(k0: u64, n: u64) -> Vec<u64> {
        (k0..k0 + n)
            .filter(|k| k % 7 != 3)
            .map(|k| {
                let jitter = ((k * 7919) % 1441) as i64 - 720;
                (1_000_000_000.0 + k as f64 * TRUE_PERIOD).round() as u64 + (jitter + 720) as u64
                    - 720
            })
            .collect()
    }

    fn ppm(period_fp: u64) -> f64 {
        (period_fp as f64 / 4_294_967_296.0 - TRUE_PERIOD) / TRUE_PERIOD * 1e6
    }

    #[test]
    fn the_schedule_refines_the_period_without_miscounting() {
        let mut e = GridEstimator::default();
        let nominal = Some(400_000);
        // Bursts at 0 s, 3 s, 12 s, then every 30 s, ~1 s each.
        let starts = [0u64, 180, 720, 2520, 4320, 6120, 7920];
        let mut last_ppm = f64::MAX;
        for (i, &k0) in starts.iter().enumerate() {
            let (_, p) = e.burst(&burst(k0, 60), nominal).expect("a grid");
            last_ppm = ppm(p).abs();
            eprintln!(
                "burst {i}: {:+.4} ppm, baseline {} ticks",
                ppm(p),
                e.baseline
            );
            // A frame miscounted at these baselines is over 100 ppm.
            assert!(last_ppm < 50.0, "burst {i} miscounted: {last_ppm} ppm");
        }
        // 0.3 ppm drifts under 10 µs a minute, against a 50 µs tolerance.
        assert!(last_ppm < 0.3, "the ladder ends at {last_ppm} ppm");
        let (at, p) = e.grid().unwrap();
        // The phase is a vsync of the true grid.
        let k = ((at as f64 - 1_000_000_000.0) / TRUE_PERIOD).round();
        let off = at as f64 - (1_000_000_000.0 + k * TRUE_PERIOD);
        assert!(off.abs() < 240.0, "the phase is {off} ticks off the grid");
        assert_eq!(p, e.period_fp);
    }

    #[test]
    fn a_jump_past_safe_counting_keeps_the_period_and_takes_the_phase() {
        let mut e = GridEstimator::default();
        e.burst(&burst(0, 60), Some(400_000)).unwrap();
        let before = e.period_fp;
        // 300 s later: 300× the first baseline, past MAX_GROWTH.
        let (at, p) = e.burst(&burst(18_000, 60), Some(400_000)).unwrap();
        assert_eq!(p, before, "the period was refined over an unsafe count");
        assert!(at > 18_000 * 400_000);
    }

    #[test]
    fn another_screen_starts_a_new_epoch() {
        let mut e = GridEstimator::default();
        e.burst(&burst(0, 60), Some(400_000)).unwrap();
        // A 120 Hz screen: half the period.
        let fast: Vec<u64> = (0..120u64).map(|k| 3_000_000_000 + k * 200_000).collect();
        let (_, p) = e.burst(&fast, Some(200_000)).unwrap();
        assert_eq!(p >> 32, 200_000);
    }

    #[test]
    fn a_thin_or_implausible_burst_says_nothing() {
        assert!(read_burst(&burst(0, 10), None).is_none());
        assert!(
            read_burst(&burst(0, 60), Some(300_000)).is_none(),
            "25% off nominal"
        );
    }

    #[test]
    fn the_worker_is_told_only_what_it_does_not_know() {
        let grid = |at: u64, p: f64| VsyncGrid {
            at,
            period_fp: fp(p),
            lead: 96_000,
            lead_ns: 4_000_000,
        };
        let g = grid(1_000_000, 400_333.6);
        assert!(needs_send(None, 0, g, 1200, MINUTE));
        let sent = Some(Sent { slot: 0, grid: g });
        // The same grid, read a thousand frames later.
        let later = 1_000_000 + ((fp(400_333.6) as u128 * 1000) >> 32) as u64;
        assert!(!needs_send(sent, 0, grid(later, 400_333.6), 1200, MINUTE));
        assert!(needs_send(sent, 1, g, 1200, MINUTE), "another slot");
        assert!(
            needs_send(sent, 0, grid(1_000_000 + 5_000, 400_333.6), 1200, MINUTE),
            "phase moved"
        );
        // 1 ppm of period is 0.4 ticks a frame: 1440 ticks over a minute.
        assert!(
            needs_send(sent, 0, grid(1_000_000, 400_334.0), 1200, MINUTE),
            "period drifts"
        );
        assert!(
            !needs_send(sent, 0, grid(1_000_000, 400_333.601), 1200, MINUTE),
            "a rounding's difference"
        );
    }
}
