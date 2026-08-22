// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Each display's share of the one absolute device, **learned from the guest's cursor echo**.
//!
//! The guest spreads its single absolute pointer device over the bounding box of every monitor
//! it has, so the value we send names a point on the whole desktop, never on one display:
//! `logical_x = value / ABS_MAX × union_width + …`. To put the guest's cursor on a particular
//! pixel of a particular display we therefore need that display's offset and extent inside the
//! union — and on a stock guest nothing tells us either. Sending the window's own unit position
//! straight to the range (the identity mapping) is exact for one display, whose share *is* the
//! whole range, and wrong for every display once there are two: measured on the two-panel rig
//! 2026-08-21, a host pointer at the BenQ's centre put the guest's cursor 88% of the way across
//! it, and past 57.5% of *either* window the guest's cursor was on the other monitor entirely.
//!
//! We do not have to know the union, the offsets or the guest's scales separately. For the slot
//! the guest's cursor lands on, the whole relation is one line per axis in the value we sent:
//!
//! ```text
//! pixel = a · u + b      where  a = union × scale,  b = −offset × scale
//! ```
//!
//! so placing the cursor on `pixel` is `u = (pixel − b) / a`. Two well-separated samples per
//! axis determine it, and **every motion is already a sample**: we send `u`, the guest echoes
//! back which scanout its cursor plane is on and at which pixel (`window/echo.rs`). The same
//! comparison that used to only complain (`guest pointer:` warnings) is the measurement.
//!
//! Fitting continuously rather than probing once is what makes this safe without a
//! notification: the guest never tells us its monitors moved, and virtio-gpu has no way to.
//! A monitor *connected*, *disconnected* or *re-moded* reaches us as `surface`/`scanoutgone`
//! and drops every fit (one slot's mode changes the union, so it changes every other slot's
//! line too). A monitor **repositioned**, or its **scale** changed, produces nothing on the
//! wire at all — mutter's fractional scale on a fixed-mode connector leaves the scanout
//! byte-identical while the union and every offset move. Those announce themselves here: the
//! next samples miss the line, and after [`CONTRADICTIONS`] of them the line is dropped and
//! refitted from the contradicting samples. The cost is a stroke or two placed by the old
//! mapping, against an event we would otherwise never receive.
//!
//! Scope: the **uncaptured** path with more than one live scanout. One display needs no fit
//! (identity is exact), a guest that reports its layout is better served by the report
//! (`arrangement::abs_through_report` — exact and immediate), and the captured path keeps its
//! own mechanism: a running position in the device range that follows the guest's echo across
//! seams (`input::captured_step_and_emit`).

use std::sync::Mutex;

use super::echo::CursorEcho;
use super::present::MAX_SCANOUTS;

/// How far apart in the device range two samples must lie before a line drawn through them is
/// trusted. Under a tenth of the range the lever is too short: a pixel of echo rounding at each
/// end swings the slope by enough to place the cursor on the wrong monitor.
pub(crate) const MIN_SPREAD: f64 = 0.08;

/// How many samples one axis keeps. Enough that a fit averages out echo rounding, few enough
/// that a stale arrangement cannot outvote the samples taken since it changed.
pub(crate) const RING: usize = 12;

/// Samples this close together in the device range describe the same place; the newer one
/// replaces the older rather than crowding the ring and shrinking its spread.
pub(crate) const NEAR_U: f64 = 0.004;

/// How far off its line a sample may fall before it counts against the fit, in guest pixels.
/// Well above the honest miss (the guest rounds our position into logical coordinates and the
/// plane back into device pixels, a pixel or two at each step) and far below the faults this
/// exists to catch, which move the cursor by a fraction of a display at least.
pub(crate) const TOLERANCE_PX: f64 = 12.0;

/// How many samples in a row must miss the line before it is abandoned. One is echo lag or a
/// sample taken mid-stroke; three in a row is the arrangement.
pub(crate) const CONTRADICTIONS: usize = 3;

/// One axis of one slot's mapping: the guest pixel as a function of the value we sent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Line {
    pub(crate) a: f64,
    pub(crate) b: f64,
}

impl Line {
    /// The guest pixel this line puts a sent value at.
    pub(crate) fn pixel(&self, u: f64) -> f64 {
        self.a * u + self.b
    }

    /// The value to send to land on `pixel`; `None` for a degenerate line.
    pub(crate) fn unit(&self, pixel: f64) -> Option<f64> {
        (self.a.abs() > f64::EPSILON).then(|| (pixel - self.b) / self.a)
    }
}

/// One observation: we sent `u` on the device, the guest put its cursor at `pixel`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub(crate) u: f64,
    pub(crate) pixel: f64,
}

/// Least squares through the samples, refused unless the result can be a display's share of
/// the range.
///
/// `extent` is the slot's own size on that axis in guest pixels. The device covers the whole
/// desktop and this display is part of it, so a full sweep of the range must cross at least
/// this display: `a >= extent`. A slope under that is a fit through samples that do not belong
/// to one mapping — a rearrangement caught mid-relearn, or a ring straddling one. Better no
/// mapping (identity, wrong but stable) than a mapping we know cannot be true.
pub(crate) fn fit(samples: &[Sample], extent: f64) -> Option<Line> {
    if samples.len() < 2 {
        return None;
    }
    let lo = samples.iter().fold(f64::INFINITY, |m, s| m.min(s.u));
    let hi = samples.iter().fold(f64::NEG_INFINITY, |m, s| m.max(s.u));
    if hi - lo < MIN_SPREAD {
        return None;
    }
    let n = samples.len() as f64;
    let su: f64 = samples.iter().map(|s| s.u).sum();
    let sp: f64 = samples.iter().map(|s| s.pixel).sum();
    let suu: f64 = samples.iter().map(|s| s.u * s.u).sum();
    let sup: f64 = samples.iter().map(|s| s.u * s.pixel).sum();
    let den = n * suu - su * su;
    if den.abs() < 1e-9 {
        return None;
    }
    let a = (n * sup - su * sp) / den;
    let b = (sp - a * su) / n;
    (a.is_finite() && b.is_finite() && a >= extent * 0.9).then_some(Line { a, b })
}

/// One axis's samples and the line through them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Axis {
    samples: Vec<Sample>,
    /// Samples that missed the current line, held until there are enough to overturn it.
    pending: Vec<Sample>,
    line: Option<Line>,
}

impl Axis {
    const fn new() -> Self {
        Axis {
            samples: Vec::new(),
            pending: Vec::new(),
            line: None,
        }
    }

    pub(crate) fn line(&self) -> Option<Line> {
        self.line
    }

    /// Take one observation. A sample that agrees with the line refines it; a run of samples
    /// that contradict it replaces it.
    pub(crate) fn observe(&mut self, s: Sample, extent: f64) {
        // A pointer the guest is CLAMPING measures nothing. Ride a guest desktop edge while
        // captured and the sends keep changing while the echoed pixel sits at the boundary —
        // a run of samples that vary in `u` and not in `pixel`. They are not evidence about
        // the line; they are evidence that this axis has run out of display. Fed in, they
        // contradict a perfectly good line, reseed a fit too flat to believe, and so cost the
        // slot its mapping — which is exactly what asks for a sweep, mid-stroke. The mapping
        // is linear only in the interior, so only the interior is sampled.
        if s.pixel <= 0.0 || s.pixel >= extent - 1.0 {
            return;
        }
        if let Some(l) = self.line {
            if (l.pixel(s.u) - s.pixel).abs() > TOLERANCE_PX {
                self.pending.push(s);
                if self.pending.len() >= CONTRADICTIONS {
                    // The arrangement moved: everything learned under the old one is worthless,
                    // and the samples that proved it are the seed of the new fit.
                    let seed = std::mem::take(&mut self.pending);
                    match fit(&seed, extent) {
                        Some(l) => {
                            self.samples = seed;
                            self.line = Some(l);
                        }
                        // Enough to doubt the old line, not enough to state a new one. Keep
                        // the line: a wrong-but-stable mapping beats none, because none is
                        // what makes the slot incomplete and summons a sweep. The dissenters
                        // stay pending, so the next contradiction is judged on a wider base
                        // and the arrangement that really did move still wins, a little later.
                        None => self.pending = seed,
                    }
                }
                return;
            }
        }
        self.pending.clear();
        self.push(s);
        self.line = fit(&self.samples, extent);
    }

    fn push(&mut self, s: Sample) {
        if let Some(e) = self.samples.iter_mut().find(|e| (e.u - s.u).abs() < NEAR_U) {
            *e = s;
            return;
        }
        if self.samples.len() == RING {
            self.samples.remove(0);
        }
        self.samples.push(s);
    }
}

/// Both axes of one slot.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Fit {
    pub(crate) x: Axis,
    pub(crate) y: Axis,
}

impl Fit {
    const fn new() -> Self {
        Fit {
            x: Axis::new(),
            y: Axis::new(),
        }
    }

    fn ready(&self) -> bool {
        self.x.line().is_some() && self.y.line().is_some()
    }
}

static FITS: Mutex<[Fit; MAX_SCANOUTS]> = Mutex::new([const { Fit::new() }; MAX_SCANOUTS]);

/// The guest echoed a cursor position for a value we sent: one sample, on the slot the guest
/// actually used.
///
/// `device` is what we put on the wire, normalised to `0.0..=1.0`. The slot is the guest's
/// answer, not ours — the whole point is that the two can differ, and a sample is about
/// wherever the cursor really went. A sprite showing on more than one plane at once is
/// ambiguous about which pixel is the pointer, so it is not sampled.
pub(crate) fn observe(device: (f64, f64), echo: &[CursorEcho; MAX_SCANOUTS]) {
    let shown: Vec<usize> = echo
        .iter()
        .enumerate()
        .filter(|(_, c)| c.visible && c.w > 0 && c.h > 0)
        .map(|(s, _)| s)
        .collect();
    let [slot] = shown[..] else {
        return;
    };
    let e = echo[slot];
    let mut fits = FITS.lock().unwrap();
    let Some(f) = fits.get_mut(slot) else {
        return;
    };
    let was = f.ready();
    f.x.observe(
        Sample {
            u: device.0,
            pixel: f64::from(e.x),
        },
        f64::from(e.w),
    );
    f.y.observe(
        Sample {
            u: device.1,
            pixel: f64::from(e.y),
        },
        f64::from(e.h),
    );
    match (was, f.ready()) {
        (false, true) => {
            let (x, y) = (f.x.line().unwrap(), f.y.line().unwrap());
            log::info!(
                "display: slot {slot} takes the absolute device as x = {:.0}·u {:+.0}, y = {:.0}·v {:+.0} — learned from the guest's cursor echo",
                x.a,
                x.b,
                y.a,
                y.b
            );
        }
        (true, false) => log::info!(
            "display: slot {slot}'s share of the absolute device stopped matching the guest — relearning"
        ),
        _ => {}
    }
}

/// The value to send so the guest's cursor lands on `unit` of `slot`'s own scanout, or `None`
/// while that slot's mapping is not known.
pub(crate) fn place(
    slot: usize,
    unit: (f64, f64),
    scanout: (u32, u32),
    abs_max: i32,
) -> Option<(i32, i32)> {
    if scanout.0 == 0 || scanout.1 == 0 {
        return None;
    }
    let fits = FITS.lock().unwrap();
    let f = fits.get(slot)?;
    let u =
        f.x.line?
            .unit(unit.0.clamp(0.0, 1.0) * f64::from(scanout.0))?;
    let v =
        f.y.line?
            .unit(unit.1.clamp(0.0, 1.0) * f64::from(scanout.1))?;
    let range = f64::from(abs_max);
    Some((
        (u.clamp(0.0, 1.0) * range).round() as i32,
        (v.clamp(0.0, 1.0) * range).round() as i32,
    ))
}

/// Where to put the guest's pointer for a unit position on one slot: the guest's own report
/// when it makes one, else the mapping learned from its echo, else straight onto the range.
///
/// The identity fallback is exact for a single display and is what a stock guest gets until it
/// has moved the pointer enough to learn from — the behaviour that shipped before this module,
/// so nothing regresses while the fit converges.
pub(crate) fn abs_position(slot: usize, u: f64, v: f64, abs_max: i32) -> Option<(i32, i32)> {
    if !super::arrangement::has_report() {
        let sizes = super::echo::scanout_sizes();
        // One display's share IS the range: there is nothing to learn, and identity is exact.
        if sizes.iter().filter(|s| s.0 > 0 && s.1 > 0).count() > 1 {
            if let Some(p) = place(slot, (u, v), sizes[slot], abs_max) {
                return Some(p);
            }
        }
    }
    super::arrangement::abs_through_report(slot, u, v, abs_max)
}

/// Which live slots still have no mapping — what a deliberate probe is for.
///
/// Fewer than two live scanouts is never incomplete: one display's share IS the range, and
/// identity is exact.
pub(crate) fn incomplete() -> bool {
    let sizes = super::echo::scanout_sizes();
    let live: Vec<usize> = sizes
        .iter()
        .enumerate()
        .filter(|(_, s)| s.0 > 0 && s.1 > 0)
        .map(|(i, _)| i)
        .collect();
    if live.len() < 2 {
        return false;
    }
    let fits = FITS.lock().unwrap();
    live.iter().any(|s| fits.get(*s).is_none_or(|f| !f.ready()))
}

/// Which live slots are missing which axis — the reason a sweep was wanted, in words.
///
/// Naming the axis matters: x and y go unlearned for different reasons (a desktop wider than
/// it is tall gives x a long lever and y a short one), so "slot 1 y" and "slot 1 x" point at
/// different faults.
pub(crate) fn incomplete_slots() -> String {
    let sizes = super::echo::scanout_sizes();
    let fits = FITS.lock().unwrap();
    let missing: Vec<String> = sizes
        .iter()
        .enumerate()
        .filter(|(_, s)| s.0 > 0 && s.1 > 0)
        .filter_map(|(i, _)| {
            let f = fits.get(i)?;
            let axes = match (f.x.line().is_some(), f.y.line().is_some()) {
                (true, true) => return None,
                (false, true) => "x",
                (true, false) => "y",
                (false, false) => "x and y",
            };
            Some(format!("slot {i} has no {axes}"))
        })
        .collect();
    if missing.is_empty() {
        "nothing — the want went away".to_string()
    } else {
        missing.join(", ")
    }
}

/// Is a deliberate sweep of the device worth running right now?
///
/// Only where there is something to learn: a guest that reports its arrangement is already
/// exact, and a single display needs nothing.
pub(crate) fn probe_wanted() -> bool {
    !super::arrangement::has_report() && incomplete()
}

/// How long after a sweep ends before another may start.
///
/// A sweep learns nothing from a guest that is showing no cursor at all — every step answers
/// "nothing visible" — and the want that started it is then still true, so without a rest the
/// tick would sweep forever, hammering the device.
pub(crate) const PROBE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(2);

/// How still the hand must be before a sweep may borrow the pointer.
///
/// A sweep takes the guest's cursor away for about a fifth of a second. Doing that in the
/// middle of a stroke is the most disruptive thing this module can do, and the moment a sweep
/// is most likely to be wanted — a mapping just went unlearned — is precisely the moment the
/// hand is busy. So the sweep waits for a gap in the user's own movement.
pub(crate) const PROBE_QUIET: std::time::Duration = std::time::Duration::from_millis(500);

/// Everything the decision to start a sweep rests on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProbeGate {
    /// [`probe_wanted`]: there is something to learn.
    pub(crate) wanted: bool,
    /// Any guest button held.
    pub(crate) buttons_down: bool,
    /// Since the last sweep ended, if there has been one.
    pub(crate) since_sweep: Option<std::time::Duration>,
    /// Since the hand last put a position on the device, if it ever has.
    pub(crate) since_hand: Option<std::time::Duration>,
}

/// May a fresh sweep start?
///
/// Note what is *not* here: whether the pointer is grabbed. The mapping a sweep learns is the
/// one the **uncaptured** pointer is placed through, so requiring a grab would learn it last
/// where it is needed first. It is also how this deadlocked: the sweep took its restore
/// position from state that only captured motion ever wrote, so it could not run until the
/// user had already moved the pointer through the wrong mapping it existed to replace.
///
/// The reservations are all about not taking the pointer out of a hand that is using it: no
/// held button (a sweep mid-drag drags the guest's content across its desktop),
/// [`PROBE_QUIET`] since the hand last moved, and [`PROBE_COOLDOWN`] since the last sweep.
pub(crate) fn probe_may_start(g: ProbeGate) -> bool {
    g.wanted
        && !g.buttons_down
        && g.since_sweep.is_none_or(|d| d >= PROBE_COOLDOWN)
        && g.since_hand.is_none_or(|d| d >= PROBE_QUIET)
}

/// The sweep a probe walks, in device units.
///
/// Wide spread on `u` so every slot's share is crossed with a long lever, and `v` alternating
/// so both axes settle from the same eight steps. Deliberately short of the extremes: the
/// guest's own hot corners live at 0 and 1, and a probe that trips the overview would be worse
/// than the misplacement it fixes.
pub(crate) const PROBE_SWEEP: [(f64, f64); 10] = [
    (0.05, 0.30),
    (0.15, 0.70),
    (0.25, 0.30),
    (0.35, 0.70),
    (0.45, 0.30),
    (0.55, 0.70),
    (0.65, 0.30),
    (0.75, 0.70),
    (0.85, 0.30),
    (0.95, 0.70),
];

/// Forget every slot's mapping.
///
/// Called for any change to the scanout pool, not just the slot that changed: one display's
/// mode is a term in *every* slot's line (it moves the union the device is spread over), so a
/// single modeset invalidates them all. Also called when the guest goes away.
pub(crate) fn forget() {
    *FITS.lock().unwrap() = [const { Fit::new() }; MAX_SCANOUTS];
}

#[cfg(test)]
mod probe_start_tests {
    use super::{probe_may_start, ProbeGate, PROBE_COOLDOWN, PROBE_QUIET};
    use std::time::Duration;

    /// A want, a free pointer, and a hand that is not mid-stroke.
    fn ready() -> ProbeGate {
        ProbeGate {
            wanted: true,
            buttons_down: false,
            since_sweep: None,
            since_hand: Some(PROBE_QUIET),
        }
    }

    /// The regression this whole change is about: no grab is required. The sweep used to
    /// demand one it could not have got — the mapping it learns is the ungrabbed pointer's,
    /// so a grabbed-only sweep learns it too late, and nothing here may grow a grab field.
    #[test]
    fn a_sweep_needs_nothing_but_a_want_and_a_pointer_at_rest() {
        assert!(probe_may_start(ready()));
    }

    /// A hand that has never touched the device cannot be mid-stroke.
    #[test]
    fn a_hand_that_never_moved_does_not_hold_the_sweep_up() {
        assert!(probe_may_start(ProbeGate {
            since_hand: None,
            ..ready()
        }));
    }

    #[test]
    fn nothing_to_learn_means_no_sweep() {
        assert!(!probe_may_start(ProbeGate {
            wanted: false,
            ..ready()
        }));
    }

    /// A sweep mid-drag would drag the guest's content across its desktop.
    #[test]
    fn a_held_button_defers_the_sweep() {
        assert!(!probe_may_start(ProbeGate {
            buttons_down: true,
            ..ready()
        }));
    }

    /// The disruption reported 2026-08-22: sweeps arriving in the middle of ordinary captured
    /// movement. The moment a sweep is most wanted — a mapping just went unlearned — is the
    /// moment the hand is busiest, so wanting one is no licence to take the pointer.
    #[test]
    fn a_sweep_waits_for_a_gap_in_the_users_own_movement() {
        assert!(!probe_may_start(ProbeGate {
            since_hand: Some(PROBE_QUIET / 2),
            ..ready()
        }));
        assert!(probe_may_start(ProbeGate {
            since_hand: Some(PROBE_QUIET),
            ..ready()
        }));
    }

    /// A guest showing no cursor answers no step, so the want survives the sweep. Without the
    /// rest that pair spins the tick, sweeping the device forever.
    #[test]
    fn a_sweep_that_taught_nothing_rests_before_trying_again() {
        assert!(!probe_may_start(ProbeGate {
            since_sweep: Some(Duration::from_millis(1)),
            ..ready()
        }));
        assert!(probe_may_start(ProbeGate {
            since_sweep: Some(PROBE_COOLDOWN),
            ..ready()
        }));
    }
}

#[cfg(test)]
mod clamped_sample_tests {
    use super::{Axis, Sample, CONTRADICTIONS};

    /// The rig's slot 0 on the y axis: a 1440-tall mode whose full height is the whole range.
    const H: f64 = 1440.0;

    fn learned() -> Axis {
        let mut ax = Axis::new();
        for (u, pixel) in [(0.1, 144.0), (0.4, 576.0), (0.7, 1008.0)] {
            ax.observe(Sample { u, pixel }, H);
        }
        assert!(ax.line().is_some(), "the fixture must start with a line");
        ax
    }

    /// The fault behind the sweep storm of 2026-08-22. Riding the top edge of the guest
    /// desktop echoes the same pixel while the sends keep changing; those pairs contradict the
    /// line, reseed a fit too flat to believe, and leave the slot with no mapping at all —
    /// which is what asks for a sweep, in the middle of the stroke that produced it.
    #[test]
    fn a_pointer_the_guest_is_clamping_teaches_nothing() {
        let mut ax = learned();
        let held = ax.line().unwrap();
        for u in [0.28, 0.26, 0.24, 0.22, 0.20, 0.18] {
            ax.observe(Sample { u, pixel: -1.0 }, H);
        }
        assert_eq!(ax.line(), Some(held), "an edge run must not move the line");
    }

    #[test]
    fn the_far_edge_is_just_as_mute_as_the_near_one() {
        let mut ax = learned();
        let held = ax.line().unwrap();
        for u in [0.90, 0.92, 0.94, 0.96, 0.98, 1.00] {
            ax.observe(Sample { u, pixel: H - 1.0 }, H);
        }
        assert_eq!(ax.line(), Some(held));
    }

    /// Doubt is not knowledge. Samples that disagree with the line but are too bunched to fit
    /// used to leave the line as None — and a slot with no line is what summons a sweep, so
    /// noise did not merely blur the mapping, it took it away.
    #[test]
    fn a_line_is_never_given_up_for_nothing() {
        let mut ax = learned();
        let held = ax.line().unwrap();
        for i in 0..CONTRADICTIONS + 2 {
            let u = 0.50 + i as f64 * 0.001;
            ax.observe(Sample { u, pixel: 40.0 }, H);
        }
        assert_eq!(
            ax.line(),
            Some(held),
            "a fit that cannot be stated must not unseat one that can"
        );
    }

    /// ...but an arrangement that really did move still wins, once the dissent has a lever.
    #[test]
    fn a_wider_dissent_still_replaces_the_line() {
        let mut ax = learned();
        let held = ax.line().unwrap();
        for (u, pixel) in [(0.15, 500.0), (0.45, 940.0), (0.75, 1380.0)] {
            ax.observe(Sample { u, pixel }, H);
        }
        let now = ax.line().expect("a fittable dissent replaces the line");
        assert_ne!(now, held);
        assert!((now.a - 1466.0).abs() < 20.0, "slope {} unexpected", now.a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rig measured 2026-08-21, in the terms this module works in: mutter spread one device
    /// over a 3560x1152 logical desktop; slot 0 is a 2560x1440 mode at scale 1.25 (2048 logical
    /// at x=0), slot 1 a 3024x1960 mode at scale 2 (1512 logical at x=2048).
    fn rig(slot: usize) -> (Line, Line) {
        match slot {
            0 => (
                Line {
                    a: 3560.0 * 1.25,
                    b: 0.0,
                },
                Line {
                    a: 1152.0 * 1.25,
                    b: 0.0,
                },
            ),
            _ => (
                Line {
                    a: 3560.0 * 2.0,
                    b: -2048.0 * 2.0,
                },
                Line {
                    a: 1152.0 * 2.0,
                    b: 0.0,
                },
            ),
        }
    }

    fn samples(l: Line, us: &[f64]) -> Vec<Sample> {
        us.iter()
            .map(|u| Sample {
                u: *u,
                pixel: l.pixel(*u).round(),
            })
            .collect()
    }

    #[test]
    fn a_line_through_two_samples_recovers_the_rig() {
        let (lx, _) = rig(0);
        let f = fit(&samples(lx, &[0.1895, 0.5334]), 2560.0).unwrap();
        assert!((f.a - lx.a).abs() < 5.0, "{f:?}");
        assert!((f.b - lx.b).abs() < 5.0, "{f:?}");
        // ...and inverting it places the cursor where we asked, to the pixel.
        assert!((f.pixel(f.unit(1280.0).unwrap()) - 1280.0).abs() < 0.5);
    }

    #[test]
    fn the_measured_teleport_is_what_identity_produces() {
        // Host pointer at the BenQ's centre, sent as the identity mapping does: u = 0.505.
        let (lx, _) = rig(0);
        assert!((lx.pixel(0.505) - 2248.0).abs() < 5.0);
        // Through the fitted line, the centre asks for the centre and gets it.
        assert!((lx.unit(1280.0).unwrap() - 0.2876).abs() < 1e-3);
    }

    #[test]
    fn samples_too_close_together_do_not_make_a_line() {
        let (lx, _) = rig(0);
        assert_eq!(fit(&samples(lx, &[0.20, 0.24]), 2560.0), None);
        assert_eq!(fit(&samples(lx, &[0.20]), 2560.0), None);
        assert_eq!(fit(&[], 2560.0), None);
    }

    #[test]
    fn a_slope_smaller_than_the_display_is_refused() {
        // A line that would have the whole device sweep less than this one display cannot be a
        // share of it — samples from two different arrangements can fit one.
        let bogus = [
            Sample { u: 0.0, pixel: 0.0 },
            Sample {
                u: 1.0,
                pixel: 1000.0,
            },
        ];
        assert_eq!(fit(&bogus, 2560.0), None);
    }

    #[test]
    fn an_axis_learns_then_refines() {
        let (lx, _) = rig(1);
        let mut ax = Axis::new();
        assert_eq!(ax.line(), None);
        for s in samples(lx, &[0.60, 0.75]) {
            ax.observe(s, 3024.0);
        }
        let learned = ax.line().expect("two spread samples make a line");
        assert!((learned.a - lx.a).abs() < 10.0);
        for s in samples(lx, &[0.65, 0.90, 0.99]) {
            ax.observe(s, 3024.0);
        }
        let refined = ax.line().unwrap();
        assert!((refined.a - lx.a).abs() < 5.0, "{refined:?}");
    }

    #[test]
    fn one_stray_sample_does_not_unseat_a_line() {
        let (lx, _) = rig(0);
        let mut ax = Axis::new();
        for s in samples(lx, &[0.10, 0.30, 0.50]) {
            ax.observe(s, 2560.0);
        }
        let before = ax.line().unwrap();
        // Echo lag: a sample taken while the pointer was still travelling.
        ax.observe(
            Sample {
                u: 0.40,
                pixel: 900.0,
            },
            2560.0,
        );
        assert_eq!(ax.line(), Some(before), "one miss must not move the line");
    }

    #[test]
    fn a_rearrangement_replaces_the_line() {
        // Learn the rig, then have the user drag the BenQ to the right of the built-in: slot 0
        // keeps its mode (nothing on the virtio-gpu wire) but now starts 1512 logical in.
        let (lx, _) = rig(0);
        let mut ax = Axis::new();
        for s in samples(lx, &[0.10, 0.30, 0.50]) {
            ax.observe(s, 2560.0);
        }
        assert!(ax.line().is_some());
        let moved = Line {
            a: 3560.0 * 1.25,
            b: -1512.0 * 1.25,
        };
        let held = ax.line().unwrap();
        let after = samples(moved, &[0.45, 0.60, 0.80]);
        for (i, s) in after.iter().enumerate() {
            ax.observe(*s, 2560.0);
            if i + 1 < CONTRADICTIONS {
                assert_eq!(ax.line(), Some(held), "the old line stands until outvoted");
            }
        }
        let relearned = ax
            .line()
            .expect("the contradicting samples seed the new fit");
        assert!((relearned.a - moved.a).abs() < 10.0, "{relearned:?}");
        assert!((relearned.b - moved.b).abs() < 10.0, "{relearned:?}");
    }

    #[test]
    fn a_stationary_pointer_keeps_the_ring_spread() {
        let (lx, _) = rig(0);
        let mut ax = Axis::new();
        // Both inside slot 0's own share: past u = 0.5754 the rig's cursor is on slot 1, so a
        // slot 0 echo out there is not a sample this axis can ever be offered.
        for s in samples(lx, &[0.10, 0.50]) {
            ax.observe(s, 2560.0);
        }
        // Twenty echoes from a pointer that has not moved must not evict the far sample.
        for _ in 0..20 {
            ax.observe(
                Sample {
                    u: 0.50,
                    pixel: lx.pixel(0.50).round(),
                },
                2560.0,
            );
        }
        assert_eq!(ax.samples.len(), 2);
        assert!(ax.line().is_some());
    }

    #[test]
    fn the_sweep_gives_every_share_a_long_lever_and_avoids_the_corners() {
        let us: Vec<f64> = PROBE_SWEEP.iter().map(|s| s.0).collect();
        let vs: Vec<f64> = PROBE_SWEEP.iter().map(|s| s.1).collect();
        // Never at the ends: the guest's hot corners are there.
        assert!(us.iter().all(|u| *u > 0.0 && *u < 1.0));
        assert!(vs.iter().all(|v| *v > 0.2 && *v < 0.8));
        // Both axes clear MIN_SPREAD several times over, and no two steps sit on top of each
        // other (which the sample ring would collapse into one).
        let (lo, hi) = (us[0], *us.last().unwrap());
        assert!(hi - lo > MIN_SPREAD * 8.0);
        assert!(
            vs.iter().cloned().fold(f64::MIN, f64::max)
                - vs.iter().cloned().fold(f64::MAX, f64::min)
                > MIN_SPREAD
        );
        for w in us.windows(2) {
            assert!(
                w[1] - w[0] > NEAR_U * 4.0,
                "steps {w:?} are too close to distinguish"
            );
        }
        // Even the smallest share of a two-display desktop gets a usable lever: a slot owning
        // a quarter of the range still sees two steps.
        for lo in [0.0, 0.25, 0.5, 0.75] {
            let n = us.iter().filter(|u| **u >= lo && **u < lo + 0.25).count();
            assert!(n >= 2, "only {n} steps land in {lo}..{}", lo + 0.25);
        }
    }

    #[test]
    fn placing_needs_both_axes() {
        let mut f = Fit::new();
        let (lx, ly) = rig(1);
        for s in samples(lx, &[0.60, 0.90]) {
            f.x.observe(s, 3024.0);
        }
        assert!(!f.ready(), "one axis is not a mapping");
        for s in samples(ly, &[0.30, 0.60]) {
            f.y.observe(s, 1960.0);
        }
        assert!(f.ready());
    }
}
