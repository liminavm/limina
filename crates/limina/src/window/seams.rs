// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Which of the captured display's seams the pointer may actually cross.
//!
//! A captured pointer moves by stepping a position in the absolute device's range, and the
//! guest decides which of its monitors that value lands on. Where every host panel shows a
//! fullscreen guest window, that is exactly right: the hand crosses a seam and the picture it
//! is looking at continues. Where it is not, the guest's cursor walks onto a display the user
//! cannot see — the grab loses the window that justified it
//! ([`super::grab_policy::must_drop_grab`]), the pointer comes back at the park, and the
//! cursor appears to teleport (reproduced on a two-panel host with the built-in's Space swiped
//! away, 2026-08-24).
//!
//! So a seam is crossable only when **the display on the other side is one the hand can see,
//! and is the same one on both sides of the mapping**:
//!
//! - the slot the *range* leads to past this side, and
//! - the slot the *host panel* adjacent on this side is showing,
//!
//! must be the same slot, and it must be covered — a guest window that is fullscreen, on its
//! panel's active Space, and on a screen at all. Anything else is an edge: the range is held
//! at this slot's own share and the fullscreen grab's edge press
//! ([`super::grab_policy::press_step`]) does the rest, handing the pointer out onto the panel
//! beyond once the user pushes and means it.
//!
//! **Why both questions and not just coverage.** Host and guest arrangements disagree
//! routinely — the relay places only a fresh set at session start, mutter ≥ 50 will not
//! rearrange a live session, and the stock tier has no `suggested X/Y` at all. With host
//! panels `A B C` and a guest that ordered them `A C B`, asking coverage alone about A's right
//! side asks about B while the range actually leads to C: the pointer reaches C whatever B's
//! answer was. Comparing the two neighbours is what makes "a crossing lands where the hand is
//! pointing" true rather than incidental, and it costs one lookup.
//!
//! **The unknown answer is an edge.** A neighbour we cannot name — no fit for it yet, no
//! window, no panel — is refused. Resistance at a seam that should have been crossable is a
//! recoverable surprise the user can fix by arranging the guest to match; a crossing onto a
//! display that is not there is the bug this module exists to stop. (Our *own* share unknown
//! is the one case with no answer at all: there is nothing to hold the range at, so the step
//! keeps its old behaviour until the fit converges.)
//!
//! **A held seam charges the guest nothing.** The eaten motion is dropped, not forwarded as
//! edge pressure: the guest's desktop really does continue past a seam, so relative motion
//! there walks its pointer onto the neighbour while the absolute device snaps it back — two
//! devices fighting over one pointer. Pressure belongs to the desktop's real outer edges
//! ([`super::arrangement::outer_edges_at`]) and to nowhere else.

use super::arrangement::{Edges, RangeRect};

/// Shares whose facing coordinates lie within this many range units still abut. The shares are
/// fitted from the guest's cursor echo, so exact abutment is not to be expected; well under a
/// display's width and well over the fit's own noise.
const RANGE_TOL: f64 = 64.0;

/// Host panel frames within this many points still abut — float drift, and the small height
/// differences our own mode adjustments introduce.
const POINT_TOL: f64 = 2.0;

/// One host panel's frame in a top-left-origin, y-down point space (CoreGraphics global
/// bounds, which are already in it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelRect {
    pub(crate) x0: f64,
    pub(crate) y0: f64,
    pub(crate) x1: f64,
    pub(crate) y1: f64,
}

/// What the seam rule needs to know about one guest display slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SlotFacts {
    pub(crate) slot: usize,
    /// This slot's share of the absolute device's range — the guest's report where there is
    /// one, else the line fitted from its cursor echo. `None` until either exists.
    pub(crate) share: Option<RangeRect>,
    /// The host panel this slot's window covers. `None` when it has no window or no screen.
    pub(crate) panel: Option<PanelRect>,
    /// The window covering that panel is a fullscreen guest on the Space the user is looking
    /// at — the whole of "the hand can see this display".
    pub(crate) covered: bool,
}

/// Which side of a slot a question is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Where the captured range may go this step: the capture slot's own share, and which of its
/// sides the pointer may leave.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hold {
    /// The capture slot's share, when it is known. `None` holds nothing.
    pub(crate) share: Option<RangeRect>,
    /// `true` where the pointer may cross into the neighbour.
    pub(crate) cross: Edges,
}

impl Hold {
    /// Nothing held — every side crossable, no share to hold at. What a slot with no known
    /// share gets, and what a single-display session is.
    pub(crate) const OPEN: Hold = Hold {
        share: None,
        cross: Edges::ALL,
    };

    /// The hold for `slot` with the range position at `at`.
    pub(crate) fn of(slots: &[SlotFacts], slot: usize, at: (f64, f64)) -> Hold {
        let Some(own) = slots.iter().find(|s| s.slot == slot) else {
            return Hold::OPEN;
        };
        let Some(share) = own.share else {
            return Hold::OPEN;
        };
        Hold {
            share: Some(share),
            cross: Edges {
                left: crossable(slots, own, at, Side::Left),
                right: crossable(slots, own, at, Side::Right),
                top: crossable(slots, own, at, Side::Top),
                bottom: crossable(slots, own, at, Side::Bottom),
            },
        }
    }

    /// Pull a candidate range position back inside the sides it may not leave.
    ///
    /// Silently: a seam is not a wall, and what it eats is charged to nobody (see the module
    /// note). The fullscreen grab's edge press is what turns a sustained push here into a
    /// release, and it reads the *fit*, which is clamped at the window's own content edge
    /// whatever the range does.
    pub(crate) fn apply(&self, cand: (f64, f64)) -> (f64, f64) {
        let Some(r) = self.share else {
            return cand;
        };
        let x = cand.0.clamp(
            if self.cross.left { f64::MIN } else { r.x0 },
            if self.cross.right { f64::MAX } else { r.x1 },
        );
        let y = cand.1.clamp(
            if self.cross.top { f64::MIN } else { r.y0 },
            if self.cross.bottom { f64::MAX } else { r.y1 },
        );
        (x, y)
    }
}

/// May the pointer cross `own`'s `side`? Both neighbours must name the same slot, and it must
/// be covered.
fn crossable(slots: &[SlotFacts], own: &SlotFacts, at: (f64, f64), side: Side) -> bool {
    let (Some(range), Some(host)) = (
        range_neighbour(slots, own, at, side),
        host_neighbour(slots, own, at, side),
    ) else {
        return false;
    };
    range == host && slots.iter().any(|s| s.slot == range && s.covered)
}

/// The slot the *range* leads to past `side`, at the cross-axis position `at` names.
///
/// An ordering query rather than a containment test at a probe point: the shares are fitted,
/// so neighbours abut approximately and a point just past the seam can fall in the gap between
/// two of them.
fn range_neighbour(
    slots: &[SlotFacts],
    own: &SlotFacts,
    at: (f64, f64),
    side: Side,
) -> Option<usize> {
    let r = own.share?;
    let mut best: Option<(f64, usize)> = None;
    for s in slots.iter().filter(|s| s.slot != own.slot) {
        let Some(c) = s.share else { continue };
        let (key, ok) = match side {
            // The cross axis must actually overlap where the pointer is: a neighbour dropped
            // clear of this row is not on the other side of the seam *here*.
            Side::Left | Side::Right => {
                let spans = c.y0 - RANGE_TOL <= at.1 && at.1 <= c.y1 + RANGE_TOL;
                match side {
                    Side::Left => (-c.x1, spans && c.x1 <= r.x0 + RANGE_TOL),
                    _ => (c.x0, spans && c.x0 >= r.x1 - RANGE_TOL),
                }
            }
            Side::Top | Side::Bottom => {
                let spans = c.x0 - RANGE_TOL <= at.0 && at.0 <= c.x1 + RANGE_TOL;
                match side {
                    Side::Top => (-c.y1, spans && c.y1 <= r.y0 + RANGE_TOL),
                    _ => (c.y0, spans && c.y0 >= r.y1 - RANGE_TOL),
                }
            }
        };
        if ok && best.is_none_or(|(b, _)| key < b) {
            best = Some((key, s.slot));
        }
    }
    best.map(|(_, s)| s)
}

/// The slot whose window covers the host panel adjacent on `side`, at the same fraction along
/// the cross axis the range position sits at.
fn host_neighbour(
    slots: &[SlotFacts],
    own: &SlotFacts,
    at: (f64, f64),
    side: Side,
) -> Option<usize> {
    let r = own.share?;
    let p = own.panel?;
    // The hand's position along the seam, carried from range units into the panel's points.
    let along = |lo: f64, hi: f64, v: f64| {
        if hi - lo <= 0.0 {
            0.5
        } else {
            ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
        }
    };
    let (px, py) = (
        p.x0 + along(r.x0, r.x1, at.0) * (p.x1 - p.x0),
        p.y0 + along(r.y0, r.y1, at.1) * (p.y1 - p.y0),
    );
    let mut best: Option<(f64, usize)> = None;
    for s in slots.iter().filter(|s| s.slot != own.slot) {
        let Some(c) = s.panel else { continue };
        let (key, ok) = match side {
            Side::Left | Side::Right => {
                let spans = c.y0 - POINT_TOL <= py && py <= c.y1 + POINT_TOL;
                match side {
                    Side::Left => (-c.x1, spans && c.x1 <= p.x0 + POINT_TOL),
                    _ => (c.x0, spans && c.x0 >= p.x1 - POINT_TOL),
                }
            }
            Side::Top | Side::Bottom => {
                let spans = c.x0 - POINT_TOL <= px && px <= c.x1 + POINT_TOL;
                match side {
                    Side::Top => (-c.y1, spans && c.y1 <= p.y0 + POINT_TOL),
                    _ => (c.y0, spans && c.y0 >= p.y1 - POINT_TOL),
                }
            }
        };
        if ok && best.is_none_or(|(b, _)| key < b) {
            best = Some((key, s.slot));
        }
    }
    best.map(|(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: f64 = 32767.0;

    fn share(x0: f64, x1: f64) -> Option<RangeRect> {
        Some(RangeRect {
            x0,
            y0: 0.0,
            x1,
            y1: RANGE,
        })
    }

    fn panel(x0: f64, x1: f64) -> Option<PanelRect> {
        Some(PanelRect {
            x0,
            y0: 0.0,
            x1,
            y1: 1440.0,
        })
    }

    /// Host `A B` left to right, guest range in the same order, both covered.
    fn agreeing_pair() -> Vec<SlotFacts> {
        vec![
            SlotFacts {
                slot: 0,
                share: share(0.0, 16000.0),
                panel: panel(0.0, 2560.0),
                covered: true,
            },
            SlotFacts {
                slot: 1,
                share: share(16000.0, RANGE),
                panel: panel(2560.0, 5584.0),
                covered: true,
            },
        ]
    }

    /// Host `A B C`, guest range `A C B` — the disagreement the agreement test exists for.
    fn disagreeing_trio() -> Vec<SlotFacts> {
        vec![
            SlotFacts {
                slot: 0,
                share: share(0.0, 10000.0),
                panel: panel(0.0, 2560.0),
                covered: true,
            },
            // Guest put slot 2 in the middle of the range; the host shows it on the RIGHT.
            SlotFacts {
                slot: 2,
                share: share(10000.0, 21000.0),
                panel: panel(5120.0, 7680.0),
                covered: true,
            },
            SlotFacts {
                slot: 1,
                share: share(21000.0, RANGE),
                panel: panel(2560.0, 5120.0),
                covered: true,
            },
        ]
    }

    fn mid() -> (f64, f64) {
        (8000.0, RANGE / 2.0)
    }

    /// THE regression guard the user asked for: with every panel covered and the two
    /// arrangements agreeing, this module must be invisible. Nothing new may be held, and no
    /// side may become an edge — that is the shipped behaviour, and it is not what changed.
    #[test]
    fn all_covered_and_agreeing_holds_nothing() {
        let slots = agreeing_pair();
        let h = Hold::of(&slots, 0, mid());
        assert!(
            h.cross.right,
            "the seam to a covered neighbour is crossable"
        );
        let past = (20000.0, RANGE / 2.0);
        assert_eq!(h.apply(past), past, "a crossable seam must not clamp");

        // And from the other side, so this is not an artefact of being the first slot.
        let h = Hold::of(&slots, 1, (20000.0, RANGE / 2.0));
        assert!(h.cross.left);
        assert_eq!(h.apply(mid()), mid());
    }

    /// The reproduced bug: the neighbour's Space is swiped away, so the seam is an edge and
    /// the range stops at this slot's own share.
    #[test]
    fn a_neighbour_the_hand_cannot_see_is_an_edge() {
        let mut slots = agreeing_pair();
        slots[1].covered = false;
        let h = Hold::of(&slots, 0, mid());
        assert!(!h.cross.right);
        assert_eq!(h.apply((20000.0, RANGE / 2.0)), (16000.0, RANGE / 2.0));
    }

    /// A panel with no limina window at all is the same answer as a hidden one.
    #[test]
    fn a_panel_with_no_window_is_an_edge() {
        let mut slots = agreeing_pair();
        slots[1].panel = None;
        slots[1].covered = false;
        let h = Hold::of(&slots, 0, mid());
        assert!(!h.cross.right);
    }

    /// Host `A B C`, guest `A C B`, EVERY panel covered: coverage alone would cross A's right
    /// seam, and the pointer would land on the panel the host shows on the far right — a jump
    /// over B. The agreement test refuses it.
    #[test]
    fn a_crossing_that_would_jump_a_panel_is_an_edge() {
        let slots = disagreeing_trio();
        let h = Hold::of(&slots, 0, (5000.0, RANGE / 2.0));
        assert_eq!(
            super::range_neighbour(&slots, &slots[0], (5000.0, RANGE / 2.0), Side::Right),
            Some(2),
            "the range leads to the slot the host shows on the far right"
        );
        assert_eq!(
            super::host_neighbour(&slots, &slots[0], (5000.0, RANGE / 2.0), Side::Right),
            Some(1),
            "the host's own neighbour is the middle panel"
        );
        assert!(!h.cross.right, "the two disagree, so it is an edge");
        assert_eq!(h.apply((12000.0, RANGE / 2.0)), (10000.0, RANGE / 2.0));
    }

    /// The unknown answer is an edge: a neighbour whose share has not been fitted yet cannot
    /// be named, so the crossing is refused rather than guessed.
    #[test]
    fn an_unfitted_neighbour_is_an_edge() {
        let mut slots = agreeing_pair();
        slots[1].share = None;
        let h = Hold::of(&slots, 0, mid());
        assert!(!h.cross.right);
    }

    /// Our OWN share unknown is the one case with no answer: there is nothing to hold the
    /// range at, so the step must keep its old behaviour rather than invent a boundary.
    #[test]
    fn our_own_share_unknown_holds_nothing() {
        let mut slots = agreeing_pair();
        slots[0].share = None;
        let h = Hold::of(&slots, 0, mid());
        assert_eq!(h, Hold::OPEN);
        assert_eq!(h.apply((99999.0, 0.0)), (99999.0, 0.0));
    }

    /// A real outer edge — nothing on the other side at all — is an edge here too, and the
    /// clamp is what keeps the pointer on the desktop.
    #[test]
    fn the_desktops_own_outer_edge_is_an_edge() {
        let slots = agreeing_pair();
        let h = Hold::of(&slots, 0, mid());
        assert!(!h.cross.left, "nothing is left of the first display");
        assert_eq!(h.apply((-500.0, RANGE / 2.0)), (0.0, RANGE / 2.0));
    }

    /// A neighbour dropped clear of this row is not on the other side of the seam *here*: the
    /// cross-axis overlap is part of the question, as it is for `outer_edges_at`.
    #[test]
    fn a_neighbour_that_does_not_span_this_row_is_not_a_neighbour() {
        let mut slots = agreeing_pair();
        slots[1].share = Some(RangeRect {
            x0: 16000.0,
            y0: 25000.0,
            x1: RANGE,
            y1: RANGE,
        });
        let h = Hold::of(&slots, 0, (8000.0, 5000.0));
        assert!(!h.cross.right, "the neighbour is below this row");
        let h = Hold::of(&slots, 0, (8000.0, 28000.0));
        assert!(h.cross.right, "and beside it further down");
    }

    /// Vertical arrangements are the same question on the other axis.
    #[test]
    fn a_panel_stacked_below_is_judged_the_same_way() {
        let stacked = |covered| {
            vec![
                SlotFacts {
                    slot: 0,
                    share: Some(RangeRect {
                        x0: 0.0,
                        y0: 0.0,
                        x1: RANGE,
                        y1: 16000.0,
                    }),
                    panel: Some(PanelRect {
                        x0: 0.0,
                        y0: 0.0,
                        x1: 2560.0,
                        y1: 1440.0,
                    }),
                    covered: true,
                },
                SlotFacts {
                    slot: 1,
                    share: Some(RangeRect {
                        x0: 0.0,
                        y0: 16000.0,
                        x1: RANGE,
                        y1: RANGE,
                    }),
                    panel: Some(PanelRect {
                        x0: 0.0,
                        y0: 1440.0,
                        x1: 2560.0,
                        y1: 2880.0,
                    }),
                    covered,
                },
            ]
        };
        assert!(
            Hold::of(&stacked(true), 0, (RANGE / 2.0, 8000.0))
                .cross
                .bottom
        );
        assert!(
            !Hold::of(&stacked(false), 0, (RANGE / 2.0, 8000.0))
                .cross
                .bottom
        );
    }

    /// The incident this module was written for, in its own measured numbers (stock F44 guest,
    /// 2026-08-24). The host has the built-in LEFT of and below the BenQ; the guest ordered it
    /// RIGHT, at logical (2048,0) of a 3560x1152 box, and its Space was swiped away. The
    /// captured pointer pushed right off the BenQ, the range walked into the built-in's share,
    /// the guest crossed, the grab died against a window on no visible Space and the cursor
    /// snapped back to the park.
    ///
    /// Both seams are edges here, and for different reasons: nothing is to the RIGHT on the
    /// host at all, and the host's LEFT neighbour is a slot the range does not lead to. So the
    /// captured pointer cannot leave the BenQ, which is correct — the way to the built-in is
    /// the grab's release and a fresh grab over there.
    #[test]
    fn the_2026_08_24_teleport_cannot_happen_again() {
        let box_w = 3560.0;
        let range_x = |logical: f64| logical / box_w * RANGE;
        let slots = vec![
            SlotFacts {
                slot: 0,
                share: Some(RangeRect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: range_x(2048.0),
                    y1: RANGE,
                }),
                panel: Some(PanelRect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 2560.0,
                    y1: 1440.0,
                }),
                covered: true,
            },
            SlotFacts {
                slot: 1,
                share: Some(RangeRect {
                    x0: range_x(2048.0),
                    y0: 0.0,
                    x1: RANGE,
                    y1: 948.0 / 1152.0 * RANGE,
                }),
                panel: Some(PanelRect {
                    x0: -1512.0,
                    y0: 747.0,
                    x1: 0.0,
                    y1: 1729.0,
                }),
                // The built-in's Space was swiped away. The assertions below hold either way,
                // which is the point: the arrangements disagree, so neither seam is crossable.
                covered: false,
            },
        ];
        let at = (18000.0, 17411.0);
        let h = Hold::of(&slots, 0, at);
        assert!(!h.cross.right, "no host panel is to the right of the BenQ");
        assert!(
            !h.cross.left,
            "the host's left neighbour is not where the range leads"
        );
        // The exact step that crossed in the trace: wire x walked 19124 -> 19488, past the
        // built-in's share edge at 18848. It must stop at the BenQ's own.
        let held = h.apply((19488.0, 17411.0));
        assert!(
            held.0 <= range_x(2048.0) + 0.5,
            "the range must not reach the hidden slot's share: {held:?}"
        );

        // And with the built-in's Space back, the answer does not change — the two
        // arrangements still disagree, so the crossing is still refused rather than teleporting
        // the hand from the BenQ's right edge onto a panel that is off to the left.
        let mut covered = slots.clone();
        covered[1].covered = true;
        let h = Hold::of(&covered, 0, at);
        assert!(!h.cross.right);
        assert!(!h.cross.left);
    }

    /// Single display: the share is the whole range, nothing neighbours it, and the clamp is a
    /// no-op inside it. Holding must never cost a one-display session anything.
    #[test]
    fn one_display_is_held_by_its_own_extent_and_nothing_else() {
        let slots = vec![SlotFacts {
            slot: 0,
            share: share(0.0, RANGE),
            panel: panel(0.0, 2560.0),
            covered: true,
        }];
        let h = Hold::of(&slots, 0, mid());
        assert_eq!(h.apply(mid()), mid());
        assert_eq!(h.apply((RANGE + 10.0, 0.0)), (RANGE, 0.0));
    }
}
