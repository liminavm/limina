// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The guest's cursor echo: where the guest says its pointer is, per scanout — the pointer
//! stack's one external oracle, and what its expectations are asserted against.
//!
//! We tell the guest where its pointer is (the absolute device) and the guest tells us back
//! where it put its cursor plane (`cursormove`/`cursorhide`, one plane enabled on exactly one
//! CRTC). Every host-side decision — which window a press is judged in, where the park sits,
//! where the release warps, which slot's cursor is worn — rests on the two agreeing. They
//! stopped agreeing once before without any host-side check noticing: the host's layout
//! guess put the pointer on one display and the guest on another, so the sprite (which
//! follows the echo) was right while every trigger (which followed the guess) fired on the
//! wrong screen. The checks here compare the two directly and crash on disagreement
//! (`assert!`, by design — the user wants a wrong-screen pointer loud, not logged).
//!
//! What the guest reports is the plane's **origin**, i.e. the pointer minus the hotspot
//! (measured 2026-08-21: a pointer pinned at the left edge with a (4,1) hotspot echoes
//! `x = -4`; `(276.3, 72.1)` sent at 1:1 echoed `(272, 71)`). [`CursorEcho`] stores the
//! pointer, hotspot added back.

use std::sync::Mutex;

use super::present::MAX_SCANOUTS;

/// One scanout's cursor plane as last echoed by the guest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CursorEcho {
    /// Whether the guest has its cursor plane enabled on this scanout.
    pub(crate) visible: bool,
    /// The pointer's pixel on this scanout (plane origin plus hotspot).
    pub(crate) x: i32,
    pub(crate) y: i32,
    /// The scanout's size when the echo arrived; zero before the guest brought a mode up.
    pub(crate) w: u32,
    pub(crate) h: u32,
    /// Which of our sends was the latest when this echo arrived ([`note_send`]). An echo
    /// stamped with our send's number answers *that* send: nothing else went out in between.
    /// This is what replaces waiting a fixed settle for the guest to catch up.
    pub(crate) seq: u64,
}

impl CursorEcho {
    const NONE: CursorEcho = CursorEcho {
        visible: false,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        seq: 0,
    };
}

/// Written by the control-channel reader thread, read by the main thread.
static ECHO: Mutex<[CursorEcho; MAX_SCANOUTS]> = Mutex::new([CursorEcho::NONE; MAX_SCANOUTS]);

/// How many positions we have put on the absolute device. Bumped by the sender (main thread),
/// read by the echo reader (control-channel thread) to stamp what it publishes.
static SEND_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// We are about to send a position: the number this send will be known by.
pub(crate) fn note_send() -> u64 {
    SEND_SEQ.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1
}

/// Where the guest's cursor is, when exactly one plane is showing it — the unambiguous
/// reading. Several planes lit at once is a sprite straddling a seam (or a stale plane) and
/// says nothing definite about where the pointer is, so it is not a point.
pub(crate) fn shown_point(echo: &[CursorEcho; MAX_SCANOUTS]) -> Option<(usize, i32, i32)> {
    let mut shown = echo
        .iter()
        .enumerate()
        .filter(|(_, c)| c.visible && c.w > 0 && c.h > 0);
    let (slot, c) = shown.next()?;
    shown.next().is_none().then_some((slot, c.x, c.y))
}

/// Has the guest answered send number `seq` yet?
///
/// The answer is the first echo that is *stamped* with that send — meaning nothing was sent
/// between our position and the guest's reply — and that says something *different* from where
/// the cursor already was. The difference matters: an echo can be in flight from before the
/// send and still be stamped with it, and pairing that stale position with the new send is
/// exactly the kind of sample that teaches a wrong mapping. A send that asks for the pixel the
/// cursor is already on therefore never settles, which is right — it carries no information.
pub(crate) fn settled(
    seq: u64,
    before: Option<(usize, i32, i32)>,
    echo: &[CursorEcho; MAX_SCANOUTS],
) -> Option<(usize, i32, i32)> {
    let point = shown_point(echo)?;
    (echo[point.0].seq == seq && Some(point) != before).then_some(point)
}

/// The guest echoed a cursor state for `slot`: `plane` is the plane origin as reported,
/// `hot` the shape's hotspot, `scanout` that slot's current mode.
pub(crate) fn publish(
    slot: usize,
    visible: bool,
    plane: (i32, i32),
    hot: (u32, u32),
    scanout: (u32, u32),
) {
    let mut all = ECHO.lock().unwrap();
    let Some(e) = all.get_mut(slot) else {
        return;
    };
    *e = CursorEcho {
        visible,
        x: plane.0.saturating_add(hot.0 as i32),
        y: plane.1.saturating_add(hot.1 as i32),
        w: scanout.0,
        h: scanout.1,
        seq: SEND_SEQ.load(std::sync::atomic::Ordering::Acquire),
    };
}

/// The guest is gone (a reboot, a fresh worker): nothing it said still holds.
/// Every slot's scanout size in guest pixels, from the worker's `surface` lines — the mode
/// sizes the captured cursor's range gain is built from (`fit::range_gain`).
static SCANOUT: Mutex<[(u32, u32); MAX_SCANOUTS]> = Mutex::new([(0, 0); MAX_SCANOUTS]);

pub(crate) fn publish_scanout(slot: usize, w: u32, h: u32) {
    let changed = {
        let mut all = SCANOUT.lock().unwrap();
        match all.get_mut(slot) {
            Some(s) if *s != (w, h) => {
                *s = (w, h);
                true
            }
            _ => false,
        }
    };
    // Every slot's share of the absolute device is measured against the desktop this mode is
    // part of, so one slot's modeset invalidates them all (`window/absfit.rs`). The lock is
    // dropped first: the fit reads these sizes.
    if changed {
        super::absfit::forget();
    }
}

pub(crate) fn scanout_sizes() -> [(u32, u32); MAX_SCANOUTS] {
    *SCANOUT.lock().unwrap()
}

pub(crate) fn forget() {
    *SCANOUT.lock().unwrap() = [(0, 0); MAX_SCANOUTS];
    *ECHO.lock().unwrap() = [CursorEcho::NONE; MAX_SCANOUTS];
    super::absfit::forget();
}

/// Every scanout's echo, as of now.
pub(crate) fn snapshot() -> [CursorEcho; MAX_SCANOUTS] {
    *ECHO.lock().unwrap()
}

/// How far the guest's pointer may sit from where we put it, in scanout pixels, before the
/// two are judged to disagree. The guest rounds our `0..=ABS_MAX` position into logical
/// coordinates and the plane into device pixels at its scale (1.25 on the rig's BenQ), so
/// the honest miss is a pixel or two; the faults this catches are a whole display, or the
/// guest's own clamp (whole pixels at the least, and in practice the far side of a seam).
pub(crate) const TOLERANCE_PX: f64 = 3.0;

/// Where the guest's pointer is, relative to where we sent it: the pure verdict.
///
/// `expect` is the pixel we sent on `slot` (a unit position times that scanout's size);
/// `echo` is every slot's plane. `Ok(None)` means the guest is showing no cursor at all —
/// it hid its pointer (mouselook, a pointer-lock game) and where it is is the guest's
/// business. `Ok(Some(miss))` is agreement within [`TOLERANCE_PX`]. `Err` is the fault,
/// spelled out: the cursor on another slot entirely, or on the right slot but elsewhere.
pub(crate) fn verdict(
    slot: usize,
    expect: (f64, f64),
    echo: &[CursorEcho; MAX_SCANOUTS],
) -> Result<Option<f64>, String> {
    let e = echo[slot];
    let elsewhere: Vec<(usize, (i32, i32))> = echo
        .iter()
        .enumerate()
        .filter(|(s, c)| *s != slot && c.visible)
        .map(|(s, c)| (s, (c.x, c.y)))
        .collect();
    if !e.visible {
        return if elsewhere.is_empty() {
            Ok(None)
        } else {
            Err(format!(
                "we sent the pointer to slot {slot} at ({:.1},{:.1}) but the guest shows its cursor on {elsewhere:?} and none on slot {slot}",
                expect.0, expect.1
            ))
        };
    }
    if e.w == 0 || e.h == 0 {
        return Ok(None);
    }
    let miss = (f64::from(e.x) - expect.0).hypot(f64::from(e.y) - expect.1);
    if miss <= TOLERANCE_PX {
        Ok(Some(miss))
    } else {
        Err(format!(
            "we sent the pointer to slot {slot} at ({:.1},{:.1}) on a {}x{} scanout but the guest's cursor there sits at ({},{}) — {miss:.1} px off{}",
            expect.0,
            expect.1,
            e.w,
            e.h,
            e.x,
            e.y,
            if elsewhere.is_empty() {
                String::new()
            } else {
                format!("; other slots also showing a cursor: {elsewhere:?}")
            }
        ))
    }
}

/// The pixel a unit position names on a scanout of this size — the expectation
/// [`verdict`] judges, computed in one place so the sender and the checker cannot differ.
/// The slot the guest's cursor is on right now: `prefer` when a plane is visible there (a
/// sprite straddling a seam is on two planes, and the one we sent is the one we mean),
/// otherwise the first slot showing one, `None` when the guest shows no cursor at all.
pub(crate) fn shown_slot(prefer: usize, echo: &[CursorEcho; MAX_SCANOUTS]) -> Option<usize> {
    if echo[prefer].visible {
        return Some(prefer);
    }
    echo.iter().position(|c| c.visible)
}

pub(crate) fn expected_pixel(unit: (f64, f64), scanout: (u32, u32)) -> (f64, f64) {
    (
        unit.0.clamp(0.0, 1.0) * f64::from(scanout.0),
        unit.1.clamp(0.0, 1.0) * f64::from(scanout.1),
    )
}

/// Is the guest's pointer pinned at this edge of `slot`'s scanout? The grab's release
/// presses at an edge for a sustained hold, so at release time the guest's cursor must be
/// at that edge of the scanout the capture window shows — not across a seam on the next
/// one. Same shape of answer as [`verdict`].
pub(crate) fn at_edge_verdict(
    slot: usize,
    edge: super::fit::Edge,
    echo: &[CursorEcho; MAX_SCANOUTS],
) -> Result<Option<f64>, String> {
    let e = echo[slot];
    if !e.visible || e.w == 0 || e.h == 0 {
        // The same hidden/elsewhere distinction as `verdict`, judged at the plane's own pixel.
        return verdict(slot, (f64::from(e.x), f64::from(e.y)), echo);
    }
    let (coord, edge_px, axis) = match edge {
        super::fit::Edge::Left => (e.x, 0, "x"),
        super::fit::Edge::Right => (e.x, e.w as i32, "x"),
        super::fit::Edge::Top => (e.y, 0, "y"),
        super::fit::Edge::Bottom => (e.y, e.h as i32, "y"),
    };
    let miss = f64::from((coord - edge_px).abs());
    if miss <= TOLERANCE_PX {
        Ok(Some(miss))
    } else {
        Err(format!(
            "releasing at the {edge:?} edge of slot {slot} ({}x{}), but the guest's cursor there is at ({},{}) — {axis} is {miss:.0} px from the edge",
            e.w, e.h, e.x, e.y
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig(slot0: CursorEcho, slot1: CursorEcho) -> [CursorEcho; MAX_SCANOUTS] {
        let mut all = [CursorEcho::NONE; MAX_SCANOUTS];
        all[0] = slot0;
        all[1] = slot1;
        all
    }

    fn shown(x: i32, y: i32, w: u32, h: u32) -> CursorEcho {
        CursorEcho {
            visible: true,
            x,
            y,
            w,
            h,
            seq: 0,
        }
    }

    /// The echo is the plane origin; the stored pointer adds the hotspot back (the measured
    /// `x = -4` at the left edge with a (4,1) hotspot is the pointer at 0).
    #[test]
    fn the_stored_pointer_is_the_plane_origin_plus_the_hotspot() {
        publish(3, true, (-4, 67), (4, 1), (2560, 1440));
        let e = snapshot()[3];
        assert_eq!((e.x, e.y, e.w, e.h, e.visible), (0, 68, 2560, 1440, true));
        forget();
        assert_eq!(snapshot()[3], CursorEcho::NONE);
    }

    #[test]
    fn agreement_within_a_few_pixels_is_a_hit() {
        let echo = rig(shown(276, 72, 2560, 1440), CursorEcho::NONE);
        let expect = expected_pixel((276.3 / 2560.0, 72.1 / 1440.0), (2560, 1440));
        assert!(matches!(verdict(0, expect, &echo), Ok(Some(m)) if m < 1.0));
        // Just past the tolerance on one axis.
        assert!(verdict(0, (280.0, 72.0), &echo).is_err());
    }

    /// THE fault this exists for: we sent slot 0, the guest put its cursor on slot 1.
    #[test]
    fn a_cursor_on_another_slot_is_the_fault_not_a_hidden_pointer() {
        let echo = rig(CursorEcho::NONE, shown(10, 10, 1512, 980));
        let err = verdict(0, (100.0, 100.0), &echo).unwrap_err();
        assert!(err.contains("on [(1, (10, 10))]"), "{err}");
        assert!(err.contains("none on slot 0"), "{err}");
    }

    /// No cursor anywhere is the guest's own hide (mouselook): not ours to judge.
    #[test]
    fn the_shown_slot_prefers_the_sent_one_and_falls_back_to_any_visible_plane() {
        let both = rig(shown(10, 10, 100, 100), shown(5, 5, 100, 100));
        assert_eq!(shown_slot(0, &both), Some(0));
        assert_eq!(shown_slot(1, &both), Some(1));
        let only_one = rig(CursorEcho::NONE, shown(5, 5, 100, 100));
        assert_eq!(shown_slot(0, &only_one), Some(1));
        assert_eq!(
            shown_slot(0, &rig(CursorEcho::NONE, CursorEcho::NONE)),
            None
        );
    }

    #[test]
    fn a_hidden_pointer_is_skipped() {
        let echo = rig(CursorEcho::NONE, CursorEcho::NONE);
        assert_eq!(verdict(0, (100.0, 100.0), &echo), Ok(None));
        // A visible plane on a scanout with no mode yet cannot be judged either.
        let echo = rig(shown(5, 5, 0, 0), CursorEcho::NONE);
        assert_eq!(verdict(0, (100.0, 100.0), &echo), Ok(None));
    }

    /// A stale plane on another slot beside the right one (mutter's ghost-cursor defect)
    /// is reported in the message but is not itself the fault.
    #[test]
    fn a_stale_plane_elsewhere_does_not_fail_an_agreeing_slot() {
        let echo = rig(shown(100, 100, 2560, 1440), shown(700, 700, 1512, 980));
        assert!(verdict(0, (100.0, 100.0), &echo).is_ok());
        let err = verdict(0, (900.0, 100.0), &echo).unwrap_err();
        assert!(err.contains("other slots also showing"), "{err}");
    }

    #[test]
    fn the_release_edge_is_where_the_guest_cursor_is() {
        use super::super::fit::Edge;
        let echo = rig(shown(0, 500, 2560, 1440), CursorEcho::NONE);
        assert!(at_edge_verdict(0, Edge::Left, &echo).is_ok());
        assert!(at_edge_verdict(0, Edge::Right, &echo).is_err());
        let echo = rig(shown(2559, 1439, 2560, 1440), CursorEcho::NONE);
        assert!(at_edge_verdict(0, Edge::Right, &echo).is_ok());
        assert!(at_edge_verdict(0, Edge::Bottom, &echo).is_ok());
        assert!(at_edge_verdict(0, Edge::Top, &echo).is_err());
        // Pinned at the seam on the WRONG scanout: the pointer crossed where we believed it
        // was pressing — the other-slot branch names it.
        let echo = rig(CursorEcho::NONE, shown(3, 500, 1512, 980));
        assert!(at_edge_verdict(0, Edge::Right, &echo)
            .unwrap_err()
            .contains("none on slot 0"));
        // Hidden everywhere: skipped.
        assert_eq!(
            at_edge_verdict(0, Edge::Right, &rig(CursorEcho::NONE, CursorEcho::NONE)),
            Ok(None)
        );
    }
}

#[cfg(test)]
mod settle_tests {
    use super::*;

    fn lit(x: i32, y: i32, seq: u64) -> CursorEcho {
        CursorEcho {
            visible: true,
            x,
            y,
            w: 2560,
            h: 1440,
            seq,
        }
    }

    fn board(slots: &[(usize, CursorEcho)]) -> [CursorEcho; MAX_SCANOUTS] {
        let mut b = [CursorEcho::NONE; MAX_SCANOUTS];
        for (s, e) in slots {
            b[*s] = *e;
        }
        b
    }

    #[test]
    fn an_echo_from_before_the_send_is_not_an_answer_to_it() {
        // Stamped with our send (nothing went out in between) but carrying the position the
        // cursor already had: in flight when we sent, and pairing it would teach a wrong slope.
        let before = Some((0usize, 100, 200));
        let b = board(&[(0, lit(100, 200, 7))]);
        assert_eq!(settled(7, before, &b), None);
    }

    #[test]
    fn the_first_moved_echo_stamped_with_the_send_is_the_answer() {
        let before = Some((0usize, 100, 200));
        let b = board(&[(0, lit(940, 530, 7))]);
        assert_eq!(settled(7, before, &b), Some((0, 940, 530)));
        // ...including one that answers on a different display, which is the whole point.
        let b = board(&[(1, lit(30, 44, 7))]);
        assert_eq!(settled(7, before, &b), Some((1, 30, 44)));
    }

    #[test]
    fn an_echo_answering_a_later_send_is_not_ours() {
        let b = board(&[(0, lit(940, 530, 9))]);
        assert_eq!(settled(7, None, &b), None);
    }

    #[test]
    fn two_planes_lit_at_once_is_not_a_position() {
        // A sprite straddling a seam, or a stale plane: nothing definite to measure.
        let b = board(&[(0, lit(2559, 500, 7)), (1, lit(0, 500, 7))]);
        assert_eq!(settled(7, None, &b), None);
        assert_eq!(shown_point(&b), None);
    }

    #[test]
    fn a_guest_showing_no_cursor_has_no_position() {
        assert_eq!(shown_point(&board(&[])), None);
    }
}
