// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The warp broker: the one owner of `CGWarpMouseCursorPosition` while capture machinery is
//! live.
//!
//! The single most-repeated fault in the pointer stack (five pre-arc instances, one in the
//! restructure arc) is a warp or its echo becoming guest motion. Every warp carries obligations
//! by convention: land where it was aimed (asserted, before and after), close the ~0.25 s
//! local-events suppression interval a warp opens, and re-assert the blank wear (the
//! ghost-cursor class — AppKit/CG can strip it behind our back). Scattered call sites each
//! remembering all of them is how the fault recurs, so the externs live here, module-private:
//! no other module *can* warp, and each broker method performs its bundle atomically. The next
//! warp-class bug is a panic message from this module, not a dogfood session.
//!
//! The CoreGraphics facts the bundles are built on:
//!   - `CGAssociateMouseAndMouseCursorPosition(0)` asks the HID layer to stop driving the
//!     cursor from the mouse. It's unreliable on its own (the cursor still drifted onto windows
//!     behind us on macOS 26), so the tap ALSO re-pins the cursor to the park on every captured
//!     move ([`WarpBroker::repin`]).
//!   - `CGWarpMouseCursorPosition` does the re-pin. A ZERO-LENGTH warp injects nothing — which
//!     is why every steady-state re-pin targets the point the cursor is already at. A NONZERO
//!     warp's vector arrives in a following event's delta, with no suppression gap, and the
//!     SIGN DEPENDS ON THE REGIME (both measured 2026-08-20: same-display mid-motion it arrives
//!     NEGATED, `LIMINA_WARP_PROBE` 32/32; crossing displays it arrives as +W, riding one of
//!     the next 1-2 events). So while captured there are exactly two nonzero warps: the
//!     release's, taken with the mouse still disassociated and the cursor hidden, where nothing
//!     can turn it into motion; and the re-park that moves the park onto the panel the guest's
//!     cursor crossed to ([`WarpBroker::repark`]), whose injected vector is recognized and
//!     subtracted when it arrives ([`WarpSwallow`]).
//!
//! One conservation check deliberately does NOT live here: the park zero-warp check in
//! `InputState::toggle_capture_to` (seed vs `PARK_INSET`). It is view-space geometry judged
//! before the CG-global conversion; the broker speaks CG global only. Moving it here would
//! trade the geometric guarantee it measures for a coordinate conversion round-trip.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_app_kit::{NSCursor, NSEvent};
use objc2_foundation::NSPoint;

use super::hostdisplay;
use super::input::HostCursor;

// `connected` is a `boolean_t` (C `int`); 0 = decoupled (captured), 1 = normal.
extern "C" {
    fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
    fn CGWarpMouseCursorPosition(point: NSPoint) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> objc2_foundation::NSRect;
}

/// What a warp is supposed to achieve, asserted before and after every warp the broker
/// performs. The captured pointer is OUR integration — we tell the guest where its cursor is,
/// so we always know which guest slot it is on and which host displays that slot's window
/// covers; a host cursor anywhere else is a fault, and a silent one becomes "the menu bar
/// revealed on the other display" in dogfood. These crash instead (`assert!`, by design: the
/// failure is a pointer on the wrong screen and the user wants it loud, not logged).
#[derive(Clone, Debug)]
pub(crate) struct Aim {
    /// Which warp this is (`engage`, `repin`, `release`), for the message.
    pub(crate) stage: &'static str,
    /// The guest slot whose window the target is meant to sit in.
    pub(crate) slot: usize,
    /// The host displays that window covers ([`hostdisplay::displays_under_window`]); the
    /// target must be on one of them. Empty when the caller has no window to judge by (an
    /// edge release onto whatever display lies past the edge) — then only "on SOME display"
    /// is required.
    pub(crate) displays: Vec<u32>,
}

/// The most a warp's readback may differ from its target, in CG points, before the warp is
/// judged to have landed elsewhere. `CGWarpMouseCursorPosition` is synchronous and lands on
/// the target **floored to whole points** (measured 2026-08-21: a warp to (1746.9, 259.8)
/// reads back (1746.0, 259.0); integer targets read back exactly, association on or off,
/// cross-display or not), so the honest miss is under √2. The window server's clamp into the
/// display union — the failure this catches — moves the cursor by whole points at the least
/// and, in the cases that matter, by a whole display.
const LANDING_TOLERANCE: f64 = 1.5;

/// Did the cursor land where the warp aimed? Pure, so the tolerance is pinned by tests.
fn landing_verdict(aimed: NSPoint, landed: NSPoint) -> Result<(), f64> {
    let miss = (landed.x - aimed.x).hypot(landed.y - aimed.y);
    if miss <= LANDING_TOLERANCE {
        Ok(())
    } else {
        Err(miss)
    }
}

/// Where the cursor is right now, CG global. Synchronous with respect to a warp just issued
/// (measured 2026-08-21, `spikes`' warp probe: the new position reads back within 0.4 ms on
/// both `NSEvent.mouseLocation` and a fresh `CGEvent`), so it is a sound post-condition.
fn cursor_now() -> NSPoint {
    let ns = NSEvent::mouseLocation();
    // SAFETY: plain CoreGraphics query.
    let h = unsafe { CGDisplayBounds(CGMainDisplayID()) }.size.height;
    NSPoint::new(ns.x, h - ns.y)
}

/// The one warp primitive: assert the target is on a display the aim allows, warp, and assert
/// the cursor is now there. Every broker method's warp goes through here.
fn warp_checked(target: NSPoint, aim: &Aim) {
    let on = hostdisplay::displays_at(target);
    assert!(
        !on.is_empty(),
        "pointer capture [{stage}]: warp target ({x:.1},{y:.1}) for slot {slot} is on NO display — the window server would clamp it onto a neighbour; expected displays {exp:?}; arrangement: {arr}",
        stage = aim.stage,
        x = target.x,
        y = target.y,
        slot = aim.slot,
        exp = aim.displays,
        arr = hostdisplay::describe_arrangement(),
    );
    assert!(
        aim.displays.is_empty() || on.iter().any(|d| aim.displays.contains(d)),
        "pointer capture [{stage}]: warp target ({x:.1},{y:.1}) for slot {slot} is on displays {on:?}, but that slot's window covers {exp:?}; arrangement: {arr}",
        stage = aim.stage,
        x = target.x,
        y = target.y,
        slot = aim.slot,
        exp = aim.displays,
        arr = hostdisplay::describe_arrangement(),
    );
    // SAFETY: plain CoreGraphics call; the target was just shown to be a real display point.
    unsafe { CGWarpMouseCursorPosition(target) };
    let landed = cursor_now();
    if let Err(miss) = landing_verdict(target, landed) {
        panic!(
            "pointer capture [{stage}]: warped to ({x:.1},{y:.1}) for slot {slot} but the cursor reads back at ({lx:.1},{ly:.1}) — {miss:.1} pt off, on displays {ld:?} (aimed at {on:?}, window covers {exp:?}); arrangement: {arr}",
            stage = aim.stage,
            x = target.x,
            y = target.y,
            slot = aim.slot,
            lx = landed.x,
            ly = landed.y,
            ld = hostdisplay::displays_at(landed),
            exp = aim.displays,
            arr = hostdisplay::describe_arrangement(),
        );
    }
}

/// Whether we currently have the host cursor hidden for capture — keeps `NSCursor::hide`/`unhide`
/// (a reference count) balanced no matter how the toggle is driven.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Re-attach the hardware mouse to the cursor.
///
/// Called right after a warp, for a reason that has nothing to do with association: a warp also
/// opens a **local events suppression interval** — 0.25 s by default — during which real mouse
/// movement no longer moves the cursor. Re-associating ends it immediately. Without this, edge
/// resistance felt like it ate travel: after pushing against an edge you had to move a long way
/// back before the cursor unstuck, because a quarter-second of your motion was being discarded
/// by the window server. Harmless when the mouse is already associated, which is the only state
/// the uncaptured path can be in (capture disassociates, and resistance does not run captured).
fn end_warp_suppression() {
    unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
}

/// The broker. One instance, owned by `InputState`; every method is main-thread-only (the
/// cursor calls must balance) and performs its whole obligation bundle before returning.
/// A re-park's armed injection detector. Measured (`[CAP]` trace, 2026-08-20, 10/10
/// crossings): a cross-display warp of vector W arrives as +W in one of the next 1-2 motion
/// events' deltas — the OPPOSITE sign of the same-display law the warp probe measured, which
/// is why the injection is detected and subtracted when it arrives rather than predicted (a
/// blind add built on the same-display sign doubled every crossing into a 2W jump). Armed for
/// [`WARP_SWALLOW_EVENTS`] real motion events; an unmatched arm expires silently — that is
/// the injection-never-happened branch, handled instead of assumed.
#[derive(Clone, Copy, Debug, PartialEq)]
struct WarpSwallow {
    /// The warp vector W, in CG global points.
    w: (f64, f64),
    /// Real motion events left before the detector disarms.
    events_left: u8,
    /// Every delta seen while armed, summed — the conservation report's evidence. If the
    /// detector expires and this sum still contains ~W, the injection arrived unrecognized and
    /// became guest motion: exactly the fault class every park bug was (the guest cursor moves
    /// only by host motion). Reported loud at expiry, so dogfood reads a warning instead of
    /// feeling a jump.
    seen: (f64, f64),
}

/// How many real motion events an armed [`WarpSwallow`] watches. The injection rode event 1
/// or 2 in every measured crossing; the third is margin. Deliberately event-counted, not
/// wall-clocked: the injection rides the next event whenever it comes, and an idle hand can
/// hold that event back for seconds.
const WARP_SWALLOW_EVENTS: u8 = 3;

/// What one [`swallow_step`] concluded, beyond the delta it hands on.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// The detector stays armed; the delta was ordinary motion.
    Pass,
    /// The injected event arrived: W was recognized and subtracted from the delta.
    Recognized,
    /// The detector's last event passed without recognition. `skipped` is the conservation
    /// verdict: true means the accumulated deltas still contain ~W — the injection arrived
    /// smeared across events or offset by real motion and became guest motion.
    Expired { skipped: bool, seen: (f64, f64) },
}

/// One armed step of the injection detector — the pure state machine, so the thresholds are
/// testable in one place. Takes the armed state and one real motion event's delta; answers
/// with the delta to hand on (W subtracted iff recognized), the state to keep (None disarms),
/// and the outcome for the broker to report.
fn swallow_step(sw: WarpSwallow, dx: f64, dy: f64) -> (f64, f64, Option<WarpSwallow>, Outcome) {
    let (wx, wy) = sw.w;
    let mag2 = wx * wx + wy * wy;
    // The injected event is W plus whatever real motion resumed with it, so the test is
    // the delta's component along Ŵ reaching |W|/2 — real post-pause deltas are a few
    // points against a W of 80+, so the regimes don't overlap. The recognized event does
    // not join `seen`: its ~W content is accounted for by the subtraction itself.
    if mag2 > 0.0 && dx * wx + dy * wy >= 0.5 * mag2 {
        return (dx - wx, dy - wy, None, Outcome::Recognized);
    }
    let seen = (sw.seen.0 + dx, sw.seen.1 + dy);
    if sw.events_left <= 1 {
        // The conservation report: the guest cursor moves only by host motion, and W is
        // the one vector here that is NOT host motion. An expiry whose accumulated deltas
        // still contain ~W (same projection test as the recognizer, over the window's sum)
        // means the injection arrived unrecognized — smeared across events or offset by
        // real motion — and became guest motion: the fault class of every park bug.
        let skipped = mag2 > 0.0 && seen.0 * wx + seen.1 * wy >= 0.5 * mag2;
        (dx, dy, None, Outcome::Expired { skipped, seen })
    } else {
        let next = WarpSwallow {
            events_left: sw.events_left - 1,
            seen,
            ..sw
        };
        (dx, dy, Some(next), Outcome::Pass)
    }
}

pub(crate) struct WarpBroker {
    /// The armed injection detector, if a re-park's warp is still owed its arrival.
    pending: Cell<Option<WarpSwallow>>,
}

impl WarpBroker {
    pub(crate) fn new() -> Self {
        Self {
            pending: Cell::new(None),
        }
    }

    /// Enter capture: decouple the HW mouse, park the cursor, and hide it behind both belts
    /// (the hide refcount and the transparent wear — AppKit can unhide behind the refcount,
    /// 2026-08-19). `park` is a point inside our own window (see `fit::park_point`), so the
    /// warp is zero-length and injects no phantom motion.
    pub(crate) fn engage(&self, park: NSPoint, aim: &Aim, host_cursor: &HostCursor) {
        // A detector armed for an injection that will never come (capture toggled in between)
        // must not survive to eat the next session's first flick.
        self.pending.set(None);
        unsafe { CGAssociateMouseAndMouseCursorPosition(0) };
        warp_checked(park, aim);
        // Idempotent: NSCursor hide/unhide is a counter — a double-hide (e.g. a stray double
        // toggle) would need a matching double-unhide or the cursor stays gone forever. Only
        // hide if we haven't already, so the count never drifts.
        if !CURSOR_HIDDEN.swap(true, Ordering::AcqRel) {
            NSCursor::hide();
        }
        // Belt to the hide's braces: wear the transparent blank for the whole capture. An
        // unhidden transparent cursor still shows nothing.
        host_cursor.set_captured(true);
        log::info!("pointer capture: ON (Cmd-Ctrl-G to release)");
    }

    /// Leave capture: warp the cursor to `release_to` (where the captured virtual cursor
    /// ended, so leaving capture is as seamless as entering it), re-couple the mouse, show
    /// the cursor, and re-assert the guest shape.
    pub(crate) fn disengage(&self, release_to: Option<(NSPoint, Aim)>, host_cursor: &HostCursor) {
        self.pending.set(None);
        // Warp BEFORE re-associating, and while still hidden: the cursor *appears* at the
        // release point rather than visibly jumping there from the park, and — the part that
        // was a real bug — the hardware cannot move it in between. Associating first made the
        // mouse live while the cursor was still parked, so motion already in flight was
        // applied from the park point; with the park on another display that put the pointer
        // on the wrong screen entirely, raising that screen's Dock instead of the guest's dash.
        // (The landing readback inside `warp_checked` is sound for the same reason: the
        // hardware cannot have moved the cursor yet.)
        if let Some((p, aim)) = &release_to {
            warp_checked(*p, aim);
        }
        unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
        // A warp opens a 0.25 s local-events suppression interval, and this one is applied at the
        // exact moment the user gets the pointer back — so every release ate up to a quarter
        // second of the motion that followed it. Re-associating closes the interval; the
        // `CGAssociateMouseAndMouseCursorPosition(1)` above cannot do it, because the warp comes
        // after. Latent since capture shipped, and load-bearing for the fullscreen grab, whose
        // whole release story is "the motion continues naturally".
        end_warp_suppression();
        if CURSOR_HIDDEN.swap(false, Ordering::AcqRel) {
            NSCursor::unhide();
        }
        host_cursor.set_captured(false);
        host_cursor.reassert();
        log::info!("pointer capture: OFF");
    }

    /// The steady-state re-pin: hold the (hidden, disassociated) cursor at the park so it
    /// can't drift onto windows behind us — `CGAssociate(false)` alone doesn't reliably
    /// freeze it. Zero-length by construction (the cursor is already at the park), so it
    /// injects nothing; no wear, no suppression concern.
    pub(crate) fn repin(&self, park: NSPoint, aim: &Aim) {
        warp_checked(park, aim);
    }

    /// The one legitimate nonzero warp while captured: move the park into the window of the
    /// panel the guest's cursor has crossed to, once the hand has paused. Warps, re-dresses
    /// the blank immediately (a cross-display warp is a display-affecting operation — the
    /// ghost-cursor class: AppKit/CG can strip the wear or unhide behind the refcount,
    /// 2026-08-19), and arms the injection detector with the vector `new − old`. Answers with
    /// the armed vector, `None` when there was no prior park to measure from (nothing armed —
    /// a first park has no injection to owe).
    pub(crate) fn repark(
        &self,
        new: NSPoint,
        old: Option<NSPoint>,
        aim: &Aim,
        host_cursor: &HostCursor,
    ) -> Option<(f64, f64)> {
        warp_checked(new, aim);
        host_cursor.rewear_captured_blank();
        old.map(|old| {
            let w = (new.x - old.x, new.y - old.y);
            self.pending.set(Some(WarpSwallow {
                w,
                events_left: WARP_SWALLOW_EVENTS,
                seen: (0.0, 0.0),
            }));
            w
        })
    }

    /// Run one real motion event's delta through the armed injection detector, if any —
    /// consumed by the REAL motion paths only (the tap and the degraded monitor).
    pub(crate) fn swallow(&self, dx: f64, dy: f64) -> (f64, f64) {
        let Some(sw) = self.pending.get() else {
            return (dx, dy);
        };
        let (wx, wy) = sw.w;
        let (out_x, out_y, next, outcome) = swallow_step(sw, dx, dy);
        self.pending.set(next);
        match outcome {
            Outcome::Pass => {}
            Outcome::Recognized => {
                log::debug!("pointer capture: swallowed injected warp delta ({wx:.0},{wy:.0})");
            }
            Outcome::Expired {
                skipped: true,
                seen,
            } => {
                // Loud, so dogfood reads a warning instead of feeling a jump.
                log::warn!(
                    "pointer capture: warp ({wx:.0},{wy:.0}) was absorbed as guest motion \
                     (deltas summed to ({:.0},{:.0}) while armed) — a conservation skip",
                    seen.0,
                    seen.1,
                );
            }
            Outcome::Expired { skipped: false, .. } => {
                log::debug!(
                    "pointer capture: warp injection never arrived (expected {wx:.0},{wy:.0})"
                );
            }
        }
        (out_x, out_y)
    }

    /// `LIMINA_WARP_PROBE`'s raw warp — the measurement instrument that established the
    /// same-display sign law. Deliberately plain: the probe exists to measure the raw
    /// injection, and a broker that compensated it would be measuring itself.
    pub(crate) fn probe(&self, target: NSPoint) {
        unsafe { CGWarpMouseCursorPosition(target) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(w: (f64, f64)) -> WarpSwallow {
        WarpSwallow {
            w,
            events_left: WARP_SWALLOW_EVENTS,
            seen: (0.0, 0.0),
        }
    }

    #[test]
    fn the_injected_warp_is_recognized_and_subtracted_when_it_arrives() {
        // The measured cross-display law: the whole vector rides one event, plus a little
        // real motion that resumed with it. The real motion must survive the subtraction.
        let (dx, dy, next, out) = swallow_step(armed((80.0, -40.0)), 83.0, -38.0);
        assert_eq!(out, Outcome::Recognized);
        assert_eq!((dx, dy), (3.0, 2.0));
        assert_eq!(next, None, "recognition disarms");
    }

    #[test]
    fn recognition_still_subtracts_exactly_w_on_a_later_event() {
        let (dx, dy, next, out) = swallow_step(armed((0.0, 120.0)), 2.0, 3.0);
        assert_eq!(out, Outcome::Pass);
        assert_eq!((dx, dy), (2.0, 3.0), "ordinary motion passes untouched");
        let sw = next.expect("still armed");
        assert_eq!(sw.seen, (2.0, 3.0));
        let (dx, dy, next, out) = swallow_step(sw, -1.0, 121.0);
        assert_eq!(out, Outcome::Recognized);
        assert_eq!((dx, dy), (-1.0, 1.0));
        assert_eq!(next, None);
    }

    #[test]
    fn an_injection_that_never_arrives_expires_silently() {
        let mut sw = armed((100.0, 0.0));
        for _ in 0..2 {
            let (_, _, next, out) = swallow_step(sw, 1.0, 2.0);
            assert_eq!(out, Outcome::Pass);
            sw = next.expect("still armed");
        }
        let (dx, dy, next, out) = swallow_step(sw, 1.0, 2.0);
        assert_eq!((dx, dy), (1.0, 2.0), "expiry never eats the delta");
        assert_eq!(next, None);
        assert_eq!(
            out,
            Outcome::Expired {
                skipped: false,
                seen: (3.0, 6.0)
            }
        );
    }

    #[test]
    fn a_smeared_injection_is_reported_as_a_conservation_skip() {
        let mut sw = armed((90.0, 0.0));
        for _ in 0..2 {
            let (_, _, next, out) = swallow_step(sw, 30.0, 0.0);
            assert_eq!(out, Outcome::Pass, "30·90 < ½·90² — below the recognizer");
            sw = next.expect("still armed");
        }
        let (.., next, out) = swallow_step(sw, 30.0, 0.0);
        assert_eq!(next, None);
        assert_eq!(
            out,
            Outcome::Expired {
                skipped: true,
                seen: (90.0, 0.0)
            }
        );
    }

    #[test]
    fn the_recognizer_fires_at_exactly_half_the_vector() {
        let (dx, dy, _, out) = swallow_step(armed((100.0, 0.0)), 50.0, 7.0);
        assert_eq!(out, Outcome::Recognized);
        assert_eq!((dx, dy), (-50.0, 7.0));
        let (.., out) = swallow_step(armed((100.0, 0.0)), 49.9, 7.0);
        assert_eq!(out, Outcome::Pass);
    }

    #[test]
    fn a_zero_vector_arm_can_never_recognize_and_expires_silently() {
        let mut sw = armed((0.0, 0.0));
        for _ in 0..2 {
            let (dx, dy, next, out) = swallow_step(sw, 500.0, 500.0);
            assert_eq!(out, Outcome::Pass);
            assert_eq!((dx, dy), (500.0, 500.0));
            sw = next.expect("still armed");
        }
        let (.., out) = swallow_step(sw, 500.0, 500.0);
        assert_eq!(
            out,
            Outcome::Expired {
                skipped: false,
                seen: (1500.0, 1500.0)
            }
        );
    }

    #[test]
    fn a_warp_that_lands_within_float_dust_is_a_hit_and_a_clamp_is_a_miss() {
        // Exact landing and sub-point dust pass; the smallest clamp the window server can
        // apply (whole points, onto a neighbouring display — hundreds of points in practice)
        // fails. Pins LANDING_TOLERANCE so it is retuned here deliberately.
        let aimed = NSPoint::new(2519.5, 40.0);
        assert_eq!(landing_verdict(aimed, aimed), Ok(()));
        assert_eq!(landing_verdict(aimed, NSPoint::new(2520.0, 40.4)), Ok(()));
        // The floor the readback applies: (1746.9, 259.8) came back (1746.0, 259.0) live.
        assert_eq!(
            landing_verdict(NSPoint::new(1746.9, 259.8), NSPoint::new(1746.0, 259.0)),
            Ok(())
        );
        assert_eq!(landing_verdict(aimed, NSPoint::new(2519.5, 42.0)), Err(2.0));
        assert!(landing_verdict(aimed, NSPoint::new(-1500.0, 900.0)).is_err());
    }
}
