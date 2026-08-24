// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host→guest arrangement relay's geometry: turn the host's panel arrangement into the
//! guest-desktop positions each connector should suggest.
//!
//! The guest consumes these through the DRM `suggested X`/`suggested Y` connector properties,
//! which mutter feeds **verbatim** into its logical monitor layout
//! (`create_preferred_logical_monitor_config`, `meta-monitor-config-manager.c:762`) and then
//! validates hard (`verify_suggested_monitors_config`): any overlap, or any monitor whose
//! rect does not share an *exact* edge with a neighbour (`mtk_rectangle_is_adjacent_to` —
//! integer abutment with strict perpendicular overlap), rejects the whole set and falls back
//! to its linear default. A partial set never forms a config at all
//! (`MONITOR_MATCH_WITH_SUGGESTED_POSITION`). So this function's contract is: **emit an
//! adjacency-exact, overlap-free, all-connectors set in predicted guest-logical units, or
//! emit nothing.**
//!
//! Positions therefore cannot be a unit conversion of the host frames: two host panels abut
//! in *points*, but their guest rects abut in *logical* units whose widths differ from the
//! point widths whenever the guest picks a scale other than the panel's backing factor. The
//! host arrangement supplies the *structure* (who touches whom, on which side, with what
//! cross-axis offset); the predicted logical sizes supply the *metric*. Adjacency is detected
//! in point space with a small tolerance and rebuilt exactly in logical space by walking the
//! adjacency graph.
//!
//! The prediction that makes the metric right: a panel's guest-logical size is its size in
//! host points (hidpi drives the guest at device pixels and mutter picks the backing factor
//! as scale; non-hidpi drives it at points and mutter picks 1 — either way logical ≈ points).
//! Where mutter picks something else (a fractional scale on a fixed-mode connector), the
//! prediction is wrong — and the correction is feedback, not a better guess: the guest's
//! agent reports the compositor's own logical rects, and [`correct_metric`] replaces the
//! predicted sizes with the reported ones before the walk (sizes only — reported *positions*
//! are the user's own arrangement and never feed back). A guest that reports nothing keeps
//! the prediction: correct at whole-number scales, linear fallback otherwise.

/// One panel's share of the host arrangement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placement {
    pub(crate) panel: u64,
    /// The panel's host frame in a shared top-left-origin, y-down point space.
    pub(crate) frame: PointRect,
    /// The predicted guest-logical size of this panel's monitor.
    pub(crate) logical: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PointRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
}

/// Host frames abutting within this many points count as touching. Covers float drift and
/// the small height differences our own mode adjustments introduce (the notch band).
const TOUCH_EPSILON: f64 = 2.0;

/// The guest-desktop position each panel's connector should suggest, or `None` when no clean
/// full set exists (a panel with no neighbour, or a layout that cannot be rebuilt without
/// overlap) — in which case nothing should be emitted, because mutter takes the set whole or
/// not at all.
///
/// A single panel is the trivial clean set at the origin.
pub(crate) fn guest_positions(placements: &[Placement]) -> Option<Vec<(u64, (u32, u32))>> {
    match placements {
        [] => return None,
        [only] => return Some(vec![(only.panel, (0, 0))]),
        _ => {}
    }

    // Walk the point-space adjacency graph, assigning logical positions: a neighbour's
    // position derives from the shared edge (the abutting axis is exact by construction;
    // the cross-axis keeps the point-space offset, logical ≈ points there).
    let mut pos: Vec<Option<(i64, i64)>> = vec![None; placements.len()];
    pos[0] = Some((0, 0));
    let mut queue = vec![0usize];
    while let Some(i) = queue.pop() {
        let (ix, iy) = pos[i].expect("queued placements have positions");
        let a = &placements[i];
        for (j, b) in placements.iter().enumerate() {
            if pos[j].is_some() {
                continue;
            }
            let Some(side) = touching(a.frame, b.frame) else {
                continue;
            };
            let dx = (b.frame.x - a.frame.x).round() as i64;
            let dy = (b.frame.y - a.frame.y).round() as i64;
            pos[j] = Some(match side {
                Side::Right => (ix + i64::from(a.logical.0), iy + dy),
                Side::Left => (ix - i64::from(b.logical.0), iy + dy),
                Side::Below => (ix + dx, iy + i64::from(a.logical.1)),
                Side::Above => (ix + dx, iy - i64::from(b.logical.1)),
            });
            queue.push(j);
        }
    }

    // A panel the walk never reached has no neighbour: mutter would reject the set.
    let pos: Vec<(i64, i64)> = pos.into_iter().collect::<Option<_>>()?;

    // The properties are unsigned: translate the whole desktop to a (0, 0) origin.
    let min_x = pos.iter().map(|p| p.0).min().expect("non-empty");
    let min_y = pos.iter().map(|p| p.1).min().expect("non-empty");
    let pos: Vec<(u32, u32)> = pos
        .iter()
        .map(|&(x, y)| ((x - min_x) as u32, (y - min_y) as u32))
        .collect();

    // The rebuild can create overlap the host never had (logical sizes larger than the point
    // frames they stand in for). Overlap rejects the whole set in the guest; reject it here.
    for (i, a) in placements.iter().enumerate() {
        for (j, b) in placements.iter().enumerate().skip(i + 1) {
            let (ax, ay) = pos[i];
            let (bx, by) = pos[j];
            let x_overlap = ax < bx + b.logical.0 && bx < ax + a.logical.0;
            let y_overlap = ay < by + b.logical.1 && by < ay + a.logical.1;
            if x_overlap && y_overlap {
                log::warn!(
                    "arrangement: panels {:x} and {:x} overlap after the logical rebuild; \
                     suggesting nothing",
                    a.panel,
                    b.panel
                );
                return None;
            }
        }
    }

    // The same shrinkage can break adjacency without overlapping: the rebuild keeps a
    // neighbour's cross-axis *point* offset, which can fall entirely outside the other
    // panel's logical extent. mutter demands every rect share an exact edge with strict
    // perpendicular overlap (`mtk_rectangle_is_adjacent_to`) — mirror that over the final
    // logical rects, so what we emit is exactly what the guest would accept.
    for (i, a) in placements.iter().enumerate() {
        let adjacent = placements
            .iter()
            .enumerate()
            .any(|(j, b)| i != j && logically_adjacent((pos[i], a.logical), (pos[j], b.logical)));
        if !adjacent {
            log::warn!(
                "arrangement: panel {:x} has no exactly-abutting neighbour after the \
                 logical rebuild; suggesting nothing",
                a.panel
            );
            return None;
        }
    }

    Some(
        placements
            .iter()
            .zip(pos)
            .map(|(p, xy)| (p.panel, xy))
            .collect(),
    )
}

/// Replace each placement's *predicted* logical size with the size the guest's compositor
/// actually gave that slot's monitor, where one has been reported.
///
/// The reported size is exact — it is the very rect mutter will validate the suggested set
/// against — so it wins over the prediction unconditionally. Slots the report does not cover
/// (a connector that just came up, a guest with no agent) keep their prediction. Stale sizes
/// (a report from before a mode change) produce at worst one wrong emission and self-heal:
/// the compositor re-reports after every arrangement change, and sizes are stable within a
/// session, so the emission changes at most once per scale change.
pub(crate) fn correct_metric(
    slots: &mut [(u32, Placement)],
    reported: &std::collections::HashMap<usize, (u32, u32)>,
) {
    for (slot, p) in slots.iter_mut() {
        if let Some(&size) = reported.get(&(*slot as usize)) {
            p.logical = size;
        }
    }
}

/// mutter's `mtk_rectangle_is_adjacent_to` over two positioned logical rects: an exact
/// shared edge with *strict* overlap on the perpendicular axis (corner contact fails).
fn logically_adjacent(a: ((u32, u32), (u32, u32)), b: ((u32, u32), (u32, u32))) -> bool {
    let ((ax, ay), (aw, ah)) = a;
    let ((bx, by), (bw, bh)) = b;
    let x_overlap = ax < bx + bw && bx < ax + aw;
    let y_overlap = ay < by + bh && by < ay + ah;
    ((ax + aw == bx || bx + bw == ax) && y_overlap)
        || ((ay + ah == by || by + bh == ay) && x_overlap)
}

enum Side {
    /// `b` is to the right of `a` (b's left edge on a's right edge).
    Right,
    Left,
    Below,
    Above,
}

/// Which side of `a` does `b` abut, in point space — `None` if they don't touch. Corner
/// contact is not touching: mutter's adjacency wants strict overlap on the perpendicular
/// axis, so the structure walk must too.
fn touching(a: PointRect, b: PointRect) -> Option<Side> {
    let x_overlap = overlap(a.x, a.w, b.x, b.w);
    let y_overlap = overlap(a.y, a.h, b.y, b.h);
    if (a.x + a.w - b.x).abs() <= TOUCH_EPSILON && y_overlap {
        Some(Side::Right)
    } else if (b.x + b.w - a.x).abs() <= TOUCH_EPSILON && y_overlap {
        Some(Side::Left)
    } else if (a.y + a.h - b.y).abs() <= TOUCH_EPSILON && x_overlap {
        Some(Side::Below)
    } else if (b.y + b.h - a.y).abs() <= TOUCH_EPSILON && x_overlap {
        Some(Side::Above)
    } else {
        None
    }
}

fn overlap(a0: f64, alen: f64, b0: f64, blen: f64) -> bool {
    a0 + TOUCH_EPSILON < b0 + blen && b0 + TOUCH_EPSILON < a0 + alen
}

// ---- The guest's own report -------------------------------------------------------------------

/// The arrangement the guest's agent last reported, or `None` if none has.
///
/// A `Mutex` rather than the main thread's `RefCell` because the control plane runs on its own
/// thread: it publishes here, and the main thread reads it per event and per tick.
static REPORTED: std::sync::Mutex<Option<Vec<(String, GuestRect)>>> = std::sync::Mutex::new(None);

/// One connector's rectangle in the guest's logical desktop, as its compositor reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GuestRect {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
}

/// The guest told us how its monitors are arranged (`limina-agent-session`, over the control
/// plane) — the compositor's own logical rects, the space it transforms an absolute device
/// against (`meta-seat-impl.c:2462` over `meta_viewport_info_get_extents`). This is the one
/// input about the guest's layout that is a measurement rather than a guess, and every use of
/// the layout on the host reads it: the relay's metric correction ([`reported_logical_sizes`]),
/// the absolute device's mapping ([`abs_through_report`]), and the edge-pressure filter
/// ([`outer_edges`]). Positions are shifted to a zero origin: a guest may arrange its monitors
/// around any origin (a display left of the primary gets a negative x) while an absolute
/// device's range starts at zero, and only the relative placement carries meaning.
pub(crate) fn publish_guest_layout(monitors: &[limina_proto::GuestMonitor]) {
    let live: Vec<&limina_proto::GuestMonitor> = monitors
        .iter()
        .filter(|m| m.width > 0 && m.height > 0)
        .collect();
    if live.is_empty() {
        return;
    }
    let ox = live.iter().map(|m| m.x).min().unwrap_or(0);
    let oy = live.iter().map(|m| m.y).min().unwrap_or(0);
    let mut layout: Vec<(String, GuestRect)> = live
        .iter()
        .map(|m| {
            (
                m.connector.clone(),
                GuestRect {
                    x: (m.x - ox).max(0) as u32,
                    y: (m.y - oy).max(0) as u32,
                    w: m.width,
                    h: m.height,
                },
            )
        })
        .collect();
    layout.sort_by(|a, b| (a.1.x, a.1.y, &a.0).cmp(&(b.1.x, b.1.y, &b.0)));
    let mut cur = REPORTED.lock().unwrap();
    if cur.as_deref() != Some(layout.as_slice()) {
        log::info!("display: the guest reports its monitors as {layout:?}");
        *cur = Some(layout);
    }
}

/// Forget the reported arrangement — the guest it came from is gone (a reboot, a fresh worker).
pub(crate) fn forget_guest_layout() {
    *REPORTED.lock().unwrap() = None;
}

/// The arrangement the guest last reported, keyed by slot.
///
/// `Virtual-N` is scanout `N-1`: that is how the guest's virtio-gpu driver names them, and it is
/// the only tie between a monitor the compositor is arranging and a scanout we are driving.
/// Connectors outside the scanout pool are dropped rather than guessed at; a report naming
/// none we could drive is no report.
fn reported_layout() -> Option<Vec<(usize, GuestRect)>> {
    let cur = REPORTED.lock().unwrap();
    let layout = cur.as_ref()?;
    let slots: Vec<(usize, GuestRect)> = layout
        .iter()
        .filter(|(_, r)| r.w > 0 && r.h > 0)
        .filter_map(|(name, r)| {
            let slot = name
                .strip_prefix("Virtual-")?
                .parse::<usize>()
                .ok()?
                .checked_sub(1)
                .filter(|s| *s < super::present::MAX_SCANOUTS)?;
            Some((slot, *r))
        })
        .collect();
    (!slots.is_empty()).then_some(slots)
}

/// The logical size the guest's compositor gave each slot's monitor, from the last report.
///
/// This is the relay's metric correction ([`correct_metric`]): where the guest picked a scale
/// the host's prediction cannot see — a fractional scale on a fixed-mode connector — the
/// reported size is the one mutter validates suggested positions against. **Sizes only**: the
/// reported positions are the user's own arrangement and must never feed back into what we
/// suggest.
pub(crate) fn reported_logical_sizes() -> std::collections::HashMap<usize, (u32, u32)> {
    reported_layout()
        .map(|slots| slots.into_iter().map(|(s, r)| (s, (r.w, r.h))).collect())
        .unwrap_or_default()
}

/// Has the guest reported its arrangement? When it has, that report is the mapping
/// ([`abs_through_report`]) and nothing needs learning; when it has not, the absolute device's
/// per-display shares are fitted from the guest's cursor echo instead (`window/absfit.rs`).
pub(crate) fn has_report() -> bool {
    reported_layout().is_some()
}

/// A unit position within one slot's content (`0.0..=1.0`, top-left origin — the output of
/// `fit::unit_through_fit`) as a position on the guest's one absolute device.
///
/// The guest spreads that device over its **whole** desktop, so a full sweep of one window must
/// cover only that display's share of the range. With a report, the slot's reported rect places
/// the point in the desktop and the reported bounding box scales it into the range. Without one
/// the point maps straight onto the range — the single-display mapping, exact for one display.
/// With two displays and no report there is no correct mapping to be had: every window sweeps
/// the whole desktop. That is the stock tier's known floor, and the guest-echo check in
/// `input.rs` is what makes it loud rather than silently wrong. `None` for a slot the report
/// does not place (a connector mid-handshake): the caller drops the event, which is a pointer
/// that briefly does not move rather than one that jumps.
pub(crate) fn abs_through_report(slot: usize, u: f64, v: f64, abs_max: i32) -> Option<(i32, i32)> {
    let (u, v) = (u.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    let range = f64::from(abs_max);
    let Some(rects) = reported_layout() else {
        return Some(((u * range).round() as i32, (v * range).round() as i32));
    };
    let r = rects.iter().find(|(s, _)| *s == slot).map(|(_, r)| *r)?;
    let dw = rects.iter().map(|(_, r)| r.x + r.w).max()?;
    let dh = rects.iter().map(|(_, r)| r.y + r.h).max()?;
    if dw == 0 || dh == 0 {
        return None;
    }
    let dx = f64::from(r.x) + u * f64::from(r.w);
    let dy = f64::from(r.y) + v * f64::from(r.h);
    Some((
        ((dx / f64::from(dw)) * range).round() as i32,
        ((dy / f64::from(dh)) * range).round() as i32,
    ))
}

/// Which sides of a slot face the outside of the guest's desktop rather than another monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Edges {
    pub(crate) left: bool,
    pub(crate) right: bool,
    pub(crate) top: bool,
    pub(crate) bottom: bool,
}

impl Edges {
    pub(crate) const ALL: Edges = Edges {
        left: true,
        right: true,
        top: true,
        bottom: true,
    };
    pub(crate) const NONE: Edges = Edges {
        left: false,
        right: false,
        top: false,
        bottom: false,
    };

    /// Keep only the part of an edge-pressure overflow that pushes at an outer edge.
    ///
    /// `overflow` is in AppKit delta convention: negative x is leftward, **positive y is
    /// downward**. A component aimed at a seam is dropped rather than scaled — there is no
    /// barrier there to charge, and sending it moves the guest's pointer.
    pub(crate) fn keep(&self, overflow: (f64, f64)) -> (f64, f64) {
        let x = match overflow.0 {
            d if d < 0.0 && !self.left => 0.0,
            d if d > 0.0 && !self.right => 0.0,
            d => d,
        };
        let y = match overflow.1 {
            d if d < 0.0 && !self.top => 0.0,
            d if d > 0.0 && !self.bottom => 0.0,
            d => d,
        };
        (x, y)
    }
}

/// One monitor's rectangle in the absolute device's range units — the guest's desktop as the
/// one device that has to cover all of it sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RangeRect {
    pub(crate) x0: f64,
    pub(crate) y0: f64,
    pub(crate) x1: f64,
    pub(crate) y1: f64,
}

impl RangeRect {
    /// Inclusive on all four sides: monitors abut at an exact coordinate, and a point on a
    /// seam belongs to both of them.
    fn holds(&self, p: (f64, f64)) -> bool {
        (self.x0..=self.x1).contains(&p.0) && (self.y0..=self.y1).contains(&p.1)
    }

    fn clamp(&self, p: (f64, f64)) -> (f64, f64) {
        (p.0.clamp(self.x0, self.x1), p.1.clamp(self.y0, self.y1))
    }

    /// How far outside this rect a point lies, squared.
    fn distance2(&self, p: (f64, f64)) -> f64 {
        let c = self.clamp(p);
        (c.0 - p.0).powi(2) + (c.1 - p.1).powi(2)
    }

    pub(crate) fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    pub(crate) fn height(&self) -> f64 {
        self.y1 - self.y0
    }
}

/// The guest's desktop in device-range units: one rect per placed slot, in the report's order.
///
/// The device's range is spread over the desktop's **bounding box**, and a desktop is a union
/// of rectangles — so unless every monitor is the same height and flush, part of that box is
/// dead space belonging to no monitor. This is the shape that says which part.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Desktop {
    rects: Vec<(usize, RangeRect)>,
}

impl Desktop {
    pub(crate) fn new(rects: Vec<(usize, RangeRect)>) -> Self {
        Self { rects }
    }

    /// One slot's share of the range, if the report places it.
    pub(crate) fn rect_of(&self, slot: usize) -> Option<RangeRect> {
        self.rects.iter().find(|(s, _)| *s == slot).map(|(_, r)| *r)
    }

    /// Hold a candidate device position on the desktop, sliding it along the wall of the
    /// monitor it is leaving.
    ///
    /// **A position is not motion, and only motion is confined.** The guest transforms an
    /// absolute value against its viewport's *extents* — the bounding box — and puts the
    /// pointer wherever that lands, dead space included, where it is over no output at all and
    /// the cursor simply vanishes (the plane is per-scanout: no scanout, no cursor). Relative
    /// motion it does confine, to the union. So the confinement of the absolute device is ours
    /// to do, and pinning at the range's ends does not do it: those are the box's corners, not
    /// the desktop's.
    ///
    /// A candidate that lands on a monitor is taken as it is — that is how the captured pointer
    /// crosses a seam, which it must, since the guest owns which display a value lands on and
    /// the echo re-homes the capture window after it (`input::follow_guest_echo`). Only a
    /// candidate that lands nowhere is clamped, and against the rect the *previous position*
    /// occupied rather than the capture slot's: between a crossing and the echo that reports it
    /// the slot is a step behind, and clamping into it would drag the pointer back over the
    /// seam it just crossed.
    ///
    /// A previous position that is itself nowhere — the guest rearranged its monitors under a
    /// live grab, which reaches us as a new report and nothing else — snaps to the nearest
    /// monitor. One jump the hand can see beats a pointer left invisible in a hole that is no
    /// longer where it was.
    pub(crate) fn confine(&self, from: (f64, f64), cand: (f64, f64)) -> (f64, f64) {
        if self.rects.iter().any(|(_, r)| r.holds(cand)) {
            return cand;
        }
        let home = self
            .rects
            .iter()
            .find(|(_, r)| r.holds(from))
            .or_else(|| {
                self.rects.iter().min_by(|a, b| {
                    a.1.distance2(from)
                        .total_cmp(&b.1.distance2(from))
                        .then(a.0.cmp(&b.0))
                })
            })
            .map(|(_, r)| *r);
        home.map_or(cand, |r| r.clamp(cand))
    }
}

/// The guest's reported desktop in device-range units, or `None` when it has not reported —
/// the range is then the single-display mapping ([`abs_through_report`]) and all of it is
/// desktop, which is exactly true for one display and the stock tier's known floor for more.
pub(crate) fn desktop_in_range(abs_max: f64) -> Option<Desktop> {
    let rects = reported_layout()?;
    let dw = f64::from(rects.iter().map(|(_, r)| r.x + r.w).max()?);
    let dh = f64::from(rects.iter().map(|(_, r)| r.y + r.h).max()?);
    if dw <= 0.0 || dh <= 0.0 {
        return None;
    }
    Some(Desktop::new(
        rects
            .iter()
            .map(|(s, r)| {
                (
                    *s,
                    RangeRect {
                        x0: f64::from(r.x) / dw * abs_max,
                        y0: f64::from(r.y) / dh * abs_max,
                        x1: f64::from(r.x + r.w) / dw * abs_max,
                        y1: f64::from(r.y + r.h) / dh * abs_max,
                    },
                )
            })
            .collect(),
    ))
}

/// Every slot's share of the device range: the guest's own report where there is one, else
/// the lines fitted from its cursor echo. The seam rule
/// ([`super::seams::Hold`]) needs the shares whether or not an agent is running, and the stock
/// tier only ever has the fitted half.
pub(crate) fn range_shares(abs_max: f64) -> Vec<(usize, RangeRect)> {
    match desktop_in_range(abs_max) {
        Some(d) => d.rects,
        None => super::absfit::shares(abs_max),
    }
}

/// Which of this slot's edges face the outside of the guest's **desktop** at the point
/// `(u, v)` of its content, rather than a seam with a neighbouring monitor — what edge
/// *pressure* is judged against.
///
/// A drag that leaves a window's content keeps reporting past the fit edge, and the fit-clamped
/// overflow forwarded as relative motion charges the guest's own barriers (GNOME's hot corner).
/// Relative motion is not confined by our mapping — a compositor clamps it at the desktop's
/// outer boundary, not at a seam — so pressure applied at a seam walks the guest's pointer onto
/// the neighbour while the absolute device keeps snapping it back: two devices fighting over
/// one pointer (the cursor ping-pongs, a band next to the seam cannot be reached, a click
/// first teleports the pointer back to the seam; observed on the two-panel rig 2026-08-18).
///
/// **The question belongs to the point, not to the side.** A guest desktop is a union of
/// rectangles and not itself a rectangle: any vertical offset or height mismatch leaves corners
/// of the bounding box that belong to no monitor, and a single edge is then a seam over the
/// span a neighbour actually abuts and a wall over the rest — the dogfood's Dell, dropped 184
/// units, meets the built-in down two thirds of its right edge and faces dead space below that.
/// So an edge is a seam exactly where something is on the other side of it *here*, which is a
/// containment test against the neighbours. Asking instead whether the edge sits at a
/// bounding-box coordinate calls every offset monitor's leading edges seams and silently drops
/// the pressure they are owed: a wall the guest holds the pointer against that charges nothing.
///
/// Without a report every edge is an outer edge — the same single-display reading
/// [`abs_through_report`] takes, so the two stay consistent. A slot the report does not place
/// has no known edges and charges nothing.
pub(crate) fn outer_edges_at(slot: usize, u: f64, v: f64) -> Edges {
    let Some(rects) = reported_layout() else {
        return Edges::ALL;
    };
    let Some(r) = rects.iter().find(|(s, _)| *s == slot).map(|(_, r)| *r) else {
        return Edges::NONE;
    };
    let (x0, y0) = (f64::from(r.x), f64::from(r.y));
    let (w, h) = (f64::from(r.w), f64::from(r.h));
    // The point on this monitor, held just inside it: the probes step half a unit off each
    // edge, and one taken from a position on the far edge would ask about the next row over.
    let x = x0 + (u.clamp(0.0, 1.0) * w).clamp(0.0, (w - 0.5).max(0.0));
    let y = y0 + (v.clamp(0.0, 1.0) * h).clamp(0.0, (h - 0.5).max(0.0));
    let occupied = |px: f64, py: f64| {
        rects.iter().any(|(s, n)| {
            *s != slot
                && (f64::from(n.x)..f64::from(n.x) + f64::from(n.w)).contains(&px)
                && (f64::from(n.y)..f64::from(n.y) + f64::from(n.h)).contains(&py)
        })
    };
    Edges {
        left: !occupied(x0 - 0.5, y),
        right: !occupied(x0 + w + 0.5, y),
        top: !occupied(x, y0 - 0.5),
        bottom: !occupied(x, y0 + h + 0.5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(panel: u64, frame: (f64, f64, f64, f64), logical: (u32, u32)) -> Placement {
        Placement {
            panel,
            frame: PointRect {
                x: frame.0,
                y: frame.1,
                w: frame.2,
                h: frame.3,
            },
            logical,
        }
    }

    const BENQ: u64 = 0xB;
    const BUILT_IN: u64 = 0x1;
    const STUDIO: u64 = 0x5;

    #[test]
    fn a_single_panel_sits_at_the_origin() {
        let got = guest_positions(&[placement(BUILT_IN, (0.0, 0.0, 1512.0, 982.0), (1512, 982))]);
        assert_eq!(got, Some(vec![(BUILT_IN, (0, 0))]));
    }

    /// The dev rig: BenQ on the left, built-in on the right, tops aligned. The guest rects
    /// abut at the BenQ's logical width, whatever its point width is.
    #[test]
    fn side_by_side_panels_abut_at_the_left_ones_logical_width() {
        let got = guest_positions(&[
            placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
            placement(BUILT_IN, (2560.0, 100.0, 1512.0, 982.0), (1512, 982)),
        ])
        .expect("clean set");
        assert_eq!(got, vec![(BENQ, (0, 0)), (BUILT_IN, (2560, 100))]);
    }

    /// THE case unit conversion gets wrong: the guest runs the BenQ at a fractional scale,
    /// so its logical width (2048) is not its point width (2560). The neighbour must sit at
    /// 2048 — the seam the pointer-units oracle measured — not at the point seam.
    #[test]
    fn a_fractional_scale_neighbour_abuts_at_the_logical_seam() {
        let got = guest_positions(&[
            placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2048, 1152)),
            placement(BUILT_IN, (2560.0, 0.0, 1512.0, 982.0), (1512, 948)),
        ])
        .expect("clean set");
        assert_eq!(got, vec![(BENQ, (0, 0)), (BUILT_IN, (2048, 0))]);
    }

    /// A panel left of the walk's root lands at negative x and the whole desktop translates
    /// to the unsigned origin the DRM properties require.
    #[test]
    fn a_panel_left_of_the_first_normalizes_to_an_unsigned_origin() {
        let got = guest_positions(&[
            placement(BUILT_IN, (2560.0, 0.0, 1512.0, 982.0), (1512, 982)),
            placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
        ])
        .expect("clean set");
        assert_eq!(got, vec![(BUILT_IN, (2560, 0)), (BENQ, (0, 0))]);
    }

    #[test]
    fn stacked_panels_abut_at_the_upper_ones_logical_height() {
        let got = guest_positions(&[
            placement(STUDIO, (0.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
            placement(BUILT_IN, (500.0, 1440.0, 1512.0, 982.0), (1512, 982)),
        ])
        .expect("clean set");
        assert_eq!(got, vec![(STUDIO, (0, 0)), (BUILT_IN, (500, 1440))]);
    }

    #[test]
    fn a_chain_of_three_walks_through_the_middle_panel() {
        let got = guest_positions(&[
            placement(BUILT_IN, (2560.0, 0.0, 1512.0, 982.0), (1512, 982)),
            placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
            placement(STUDIO, (4072.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
        ])
        .expect("clean set");
        assert_eq!(
            got,
            vec![
                (BUILT_IN, (2560, 0)),
                (BENQ, (0, 0)),
                (STUDIO, (2560 + 1512, 0)),
            ]
        );
    }

    /// Corner-only contact is not adjacency to mutter, so it is not structure to us: the
    /// panel is unreachable and the set is refused whole.
    #[test]
    fn a_panel_with_no_shared_edge_refuses_the_whole_set() {
        assert_eq!(
            guest_positions(&[
                placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
                placement(BUILT_IN, (2560.0, 1440.0, 1512.0, 982.0), (1512, 982)),
            ]),
            None
        );
        assert_eq!(
            guest_positions(&[
                placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
                placement(BUILT_IN, (3000.0, 0.0, 1512.0, 982.0), (1512, 982)),
            ]),
            None
        );
    }

    /// Two chains can rebuild into the same region when a panel's logical size outgrows its
    /// point frame: a 2×2 grid whose top-right panel is logically twice as tall reaches down
    /// into the square the bottom-right panel is placed in through the bottom-left chain.
    /// A set that would overlap in the guest is refused rather than sent to certain
    /// rejection.
    #[test]
    fn a_rebuild_that_overlaps_refuses_the_whole_set() {
        const D: u64 = 0xD;
        let got = guest_positions(&[
            placement(BENQ, (0.0, 0.0, 1000.0, 1000.0), (1000, 1000)),
            placement(BUILT_IN, (1000.0, 0.0, 1000.0, 1000.0), (1000, 2000)),
            placement(STUDIO, (0.0, 1000.0, 1000.0, 1000.0), (1000, 1000)),
            placement(D, (1000.0, 1000.0, 1000.0, 1000.0), (1000, 1000)),
        ]);
        assert_eq!(got, None);
    }

    /// Shrinkage can also break *adjacency* without overlapping: a neighbour hung low on
    /// the cross axis keeps its point offset (1200), but the left panel's logical height
    /// shrank to 1152, so the final rects share an edge line with no perpendicular
    /// overlap. mutter would reject that set — so nothing is emitted.
    #[test]
    fn a_rebuild_that_loses_adjacency_refuses_the_whole_set() {
        let got = guest_positions(&[
            placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2048, 1152)),
            placement(BUILT_IN, (2560.0, 1200.0, 1512.0, 982.0), (1512, 982)),
        ]);
        assert_eq!(got, None);
    }

    /// The residual, end to end: the BenQ on the left feeds its width into its neighbour's
    /// position. Predicted at its point size (2560) the neighbour lands at the point seam,
    /// but the guest runs the BenQ at scale 1.25 (logical 2048), so mutter — validating
    /// against its own 2048-wide rect — sees a gap and rejects the whole set. The reported
    /// sizes correct the metric and the set abuts at the true logical seam.
    #[test]
    fn reported_sizes_correct_a_mispredicted_metric() {
        let mut slots = vec![
            (
                0u32,
                placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
            ),
            (
                1u32,
                placement(BUILT_IN, (2560.0, 0.0, 1512.0, 982.0), (1512, 982)),
            ),
        ];
        // Prediction alone: the neighbour lands at the point seam, 512 past the true one.
        let predicted: Vec<_> = slots.iter().map(|(_, p)| *p).collect();
        assert_eq!(
            guest_positions(&predicted).expect("clean set")[1].1,
            (2560, 0)
        );

        // The guest's compositor reports what it actually did: BenQ at scale 1.25.
        let reported = std::collections::HashMap::from([(0usize, (2048u32, 1152u32))]);
        correct_metric(&mut slots, &reported);
        let corrected: Vec<_> = slots.iter().map(|(_, p)| *p).collect();
        let got = guest_positions(&corrected).expect("clean set");
        assert_eq!(got, vec![(BENQ, (0, 0)), (BUILT_IN, (2048, 0))]);
    }

    /// Slots the report does not cover keep their prediction — a connector that just came up
    /// has no reported rect yet, and a guest with no agent reports nothing at all.
    #[test]
    fn unreported_slots_keep_the_predicted_metric() {
        let mut slots = vec![(
            0u32,
            placement(BENQ, (0.0, 0.0, 2560.0, 1440.0), (2560, 1440)),
        )];
        correct_metric(&mut slots, &std::collections::HashMap::new());
        assert_eq!(slots[0].1.logical, (2560, 1440));
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert_eq!(guest_positions(&[]), None);
    }

    /// Sub-point drift from float frame math still reads as touching.
    #[test]
    fn drifted_frames_within_epsilon_still_abut() {
        let got = guest_positions(&[
            placement(BENQ, (0.0, 0.0, 2559.5, 1440.0), (2560, 1440)),
            placement(BUILT_IN, (2560.5, 0.0, 1512.0, 982.0), (1512, 982)),
        ])
        .expect("clean set");
        assert_eq!(got[1].1, (2560, 0));
    }

    // ---- The guest's report -------------------------------------------------------------------

    const ABS: i32 = 32767;
    /// The middle of an odd range rounds up: `0.5 * 32767` is `16383.5`.
    const MID: i32 = 16384;

    /// The reported arrangement is process-global (the control plane publishes it from its own
    /// thread), so tests that touch it must not run alongside each other. Held for the whole
    /// of each such test, because the assertion reads the same global the next test would
    /// overwrite.
    static REPORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A helper standing in for the agent's report: `(connector, x, width)`, all in the guest's
    /// logical units, laid out on one row.
    fn report(monitors: &[(&str, i32, u32)]) {
        let monitors: Vec<limina_proto::GuestMonitor> = monitors
            .iter()
            .map(|(c, x, w)| limina_proto::GuestMonitor {
                connector: (*c).to_string(),
                x: *x,
                y: 0,
                width: *w,
                height: 1000,
            })
            .collect();
        publish_guest_layout(&monitors);
    }

    /// As [`report`], for an arrangement that is not one flush row: `(connector, x, y, w, h)`.
    fn report_rects(monitors: &[(&str, i32, i32, u32, u32)]) {
        let monitors: Vec<limina_proto::GuestMonitor> = monitors
            .iter()
            .map(|(c, x, y, w, h)| limina_proto::GuestMonitor {
                connector: (*c).to_string(),
                x: *x,
                y: *y,
                width: *w,
                height: *h,
            })
            .collect();
        publish_guest_layout(&monitors);
    }

    /// The dogfood's own arrangement 2026-08-22: the Dell dropped 184 logical units below the
    /// built-in. `Virtual-1` is slot 0.
    fn report_the_ragged_desktop() {
        report_rects(&[
            ("Virtual-1", 0, 184, 3072, 1728),
            ("Virtual-2", 3072, 0, 2048, 1328),
        ]);
    }

    /// The case that must not change: with no report the mapping is what it always was for one
    /// display — the unit position straight over the range.
    #[test]
    fn without_a_report_a_window_spans_the_whole_range() {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        forget_guest_layout();
        assert_eq!(abs_through_report(0, 0.0, 0.0, ABS), Some((0, 0)));
        assert_eq!(abs_through_report(0, 1.0, 1.0, ABS), Some((ABS, ABS)));
        assert_eq!(abs_through_report(0, 0.5, 0.5, ABS), Some((MID, MID)));
        assert_eq!(
            abs_through_report(0, 5.0, -1.0, ABS),
            Some((ABS, 0)),
            "clamped"
        );
        assert_eq!(outer_edges_at(0, 0.5, 0.5), Edges::ALL);
        assert_eq!(
            outer_edges_at(7, 0.5, 0.5),
            Edges::ALL,
            "no report: every window is alone"
        );
    }

    /// The rig's own numbers, as the guest reports them: the built-in (`Virtual-2`, slot 1) on
    /// the left at 1512 logical, the BenQ (`Virtual-1`, slot 0) on the right at 2048. The
    /// share of the range is the guest's logical share, and the guest's own order wins over
    /// slot order.
    #[test]
    fn the_share_of_the_range_is_the_guests_reported_share() {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        report(&[("Virtual-2", 0, 1512), ("Virtual-1", 1512, 2048)]);
        let seam = (1512.0 / 3560.0 * f64::from(ABS)).round() as i32;
        assert_eq!(abs_through_report(1, 0.0, 0.0, ABS), Some((0, 0)));
        assert_eq!(abs_through_report(1, 1.0, 0.0, ABS).unwrap().0, seam);
        assert_eq!(abs_through_report(0, 0.0, 0.0, ABS).unwrap().0, seam);
        assert_eq!(abs_through_report(0, 1.0, 0.0, ABS).unwrap().0, ABS);
        // Out of range clamps within the slot, never onto the neighbour.
        assert_eq!(abs_through_report(1, 5.0, 0.0, ABS).unwrap().0, seam);
        // The seam is not an edge of the desktop; everything else is.
        let left = outer_edges_at(1, 0.5, 0.5);
        assert!(left.left && !left.right && left.top && left.bottom);
        let right = outer_edges_at(0, 0.5, 0.5);
        assert!(!right.left && right.right && right.top && right.bottom);
        forget_guest_layout();
    }

    /// A guest is free to arrange its monitors around any origin; the range is not.
    #[test]
    fn a_negative_origin_is_shifted_to_zero() {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        report(&[("Virtual-1", -1512, 1512), ("Virtual-2", 0, 2048)]);
        assert_eq!(abs_through_report(0, 0.0, 0.0, ABS), Some((0, 0)));
        assert_eq!(abs_through_report(1, 1.0, 0.0, ABS).unwrap().0, ABS);
        assert_eq!(reported_logical_sizes()[&0], (1512, 1000));
        forget_guest_layout();
    }

    /// A connector with a mode but no place in the report yet — it came up between the
    /// compositor's last arrangement and this event — has no place on the desktop, so a
    /// position on it means nothing and is refused rather than invented; and nothing is
    /// charged for its edges.
    #[test]
    fn an_unplaced_slot_gets_no_position_and_no_edges() {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        report(&[("Virtual-1", 0, 1000)]);
        assert_eq!(abs_through_report(0, 1.0, 1.0, ABS), Some((ABS, ABS)));
        assert_eq!(abs_through_report(1, 0.5, 0.5, ABS), None);
        assert_eq!(outer_edges_at(1, 0.5, 0.5), Edges::NONE);
        forget_guest_layout();
    }

    /// A report naming only connectors we could never drive — outside the scanout pool, or
    /// not virtio connectors at all — is no report.
    #[test]
    fn a_report_for_unknown_connectors_is_no_report() {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        report(&[("Virtual-99", 0, 100), ("HDMI-1", 100, 100)]);
        assert_eq!(abs_through_report(0, 0.5, 0.5, ABS), Some((MID, MID)));
        assert_eq!(outer_edges_at(0, 0.5, 0.5), Edges::ALL);
        forget_guest_layout();
    }

    /// The fault, in one assertion: a guest desktop that is not a rectangle leaves corners of
    /// the bounding box belonging to no monitor, and an edge facing that dead space is a wall —
    /// there is nothing on the other side to cross to. Read off the box, the Dell's top is not
    /// at `y == 0`, so it read as a seam and every upward push was dropped: a wall the guest
    /// holds the pointer against while nothing charges its barrier.
    #[test]
    fn an_edge_facing_dead_space_is_outer_although_the_box_reaches_past_it() {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        report_the_ragged_desktop();
        assert!(
            outer_edges_at(0, 0.5, 0.0).top,
            "nothing sits above the Dell, wherever the box's top is"
        );
        assert!(
            outer_edges_at(1, 0.5, 1.0).bottom,
            "nor below the built-in, which stops 584 short of the box"
        );
        forget_guest_layout();
    }

    /// One side is not one class. The Dell's right edge meets the built-in over the built-in's
    /// height and faces dead space below it, so which it is belongs to the *point*, not the
    /// side — and a per-side answer gets one half of this edge wrong whichever way it votes.
    #[test]
    fn an_edge_is_a_seam_only_where_the_neighbour_actually_abuts_it() {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        report_the_ragged_desktop();
        // Down the Dell's 1728: the built-in ends at guest y 1328, 66% of the way down.
        assert!(!outer_edges_at(0, 1.0, 0.5).right, "the built-in is there");
        assert!(outer_edges_at(0, 1.0, 0.9).right, "and stops before here");
        // The same edge from the other side: the built-in's left is a wall above the Dell.
        assert!(
            outer_edges_at(1, 0.0, 0.05).left,
            "the Dell starts 184 lower"
        );
        assert!(!outer_edges_at(1, 0.0, 0.5).left);
        forget_guest_layout();
    }

    /// The ragged desktop in device-range units. Slot 0 is the Dell, slot 1 the built-in.
    fn ragged_in_range() -> Desktop {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        report_the_ragged_desktop();
        let d = desktop_in_range(f64::from(ABS)).expect("a reported desktop");
        forget_guest_layout();
        d
    }

    fn near(got: (f64, f64), want: (f64, f64)) {
        assert!(
            (got.0 - want.0).abs() < 0.5 && (got.1 - want.1).abs() < 0.5,
            "{got:?} is not {want:?}"
        );
    }

    /// The fault: the device's range covers the bounding box, so pinning at its ends lets a
    /// monitor that does not reach the box's edge be pushed clean off itself — into dead space,
    /// where the pointer is over no output and the cursor vanishes.
    #[test]
    fn a_step_off_a_monitor_into_dead_space_is_held_at_its_own_edge() {
        let d = ragged_in_range();
        let dell = d.rect_of(0).expect("the Dell is placed");
        assert!(dell.y0 > 0.0, "the Dell does not reach the box's top");
        // Straight up from inside the Dell, past its top: nothing is above it.
        near(
            d.confine((10000.0, dell.y0 + 50.0), (10000.0, dell.y0 - 900.0)),
            (10000.0, dell.y0),
        );
        // And down past the built-in's bottom, which stops well short of the box's.
        let built_in = d.rect_of(1).expect("the built-in is placed");
        near(
            d.confine(
                (25000.0, built_in.y1 - 50.0),
                (25000.0, built_in.y1 + 900.0),
            ),
            (25000.0, built_in.y1),
        );
    }

    /// What the confinement must not cost: the captured pointer crosses seams by walking the
    /// range, and the guest owns which display a value lands on. A step that lands on a
    /// monitor is that step, wherever it started.
    #[test]
    fn a_step_that_lands_on_the_neighbour_crosses_untouched() {
        let d = ragged_in_range();
        let seam = d.rect_of(0).expect("the Dell").x1;
        let cand = (seam + 60.0, 10000.0);
        assert_eq!(d.confine((seam - 60.0, 10000.0), cand), cand);
    }

    /// The same edge, below where the neighbour reaches: a wall, and the clamp finds it
    /// without being told which edges are which.
    #[test]
    fn the_part_of_a_seam_the_neighbour_does_not_reach_is_a_wall() {
        let d = ragged_in_range();
        let seam = d.rect_of(0).expect("the Dell").x1;
        let below = d.rect_of(1).expect("the built-in").y1 + 2000.0;
        near(
            d.confine((seam - 60.0, below), (seam + 60.0, below)),
            (seam, below),
        );
        // Diagonally into the dead corner: it slides along the wall rather than stopping.
        let d2 = d.rect_of(1).expect("the built-in").y1;
        near(
            d.confine((seam - 60.0, d2 - 100.0), (seam + 60.0, d2 + 400.0)),
            (seam, d2 + 400.0),
        );
    }

    /// A position that is itself nowhere — the guest rearranged its monitors under a live
    /// grab, which reaches us as a new report and nothing else — snaps back onto the desktop
    /// instead of being clamped deeper into the hole.
    #[test]
    fn a_previous_position_in_dead_space_snaps_to_the_nearest_monitor() {
        let d = ragged_in_range();
        let dell = d.rect_of(0).expect("the Dell");
        let built_in = d.rect_of(1).expect("the built-in");
        // The bottom-right corner of the box belongs to neither; the Dell is the closer.
        let lost = (dell.x1 + 500.0, built_in.y1 + 4000.0);
        near(d.confine(lost, (lost.0 + 10.0, lost.1)), (dell.x1, lost.1));
    }

    /// One display: the whole range is the desktop, and nothing is ever held back.
    #[test]
    fn with_one_monitor_every_position_is_on_the_desktop() {
        let _g = REPORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        report(&[("Virtual-1", 0, 1512)]);
        let d = desktop_in_range(f64::from(ABS)).expect("a reported desktop");
        forget_guest_layout();
        assert_eq!(d.confine((100.0, 100.0), (200.0, 200.0)), (200.0, 200.0));
        near(
            d.confine((100.0, 100.0), (-50.0, 99999.0)),
            (0.0, f64::from(ABS)),
        );
    }

    /// Pressure aimed at a seam is dropped; pressure aimed outward on the same event is kept.
    #[test]
    fn pressure_is_kept_outward_and_dropped_at_the_seam() {
        let e = Edges {
            left: false,
            right: true,
            top: true,
            bottom: true,
        };
        // A shove up and to the left: the leftward half is at the seam, the upward half is not.
        assert_eq!(e.keep((-9.0, -3.0)), (0.0, -3.0));
        assert_eq!(e.keep((9.0, 0.0)), (9.0, 0.0));
        assert_eq!(Edges::ALL.keep((-5.0, 5.0)), (-5.0, 5.0));
        assert_eq!(Edges::NONE.keep((-5.0, 5.0)), (0.0, 0.0));
    }
}
