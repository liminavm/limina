// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Pure letterbox geometry: aspect-fit the guest scanout into the window's content
//! view, plus the inverse mapping the absolute-pointer path needs. No AppKit types —
//! the math unit-tests headless, and the window/input code shares one `FitRect` per
//! tick (via an `Rc<Cell<_>>`) so the pixels and the pointer can never disagree.

/// How guest pixels relate to screen points.
///
/// AppKit works in *points*; a Retina display draws each point as 2x2 device pixels. limina
/// historically drove the guest 1:1 with points, so a 2x panel handed the guest half its real
/// resolution and Core Animation upscaled the scanout — visibly soft, and the guest could never
/// offer a 2x scale because its framebuffer was already the logical size.
///
/// HiDPI mode makes a guest pixel a *device* pixel: the guest is driven to points x backing,
/// renders at the panel's native resolution, and (given the EDID density we now report) picks
/// the 2x scale itself. The compositing path needs no change — the layer frame stays in points
/// and CA maps the larger surface onto it 1:1 in device pixels.
///
/// Everything that has to cross the two unit systems goes through here, so the conversion is one
/// tested thing rather than a scattering of `* backing`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Scale(f64);

impl Scale {
    /// `backing` is `NSScreen.backingScaleFactor`. A non-positive or non-finite value (never
    /// seen, but it comes from AppKit) falls back to 1, which is the historical behavior.
    pub(crate) fn new(backing: f64, hidpi: bool) -> Self {
        if !hidpi || !backing.is_finite() || backing <= 0.0 {
            return Self(1.0);
        }
        Self(backing)
    }

    /// The point-for-pixel scale — what every non-HiDPI path uses.
    pub(crate) fn none() -> Self {
        Self(1.0)
    }

    /// Screen/view points to guest pixels. Rounded, and floored at 1 so a degenerate view
    /// never asks the guest for a zero-sized mode.
    pub(crate) fn to_guest(self, points: (f64, f64)) -> (u32, u32) {
        let px = |v: f64| ((v * self.0).round().max(1.0)) as u32;
        (px(points.0), px(points.1))
    }

    /// Guest pixels back to points, for sizing the window to a guest-chosen mode.
    pub(crate) fn to_points(self, guest: (u32, u32)) -> (f64, f64) {
        (f64::from(guest.0) / self.0, f64::from(guest.1) / self.0)
    }
}

/// The fitted content rect, in view points, bottom-left origin (AppKit convention).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub(crate) struct FitRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl FitRect {
    pub(crate) fn full(vw: f64, vh: f64) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            w: vw.max(0.0),
            h: vh.max(0.0),
        }
    }
}

/// The first-appearance window content size (no remembered state): **half the
/// display's area, at the guest's aspect ratio**, clamped into the visible frame
/// (minus a title-bar allowance). Half-area means the window is never too small, never
/// screen-filling, and — because the content aspect matches the guest — never
/// letterboxed on first show. Dynamic mode derives its first-boot guest resolution
/// from this same rule (screen aspect), so window == guest there.
pub(crate) fn default_window_content(
    guest: (u32, u32),
    screen: (f64, f64),
    visible: (f64, f64),
) -> (u32, u32) {
    let (gw, gh) = (f64::from(guest.0.max(1)), f64::from(guest.1.max(1)));
    let half_area = (screen.0 * screen.1 / 2.0).max(64.0 * 64.0);
    content_for_area(half_area, gw / gh, visible)
}

/// Reshape a window's content to a new aspect while **preserving its on-screen area**,
/// then clamp into the target screen's visible frame. Used on host-mode display
/// migration: when the window moves to a differently-shaped display, the guest is driven
/// to the new screen's aspect, so the window (which AppKit left at its old shape) would
/// letterbox until the user dragged it. Reshaping here trades width for height at constant
/// area — the window neither balloons nor collapses — so the guest fills it with no bars.
pub(crate) fn reshape_to_aspect(
    current: (f64, f64),
    aspect: (u32, u32),
    visible: (f64, f64),
) -> (u32, u32) {
    let a = f64::from(aspect.0.max(1)) / f64::from(aspect.1.max(1));
    let area = (current.0 * current.1).max(64.0 * 64.0);
    content_for_area(area, a, visible)
}

/// A content size of the given `area` (pt²) at `aspect` (w/h), clamped into the visible
/// frame with the aspect preserved so the window never spills off-screen and AppKit never
/// has to constrain it.
fn content_for_area(area: f64, aspect: f64, visible: (f64, f64)) -> (u32, u32) {
    const TITLE_BAR: f64 = 28.0;
    let aspect = if aspect > 0.0 { aspect } else { 1.0 };
    let mut h = (area / aspect).sqrt();
    let mut w = h * aspect;
    let (vw, vh) = (visible.0, (visible.1 - TITLE_BAR).max(64.0));
    if vw > 0.0 && w > vw {
        h *= vw / w;
        w = vw;
    }
    if h > vh {
        w *= vh / h;
        h = vh;
    }
    (w.round().max(64.0) as u32, h.round().max(64.0) as u32)
}

/// Trim the camera-housing strip off the top of a content area.
///
/// On a notched built-in display the app gets the **whole** panel in fullscreen (the
/// `NSPrefersDisplaySafeAreaCompatibilityMode` key in the bundle — see [`NotchPolicy`]), so it
/// falls to us to decide whether the guest uses that strip. `inset` is the notch height in
/// points, already zero unless the policy is `avoid` AND the window is fullscreen on a notched
/// screen. Trimming from the *top* is all that's needed: the view's origin is bottom-left, so a
/// shorter height leaves the housing strip uncovered without moving anything.
///
/// [`NotchPolicy`]: crate::vmlib::schema::NotchPolicy
pub(crate) fn usable_content(vw: f64, vh: f64, inset: f64) -> (f64, f64) {
    if !inset.is_finite() || inset <= 0.0 {
        return (vw, vh);
    }
    (vw, (vh - inset).max(0.0))
}

/// Aspect-fit a `gw`×`gh` guest resolution into a `vw`×`vh` view, centered.
///
/// An exact match short-circuits to the full view so float rounding can never
/// introduce 1-px bars in the converged case; degenerate inputs (zero-size guest or
/// view) also fall back to the full view rather than a 0×0 rect.
pub(crate) fn aspect_fit(gw: u32, gh: u32, vw: f64, vh: f64) -> FitRect {
    if gw == 0 || gh == 0 || vw <= 0.0 || vh <= 0.0 {
        return FitRect::full(vw, vh);
    }
    let (gw, gh) = (gw as f64, gh as f64);
    if gw == vw && gh == vh {
        return FitRect::full(vw, vh);
    }
    let scale = (vw / gw).min(vh / gh);
    let (w, h) = (gw * scale, gh * scale);
    FitRect {
        x: (vw - w) / 2.0,
        y: (vh - h) / 2.0,
        w,
        h,
    }
}

/// Map a view point (bottom-left origin) to evdev absolute coordinates through the
/// fit rect: subtract the letterbox offset, normalize by the fitted size, clamp into
/// the content (drag semantics — a drag that leaves the content pins to its edge),
/// and flip Y (AppKit bottom-left → evdev top-left).
pub(crate) fn abs_through_fit(px: f64, py: f64, fit: FitRect, abs_max: i32) -> (i32, i32) {
    if fit.w <= 0.0 || fit.h <= 0.0 {
        return (0, 0);
    }
    let fx = ((px - fit.x) / fit.w).clamp(0.0, 1.0);
    let fy = (1.0 - (py - fit.y) / fit.h).clamp(0.0, 1.0);
    (
        (fx * f64::from(abs_max)).round() as i32,
        (fy * f64::from(abs_max)).round() as i32,
    )
}

/// One captured-mode virtual-cursor step: the clamped position plus the motion the clamp
/// ate ([`capture_step`]).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct CaptureStep {
    /// The new virtual cursor position (view points, bottom-left origin), clamped into the fit.
    pub pos: (f64, f64),
    /// The clamped-off overflow, in AppKit **delta** convention (positive `y` = down): the
    /// component of the motion that tried to push past a content edge. Zero away from edges.
    /// Forwarded to the guest's relative-mouse device so edge/corner *pressure* still exists
    /// in the guest — GNOME's hot corner is a mutter pressure barrier that fires on motion
    /// pushed INTO it while pinned, which a pre-clamped absolute stream can never express.
    pub overflow: (f64, f64),
}

/// Advance the captured-mode virtual cursor by one motion delta, clamped to the fitted
/// content. `pos` is the position in view points (bottom-left origin, same space as the
/// fit rect); `None` seeds at the content centre (nothing has ever placed the cursor).
/// `dx`/`dy` are AppKit mouse deltas — positive `dy` is *downward*, so it subtracts in
/// this bottom-left space. The clamp is what contains the cursor at the guest's edges,
/// exactly like the host screen edge does outside capture; it also re-pins a stale
/// position into a fit rect that shrank (fullscreen toggle mid-capture).
pub(crate) fn capture_step(pos: Option<(f64, f64)>, dx: f64, dy: f64, fit: FitRect) -> CaptureStep {
    let (sx, sy) = pos.unwrap_or((fit.x + fit.w / 2.0, fit.y + fit.h / 2.0));
    let (ux, uy) = (sx + dx, sy - dy);
    let cx = ux.clamp(fit.x, fit.x + fit.w.max(0.0));
    let cy = uy.clamp(fit.y, fit.y + fit.h.max(0.0));
    // Overflow is bounded by this event's own delta: a *stale-position* clamp (the fit
    // shrank while captured) repins silently instead of masquerading as a huge shove.
    // View-space y grows UP, deltas grow DOWN — flip y back into delta convention.
    let bound = |v: f64, d: f64| v.clamp(d.min(0.0), d.max(0.0));
    CaptureStep {
        pos: (cx, cy),
        overflow: (bound(ux - cx, dx), bound(-(uy - cy), dy)),
    }
}

/// How far inside the guest's edge the resistance parks the host cursor while it holds.
///
/// Pinning *on* the edge is not enough at the top: macOS reveals the menu bar and the window's
/// title bar the instant the cursor touches the top row of the screen, and a fullscreen guest's
/// top edge IS that row — so the chrome is already down before any amount of resistance could
/// push back. The same is true of an auto-hidden Dock on whichever edge it lives. Holding a
/// couple of points short of the edge keeps the cursor off those triggers; the guest is
/// unaffected because while we hold, its pointer is driven by the forwarded pressure (below)
/// rather than by the host cursor's position.
const KEEPOUT: f64 = 2.0;

/// How far back inside the held edge the pointer must travel before the accumulated push is
/// forgotten. Must exceed [`KEEPOUT`], or the cursor we park ourselves would read as a retreat
/// and drain the accumulator we just filled.
const RELEASE_MARGIN: f64 = 6.0;

/// Fraction of the configured threshold the *side* edges resist with.
///
/// The two escapes are not equally unwelcome. Leaving sideways is usually deliberate — it is how
/// you reach another display, the thing this feature exists to keep possible — while the top
/// edge's reward is macOS chrome dropping over the guest, which is never what you meant. So the
/// sides yield sooner than the configured threshold and the top holds the full amount.
const SIDE_FACTOR: f64 = 0.5;

/// How close to a corner counts as being *at* it, where the pointer is never released.
pub(crate) const CORNER_ZONE: f64 = 32.0;

/// The most outward push a **single event** can contribute to the accumulator.
///
/// Without this, breakthrough is pure distance and the deltas are post-ballistics, so a flick
/// arrives as one 100+ pt event and is through on first contact while a slow deliberate push
/// pays ten events for the same nominal distance — resistance strongest against the motion you
/// mean, weakest against the careless flick it exists to stop. Capping per event makes it a test
/// of *sustained* motion, which is independently where the chrome ask ended up
/// (`input::REVEAL_TICK_CAP`). At 60 Hz a 100 pt threshold is then ~10 events, ~0.17 s of
/// pushing.
///
/// Caps only what is ACCUMULATED. The overflow forwarded to the guest stays the full delta —
/// that is real motion the guest's own barriers are entitled to.
const PUSH_EVENT_CAP: f64 = 10.0;

/// Slack allowed between a reported position change and the delta that claims to explain it,
/// before [`is_reflow`] calls the event a layout artifact rather than motion.
const REFLOW_SLACK: f64 = 8.0;

/// Whether this pointer event is the *content moving under the pointer* rather than the pointer
/// moving — a re-layout reported through the same channel as a mouse move.
///
/// Real motion displaces the pointer by its own delta. A reflow does not, and the mismatch is the
/// only signal that does not require knowing which reflow happened or how big the inset is.
///
/// This exists because granting the `notch = extend` chrome ask drops the overlay, re-parenting
/// the guest view into the shorter carrier — and the resulting event claims the pointer moved. Two
/// samples measured in one boot: `y` 982.0 -> 31.9 carrying `dy = +33.0` (exactly the notch
/// inset), and `y` 947.9 -> 30.6 carrying `dy = +1.0`, a 917-point jump attributed to a one-point
/// move. Both read as "the pointer went back into the guest", which released the ask, restored the
/// geometry, and let the still-leaning pointer re-arm it — the chrome oscillated for as long as
/// the user kept pushing.
pub(crate) fn is_reflow(prev: Option<(f64, f64)>, cur: (f64, f64), delta_y: f64) -> bool {
    prev.is_some_and(|p| (cur.1 - p.1).abs() > delta_y.abs() + REFLOW_SLACK)
}

/// What the uncaptured fullscreen pointer path owes one motion event.
///
/// Two duties share that path and are otherwise unrelated: holding the pointer at the guest's
/// edges (`[display] edge-resistance`, a preference) and running the `notch = extend` chrome ask
/// (`InputState::reveal_step`, the only way back to the menu bar under the overlay). Sharing a
/// function made the second silently inherit the first's enable check, so `Edge resist: Off` left
/// an `extend` guest with no way to ask for the chrome — the third time the ask has been lost by
/// riding on a code path that exists for another reason. Deciding both here, from named inputs,
/// is what makes the independence testable.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct EdgeDuties {
    /// Feed this event to the chrome ask.
    pub ask: bool,
    /// Run it through [`EdgeResist`].
    pub resist: bool,
}

/// Decide both duties for one event. See [`EdgeDuties`].
pub(crate) fn edge_duties(fullscreen_and_key: bool, resistance_enabled: bool) -> EdgeDuties {
    EdgeDuties {
        // Not conditioned on resistance: the ask is a `notch = extend` feature, and the overlay is
        // up (or not) regardless of what the preference says.
        ask: fullscreen_and_key,
        resist: fullscreen_and_key && resistance_enabled,
    }
}

// A corner used to merely hold *longer* than the rest of the edge (a CORNER_FACTOR multiplier on
// the threshold). Right diagnosis, wrong layer: whatever multiple was chosen, breaking through
// ended the forwarded pressure, and a barrier that wants sustained push cannot be served by a
// bounded burst. Corners now simply never release — see the corner arm of `EdgeResist::step`.

/// Coordinate slop for "is the pointer against this edge". Positions arrive as f64 points that
/// have been through two affine conversions; an exact compare would miss by ulps.
const EPS: f64 = 0.001;

/// One axis of a rectangle inset by `inset` on both sides, as `(low, high)`.
///
/// Content thinner than two insets collapses to its own centre rather than producing an
/// inside-out range — `f64::clamp` panics when min > max, and a mid-resize tick can get there.
fn band(origin: f64, extent: f64, inset: f64) -> (f64, f64) {
    let extent = extent.max(0.0);
    if extent <= 2.0 * inset {
        let mid = origin + extent / 2.0;
        (mid, mid)
    } else {
        (origin + inset, origin + extent - inset)
    }
}

/// One edge-resistance step: where the host cursor belongs, the pressure to hand the guest,
/// and whether the pointer is free to leave.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct ResistStep {
    /// The host cursor position (view points, bottom-left origin) after resistance.
    pub pos: (f64, f64),
    /// Motion the resistance ate, in AppKit **delta** convention — forwarded to the guest's
    /// relative-mouse device so mutter's barriers (the GNOME hot corner, a guest panel's
    /// reveal edge) still see the shove, exactly as in captured mode.
    pub overflow: (f64, f64),
    /// True when this event needs no intervention: the pointer is inside the content, or it has
    /// pushed hard enough to break out. The caller passes the event through untouched.
    pub free: bool,
}

// This struct used to carry a `revealed` flag — "the event that broke out upward", once the
// `notch = extend` chrome ask. It stopped being the ask when the gesture became a timed charge in
// `InputState::reveal_step` (a breakthrough is a *distance*, and a two-event flick summoned the
// menu bar), and lingered afterwards as a field only the trace read: a retired mechanism still
// looking like the live one, which is exactly how the ask acquired two owners in the first place.

/// The outward component of this motion, if the pointer is against an edge of `fit`.
///
/// Guest-side pressure barriers — mutter's hot corner, a guest panel's reveal edge — charge on
/// *motion into* the barrier, not on the pointer merely being there. Our uncaptured pointer is
/// driven by the absolute tablet, which can only ever say "the cursor is at (0,0)", so unless the
/// push is forwarded separately on the relative device the guest's barriers never accumulate
/// anything and the hot corner is simply unreachable. Edge resistance forwards it while it holds,
/// which quietly made a core guest interaction depend on the Accessibility grant the tap needs;
/// this is the same measurement for the path that has no tap.
///
/// Returns AppKit deltas (positive `dy` is downward), zero on any axis not pushing outward at an
/// edge — so it is silent everywhere except exactly where a barrier could be.
pub(crate) fn edge_overflow(cur: (f64, f64), dx: f64, dy: f64, fit: FitRect) -> (f64, f64) {
    if fit.w <= 0.0 || fit.h <= 0.0 {
        return (0.0, 0.0);
    }
    // `out_high` is the sign of the delta that points away from the content at that axis's high
    // edge; the axes differ only there, because view y grows up while deltas grow down.
    let out = |p: f64, lo: f64, hi: f64, d: f64, out_high: f64| {
        let against = if d * out_high > 0.0 {
            p >= hi - EPS
        } else {
            p <= lo + EPS
        };
        if d != 0.0 && against {
            d
        } else {
            0.0
        }
    };
    (
        out(cur.0, fit.x, fit.x + fit.w, dx, 1.0),
        out(cur.1, fit.y, fit.y + fit.h, dy, -1.0),
    )
}

/// Fullscreen edge resistance: the pointer sticks at the guest's edge until *pushed* through.
///
/// In fullscreen the host cursor reaching the top edge instantly reveals the macOS menu bar and
/// title bar, and reaching a side edge leaves for the next display. Both are one careless flick
/// away, which makes a fullscreen guest feel leaky. Resistance makes leaving *deliberate*: motion
/// past an edge is absorbed (and forwarded to the guest as edge pressure, so the guest's own top
/// bar and hot corner keep working) until the accumulated outward push crosses `threshold`
/// points, at which point the pointer breaks through and behaves normally until it comes back
/// inside. The threshold is the *top* edge's; the sides take [`SIDE_FACTOR`] of it, and while
/// resistance holds, the cursor is parked [`KEEPOUT`] points short of the edge so macOS's own
/// reveals never fire in the first place.
///
/// This is the uncaptured counterpart to pointer capture (Cmd-Ctrl-G), which prevents the same
/// escapes absolutely by parking the host cursor at screen centre.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EdgeResist {
    /// Points of accumulated outward push needed to break out at the **top or bottom**; the
    /// sides need [`SIDE_FACTOR`] of it. Zero (or negative) disables resistance entirely —
    /// every step comes back `free`.
    threshold: f64,
    /// Outward push accumulated at the current edge, per axis, as magnitudes.
    acc: (f64, f64),
    /// Broken through: stay out of the way until the pointer re-enters the content.
    through: bool,
    /// Whether to hold the cursor short of the edge (see [`KEEPOUT`]). Off under the `extend`
    /// overlay, where nothing can reveal over the guest and the keep-out would only stop the
    /// pointer reaching the guest's own top edge — the top bar and the top-left hot corner want
    /// it at the true corner.
    keepout: bool,
}

impl EdgeResist {
    /// The model's own state, for the trace. Logging what the model *decided* alongside what it
    /// was *told* is what keeps an analyzer from re-deriving it — the trap that made the first
    /// `analyze-trace.py` disagree with the app (`spikes/edge-pressure/RESULTS.md`).
    pub(crate) fn state(&self) -> (f64, f64, bool, f64) {
        (self.acc.0, self.acc.1, self.through, self.threshold)
    }

    pub(crate) fn new(threshold: f64) -> Self {
        Self {
            threshold: if threshold.is_finite() {
                threshold
            } else {
                0.0
            },
            acc: (0.0, 0.0),
            through: false,
            keepout: true,
        }
    }

    /// Turn the keep-out band on or off. Set per event by the caller from whether the guest is
    /// hosted in the `extend` overlay.
    pub(crate) fn set_keepout(&mut self, on: bool) {
        self.keepout = on;
    }

    pub(crate) fn enabled(&self) -> bool {
        self.threshold > 0.0
    }

    /// Forget any accumulated push and any breakthrough. Called when resistance stops applying
    /// (leaving fullscreen, losing key, entering capture) so a stale half-push can't let the
    /// next single twitch out.
    pub(crate) fn reset(&mut self) {
        self.acc = (0.0, 0.0);
        self.through = false;
    }

    /// Advance by one motion event.
    ///
    /// `cur` is where the window server has **already put** the cursor (view points, bottom-left
    /// origin) — the event's own location, after this motion was applied. It is deliberately not
    /// a position we integrate ourselves: the caller consumes the events it resists, so any
    /// position maintained downstream of the tap stops updating exactly while resistance is
    /// engaged, and goes stale the moment the pointer leaves the window for another display.
    /// Steering by a stale origin is what used to yank a pointer that had legitimately escaped
    /// back into the guest.
    ///
    /// `dx`/`dy` are the event's deltas in AppKit convention (positive `dy` is *downward*). They
    /// are what the pressure is measured in, and they must be: at a real screen edge the cursor
    /// stops moving but the deltas keep coming, so position alone cannot tell a shove from a
    /// rest.
    /// `escaped` is "the pointer is no longer on our window at all" — on another display, in
    /// practice, since resistance only runs fullscreen. It must be decided by the caller against
    /// the *view*, not this `fit`: a letterboxed guest has bars that are still our own window and
    /// still worth resisting past.
    pub(crate) fn step(
        &mut self,
        cur: (f64, f64),
        dx: f64,
        dy: f64,
        fit: FitRect,
        escaped: bool,
    ) -> ResistStep {
        let free = |pos| ResistStep {
            pos,
            overflow: (0.0, 0.0),
            free: true,
        };
        if !self.enabled() {
            return free(cur);
        }
        if escaped {
            // Already gone. Every position out there reads as "hard against the edge" (`against`
            // is `p >= hi - EPS`, and the pointer may be hundreds of points past it), so without
            // this the next outward twitch is absorbed and the cursor warped back off the display
            // the user just moved to. Reported as "the mouse jumps around while I change focus",
            // because only outward motion is caught and inward motion is not.
            self.through = true;
            self.acc = (0.0, 0.0);
            return free(cur);
        }
        if self.through {
            // Already out. Re-arm only once the pointer is genuinely back inside the content —
            // and *inside* means clear of the boundary by the same retreat margin a push is
            // forgiven at. Re-arming at the boundary would fight the user on the way out, and at
            // the top it is worse than that: once through, the cursor rests against the screen
            // edge and reports that same position forever, so a boundary-inclusive test re-arms
            // instantly and the next twitch drags it back off the menu bar it just earned.
            let (rx_lo, rx_hi) = band(fit.x, fit.w, RELEASE_MARGIN);
            let (ry_lo, ry_hi) = band(fit.y, fit.h, RELEASE_MARGIN);
            let back_inside = fit.w > 0.0
                && fit.h > 0.0
                && (rx_lo..=rx_hi).contains(&cur.0)
                && (ry_lo..=ry_hi).contains(&cur.1);
            if back_inside {
                self.reset();
            } else {
                return free(cur);
            }
        }

        // The area the held cursor is allowed to occupy: the content, minus the keep-out band
        // that stops it triggering macOS's own edge reveals.
        let inset = if self.keepout { KEEPOUT } else { 0.0 };
        let (lo_x, hi_x) = band(fit.x, fit.w, inset);
        let (lo_y, hi_y) = band(fit.y, fit.h, inset);

        // Is this event pushing *outward* at an edge the pointer is already against? Both halves
        // matter: against-the-edge alone is just resting there, and outward motion alone is the
        // ordinary business of crossing the content.
        // `out_high` says which sign of the delta points *away* from the content at that axis's
        // high edge — the axes differ only there, because view y grows up while deltas grow down,
        // so leaving over the TOP is a negative dy.
        let push = |p: f64, lo: f64, hi: f64, d: f64, out_high: f64| {
            let against = if d * out_high > 0.0 {
                p >= hi - EPS
            } else {
                p <= lo + EPS
            };
            if d != 0.0 && against {
                d
            } else {
                0.0
            }
        };
        let push_x = push(cur.0, lo_x, hi_x, dx, 1.0);
        let push_y = push(cur.1, lo_y, hi_y, dy, -1.0);

        // A genuine retreat forgets the push — the accumulator is about a *sustained* shove, and
        // without this a hundred unrelated nudges over a session would eventually add up to a
        // breakthrough. Measured as distance back inside rather than as inward motion, because
        // the keep-out warp is itself an inward move and must not drain what it is holding.
        let inside_x = (cur.0 - lo_x).min(hi_x - cur.0);
        let inside_y = (cur.1 - lo_y).min(hi_y - cur.1);
        if inside_x > RELEASE_MARGIN {
            self.acc.0 = 0.0;
        }
        if inside_y > RELEASE_MARGIN {
            self.acc.1 = 0.0;
        }
        if (push_x, push_y) == (0.0, 0.0) {
            // Not pushing at an edge: nothing to hold, and nothing to charge for. Passing the
            // event through untouched is what keeps the letterbox bars and the content itself
            // freely traversable.
            return free(cur);
        }

        // In a corner both edges hold harder, so the guest's own corner trigger gets its pressure
        // before we let go. `inside_*` are the distances to the nearest edge on each axis, already
        // computed above — near a corner both are small.
        if inside_x <= CORNER_ZONE && inside_y <= CORNER_ZONE {
            // A corner never releases the pointer. Breaking out there ends the forwarded pressure
            // mid-gesture — the guest got one burst and then silence however long the user kept
            // pushing, so its hot corner charged to just under its trigger and stopped. Holding
            // lets the push continue indefinitely, which is what a pressure barrier expects.
            // Nothing is trapped: sliding a few points along either edge leaves the zone and the
            // ordinary thresholds apply again.
            //
            // Deliberately does NOT accumulate. A corner never releases, so charge banked here
            // can only ever be spent on a *different* edge later: a second of hot-corner dwell
            // used to bank 600 pt, and sliding out along the top drained only the x accumulator,
            // so the next 1 pt nudge broke through. A breakthrough disconnected from the motion
            // that caused it is unattributable by construction.
            return ResistStep {
                pos: (cur.0.clamp(lo_x, hi_x), cur.1.clamp(lo_y, hi_y)),
                overflow: (push_x, push_y),
                free: false,
            };
        }
        // Cap what one event may contribute (see `PUSH_EVENT_CAP`), and cap the accumulator at
        // what its own axis actually requires so dwelling can never bank spendable charge.
        let side = self.threshold * SIDE_FACTOR;
        self.acc.0 = (self.acc.0 + push_x.abs().min(PUSH_EVENT_CAP)).min(side);
        self.acc.1 = (self.acc.1 + push_y.abs().min(PUSH_EVENT_CAP)).min(self.threshold);
        if self.acc.0 >= side || self.acc.1 >= self.threshold {
            self.through = true;
            self.acc = (0.0, 0.0);
            return free(cur);
        }
        ResistStep {
            pos: (cur.0.clamp(lo_x, hi_x), cur.1.clamp(lo_y, hi_y)),
            overflow: (push_x, push_y),
            free: false,
        }
    }
}

/// Is a view point inside the fitted content (as opposed to the letterbox bars)?
pub(crate) fn point_in_fit(px: f64, py: f64, fit: FitRect) -> bool {
    fit.w > 0.0
        && fit.h > 0.0
        && px >= fit.x
        && px <= fit.x + fit.w
        && py >= fit.y
        && py <= fit.y + fit.h
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABS_MAX: i32 = 32767;

    /// The measured geometry of a 14" MacBook Pro built-in display (`spikes/notch-fullscreen/`):
    /// the fullscreen content view is the full panel under the compatibility key, and the
    /// camera housing eats 33 pt of it.
    const PANEL: (f64, f64) = (1512.0, 982.0);
    const NOTCH: f64 = 33.0;

    /// Both samples are verbatim from one `LIMINA_EDGE_TRACE=1` boot, at the moment granting the
    /// chrome ask dropped the overlay. Neither is motion; the pointer never moved. Before this
    /// check they both read as "back inside the guest" and released the ask that had just been
    /// granted, so the chrome oscillated for as long as the user leaned on the edge.
    #[test]
    fn the_overlay_dropping_under_a_still_pointer_is_not_pointer_motion() {
        // The reflow arrives wearing a plausible delta — exactly the notch inset, downward.
        assert!(is_reflow(Some((1464.6, 982.0)), (1462.4, 31.9), 33.0));
        // And sometimes an implausible one: a 917-point jump claiming to be a one-point move.
        assert!(is_reflow(Some((1455.0, 947.9)), (1454.0, 30.6), 1.0));
    }

    #[test]
    fn real_motion_moves_the_pointer_by_its_own_delta_and_is_never_a_reflow() {
        // A hard push at the edge: the position tracks the delta.
        assert!(!is_reflow(Some((443.8, 982.0)), (443.8, 940.0), -42.0));
        // Sub-pixel drift and rounding stay inside the slack.
        assert!(!is_reflow(Some((443.8, 982.0)), (443.8, 982.0), -1.0));
        assert!(!is_reflow(Some((100.0, 500.0)), (100.0, 493.0), 0.0));
        // The very first event has nothing to compare against and must not be discarded.
        assert!(!is_reflow(None, (1462.4, 31.9), 33.0));
    }

    #[test]
    fn notch_avoid_trims_the_housing_strip_off_the_top() {
        assert_eq!(
            usable_content(PANEL.0, PANEL.1, NOTCH),
            (1512.0, 949.0),
            "avoid must hand back exactly the below-the-notch area AppKit used to give us"
        );
    }

    #[test]
    fn notch_extend_and_notchless_screens_keep_the_whole_panel() {
        assert_eq!(usable_content(PANEL.0, PANEL.1, 0.0), PANEL);
        // A negative or NaN inset is a bad read from AppKit, not a reason to shrink the guest.
        assert_eq!(usable_content(PANEL.0, PANEL.1, -5.0), PANEL);
        assert_eq!(usable_content(PANEL.0, PANEL.1, f64::NAN), PANEL);
    }

    #[test]
    fn a_notch_taller_than_the_view_clamps_to_zero_not_negative() {
        // Degenerate (a tiny window on a notched screen); must not produce a negative height
        // that would flip the fit rect inside out.
        assert_eq!(usable_content(400.0, 20.0, NOTCH), (400.0, 0.0));
    }

    #[test]
    fn avoiding_the_notch_fits_the_guest_exactly_with_no_side_bars() {
        // The bug this closes: host mode drove the guest to the full panel (982 pt) while the
        // fullscreen content view was only 949 pt tall, so aspect_fit letterboxed on ALL sides.
        // Sizing the guest to the usable area makes fullscreen an exact fit again.
        let (uw, uh) = usable_content(PANEL.0, PANEL.1, NOTCH);
        let fit = aspect_fit(uw as u32, uh as u32, uw, uh);
        assert_eq!(fit, FitRect::full(uw, uh));
        assert_eq!(
            (fit.x, fit.y),
            (0.0, 0.0),
            "the strip is trimmed from the TOP"
        );
    }

    /// Push at `cur` until resistance lets go, returning the number of events it took.
    /// Breakthrough is a *sustained*-motion test now (see `PUSH_EVENT_CAP`), so tests that need
    /// to get out cannot do it with one big delta.
    fn push_until_free(r: &mut EdgeResist, cur: (f64, f64), d: (f64, f64), fit: FitRect) -> u32 {
        for n in 1..=200 {
            if r.step(cur, d.0, d.1, fit, false).free {
                return n;
            }
        }
        panic!("never broke through");
    }

    /// A fullscreen-sized content area for the resistance tests.
    fn screen_fit() -> FitRect {
        FitRect::full(1512.0, 949.0)
    }

    /// The top edge of [`screen_fit`], where the window server pins the cursor once it has
    /// reached the top of the screen: every further upward event reports this same location and
    /// a fresh negative delta.
    const TOP: f64 = 949.0;

    #[test]
    fn resistance_holds_the_cursor_clear_of_the_top_edge_and_reports_pressure() {
        let mut r = EdgeResist::new(100.0);
        // Against the top of the screen, still shoving up (AppKit deltas: up is NEGATIVE dy).
        let step = r.step((700.0, TOP), 0.0, -10.0, screen_fit(), false);
        assert!(!step.free, "a 10 pt nudge must not reveal the menu bar");
        assert_eq!(
            step.pos,
            (700.0, TOP - KEEPOUT),
            "held SHORT of the edge — pinning it ON the edge is what dropped the macOS chrome, \
             which fires on contact and cannot be pushed back up"
        );
        assert_eq!(
            step.overflow,
            (0.0, -10.0),
            "the eaten motion goes to the guest as edge pressure (hot corner / top bar)"
        );
    }

    #[test]
    fn a_sustained_shove_breaks_through() {
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        for _ in 0..9 {
            assert!(!r.step((700.0, TOP), 0.0, -10.0, fit, false).free);
        }
        // 10 x 10 pt reaches the threshold: the pointer is released and the chrome may appear.
        let out = r.step((700.0, TOP), 0.0, -10.0, fit, false);
        assert!(out.free, "100 pt of push must break out");
        // Still out on the next event, without re-earning it.
        assert!(r.step((700.0, TOP), 0.0, -10.0, fit, false).free);
    }

    #[test]
    fn a_pointer_that_broke_through_the_top_can_stay_on_the_menu_bar() {
        // Having earned the chrome, you get to use it. Once through, the cursor rests against
        // the top of the screen — which is the content's own boundary — so a re-arm test that
        // merely asks "is it inside the content" fires immediately and hauls the pointer back
        // off the menu bar it just paid for.
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        push_until_free(&mut r, (700.0, TOP), (0.0, -20.0), fit);
        for x in [700.0, 640.0, 900.0, 300.0] {
            let step = r.step((x, TOP), 12.0, -1.0, fit, false);
            assert!(step.free, "still free to track along the top at x={x}");
            assert_eq!(step.pos, (x, TOP), "and never yanked downward");
        }
        // Coming properly back down into the guest re-arms it.
        assert!(r.step((700.0, TOP - 200.0), 0.0, 200.0, fit, false).free);
        assert!(!r.step((700.0, TOP), 0.0, -10.0, fit, false).free);
    }

    #[test]
    fn the_sides_yield_sooner_than_the_top() {
        // Deliberate asymmetry: crossing to another display is a thing you mean to do, while
        // the top edge's reward is macOS chrome over the guest.
        let fit = screen_fit();
        let mut side = EdgeResist::new(100.0);
        for _ in 0..4 {
            assert!(!side.step((1512.0, 400.0), 10.0, 0.0, fit, false).free);
        }
        assert!(
            side.step((1512.0, 400.0), 10.0, 0.0, fit, false).free,
            "half the threshold is enough sideways"
        );
        let mut top = EdgeResist::new(100.0);
        for _ in 0..5 {
            assert!(
                !top.step((700.0, TOP), 0.0, -10.0, fit, false).free,
                "the same 50 pt upward is still held"
            );
        }
    }

    #[test]
    fn pushing_into_an_edge_is_forwarded_as_pressure_but_resting_there_is_not() {
        // The bug this guards: the guest's pressure barriers charge on motion *into* them, and
        // the absolute tablet can only ever report a position. Without this the hot corner was
        // unreachable whenever the capture tap was missing — i.e. whenever Accessibility was
        // ungranted, which is any freshly built binary.
        let fit = screen_fit();
        // Against the top-left corner, pushing further into it: both axes count.
        assert_eq!(edge_overflow((0.0, TOP), -8.0, -6.0, fit), (-8.0, -6.0));
        // Resting against it with no motion charges nothing.
        assert_eq!(edge_overflow((0.0, TOP), 0.0, 0.0, fit), (0.0, 0.0));
        // At the corner but moving *away* on one axis: only the axis still pushing counts.
        assert_eq!(edge_overflow((0.0, TOP), 8.0, -6.0, fit), (0.0, -6.0));
        // Mid-content motion is ordinary pointer movement, never pressure.
        assert_eq!(edge_overflow((700.0, 400.0), -8.0, -6.0, fit), (0.0, 0.0));
        // A degenerate fit (mid-resize) must not report phantom pressure.
        assert_eq!(
            edge_overflow((0.0, 0.0), -8.0, -6.0, FitRect::full(0.0, 0.0)),
            (0.0, 0.0)
        );
    }

    #[test]
    fn a_corner_never_releases_so_the_guest_can_keep_charging() {
        // The bug: mutter's pressure barriers — the GNOME hot corner among them — want 100 px of
        // push, and we forward exactly what we absorb. Releasing the pointer mid-gesture ended
        // the pressure, so the corner charged part-way and stopped: "I managed it once, can't
        // figure out how to do it again". A corner is a target, not an exit.
        let fit = screen_fit();
        let mut r = EdgeResist::new(100.0);
        let corner = (0.0, TOP);
        let mut delivered = 0.0;
        for i in 0..60 {
            let step = r.step(corner, -10.0, 0.0, fit, false);
            assert!(!step.free, "still holding after {i} events");
            delivered += step.overflow.0.abs();
        }
        assert!(delivered >= 500.0, "pressure never stops (got {delivered})");
        // It is only the corner: the same push mid-edge yields at the usual half-threshold.
        let mut mid = EdgeResist::new(100.0);
        for _ in 0..4 {
            assert!(!mid.step((0.0, 400.0), -10.0, 0.0, fit, false).free);
        }
        assert!(
            mid.step((0.0, 400.0), -10.0, 0.0, fit, false).free,
            "mid-edge still yields"
        );
    }

    #[test]
    fn sliding_along_an_edge_does_not_accumulate_a_breakthrough() {
        // The failure this guards: a purely horizontal drag along the top edge should never
        // trip the menu bar, no matter how long it goes on.
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        for x in 0..50 {
            let step = r.step((100.0 + f64::from(x) * 12.0, TOP), 12.0, 0.0, fit, false);
            assert!(
                step.free,
                "moving along the content is never resisted (no outward component)"
            );
        }
        // ...and the accumulator is clean, so a fresh nudge is still resisted.
        assert!(!r.step((700.0, TOP), 0.0, -10.0, fit, false).free);
    }

    #[test]
    fn pushes_separated_by_a_retreat_do_not_add_up() {
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        for _ in 0..9 {
            assert!(!r.step((700.0, TOP), 0.0, -10.0, fit, false).free);
        }
        // Back into the content, well clear of the edge: that drains the accumulator.
        assert!(r.step((700.0, TOP - 40.0), 0.0, 40.0, fit, false).free);
        // So the next nudge starts from zero rather than tipping over the edge.
        assert!(!r.step((700.0, TOP), 0.0, -10.0, fit, false).free);
    }

    #[test]
    fn the_keepout_warp_does_not_drain_the_push_it_is_holding() {
        // The cursor we park sits KEEPOUT inside the edge, and the warp itself surfaces as an
        // inward-looking event. If that counted as a retreat, resistance could never accumulate
        // and the top edge would be free to cross on the first flick.
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        for _ in 0..9 {
            assert!(!r.step((700.0, TOP), 0.0, -10.0, fit, false).free);
            // The warp's own event: lands at the parked position, moving inward.
            assert!(
                r.step((700.0, TOP - KEEPOUT), 0.0, KEEPOUT, fit, false)
                    .free
            );
        }
        assert!(
            r.step((700.0, TOP), 0.0, -10.0, fit, false).free,
            "the tenth push still breaks through — the warps did not reset the count"
        );
    }

    #[test]
    fn breaking_out_re_arms_only_after_returning_inside() {
        let mut r = EdgeResist::new(20.0);
        let fit = screen_fit();
        // Two capped events, not one 25 pt shove: no single event can buy a breakthrough now
        // (`PUSH_EVENT_CAP`).
        push_until_free(&mut r, (700.0, TOP), (0.0, -25.0), fit);
        // Well outside, still free.
        assert!(r.step((700.0, 980.0), 0.0, -5.0, fit, false).free);
        // Back inside re-arms: the very next outward nudge is resisted again.
        assert!(r.step((700.0, 900.0), 0.0, 0.0, fit, false).free);
        assert!(!r.step((700.0, TOP), 0.0, -5.0, fit, false).free);
    }

    #[test]
    fn a_pointer_that_escaped_sideways_is_never_pulled_back() {
        // The dogfood bug (2026-08-01): after breaking out onto the next display, the pointer
        // would sometimes get thrown back into the guest. The old step integrated deltas from a
        // position the window kept — and the window stops seeing motion the moment the pointer
        // leaves it, so that position froze on the edge. Small moves *on the other display* then
        // computed as re-entering the content, re-armed the resistance, and warped the cursor
        // home. Judging by the event's real location cannot make that mistake.
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        push_until_free(&mut r, (1512.0, 400.0), (20.0, 0.0), fit);
        // Wandering around the neighbouring display, including back towards the guest.
        for x in [1600.0, 1900.0, 2400.0, 1700.0, 1560.0] {
            let step = r.step((x, 400.0), -20.0, 5.0, fit, false);
            assert!(step.free, "at x={x} the pointer is still off the guest");
            assert_eq!(step.pos, (x, 400.0), "and is left exactly where it is");
        }
    }

    #[test]
    fn resistance_also_holds_the_side_edges_for_multi_display_escapes() {
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        let step = r.step((1512.0, 400.0), 15.0, 0.0, fit, false);
        assert!(
            !step.free,
            "drifting right must not spill onto the next display"
        );
        assert_eq!(step.pos, (1512.0 - KEEPOUT, 400.0));
        assert_eq!(step.overflow, (15.0, 0.0));
    }

    #[test]
    fn a_zero_threshold_disables_resistance_entirely() {
        let mut r = EdgeResist::new(0.0);
        assert!(!r.enabled());
        let step = r.step((700.0, TOP), 0.0, -10.0, screen_fit(), false);
        assert!(step.free);
        assert_eq!(step.pos, (700.0, TOP), "the pointer is left untouched");
    }

    #[test]
    fn reset_drops_a_half_earned_breakthrough() {
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        for _ in 0..9 {
            r.step((700.0, TOP), 0.0, -10.0, fit, false);
        }
        r.reset(); // e.g. left fullscreen, or the window lost key
        assert!(!r.step((700.0, TOP), 0.0, -10.0, fit, false).free);
    }

    #[test]
    fn without_the_keepout_the_cursor_reaches_the_true_top_edge() {
        // Under the `extend` overlay nothing can reveal over the guest, so the keep-out band has
        // no job — and holding the cursor short of the edge would stop the guest's own top bar
        // and top-left hot corner getting the pointer where they expect it.
        let mut r = EdgeResist::new(100.0);
        r.set_keepout(false);
        let step = r.step((0.0, TOP), -10.0, -10.0, screen_fit(), false);
        assert!(!step.free, "still resisted — it is still an outward shove");
        assert_eq!(
            step.pos,
            (0.0, TOP),
            "but held AT the corner, not two points inside it"
        );
    }

    #[test]
    fn turning_edge_resistance_off_does_not_take_the_chrome_ask_with_it() {
        // `Edge resist: Off` + `notch = extend` + Accessibility granted used to leave no way to
        // reach the menu bar at all: the tap's resistance path returned early on the disabled
        // threshold, above its `reveal_step` call, and the local monitor stands down whenever the
        // tap is installed. The gesture worked only for users who had *not* granted Accessibility.
        assert_eq!(
            edge_duties(true, false),
            EdgeDuties {
                ask: true,
                resist: false
            },
        );
        assert_eq!(
            edge_duties(true, true),
            EdgeDuties {
                ask: true,
                resist: true
            },
        );
    }

    #[test]
    fn neither_duty_applies_to_a_window_that_is_not_fullscreen_and_key() {
        // Windowed, the pointer must be free to leave, and there is no overlay to ask out of.
        for enabled in [true, false] {
            assert_eq!(
                edge_duties(false, enabled),
                EdgeDuties {
                    ask: false,
                    resist: false
                },
            );
        }
    }

    // ---------------------------------------------------------------------------------------
    // Characterization of the CURRENT resistance model, 2026-08-02. Every assertion below
    // records behavior we intend to change: when the model is fixed these flip, and flipping
    // them is the point — do not "repair" one by relaxing it. Findings written up in
    // `spikes/edge-pressure/RESULTS.md` round 3. Produced headlessly in about a minute, which
    // is why no synthetic-event harness was needed: `step` is pure.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn a_flick_and_a_deliberate_push_now_cost_the_same_number_of_events() {
        // Was the reverse: breakthrough was pure distance, and a flick arrives as ONE
        // post-ballistics delta of 100+ pt while a careful push arrives as ten small ones — so
        // resistance was strongest against the motion you mean and absent for the careless one
        // it exists to stop. Capping the per-event contribution makes it a test of sustained
        // motion instead, which is where the chrome ask independently landed.
        let fit = screen_fit();
        let top = fit.y + fit.h;

        let mut flick = EdgeResist::new(100.0);
        flick.set_keepout(false);
        assert!(
            !flick.step((700.0, top), 0.0, -100.0, fit, false).free,
            "one huge delta no longer buys the whole threshold"
        );
        let flick_events = 1 + push_until_free(&mut flick, (700.0, top), (0.0, -100.0), fit);

        let mut push = EdgeResist::new(100.0);
        push.set_keepout(false);
        let push_events = push_until_free(&mut push, (700.0, top), (0.0, -10.0), fit);

        assert_eq!(
            flick_events, push_events,
            "a 100 pt/event flick and a 10 pt/event push both take the same sustained effort"
        );
    }

    #[test]
    fn dwelling_in_a_corner_banks_nothing_for_the_adjacent_edge() {
        // It used to: the corner arm accumulated but never released, and sliding out along the
        // top drained only the x accumulator (`inside_y` never clears the release margin), so a
        // second of hot-corner dwell banked 600 pt and the next 1 pt nudge cashed it. A
        // breakthrough disconnected from the motion that caused it is unattributable by
        // construction — which is what "weird, can't say why" sounds like.
        let fit = screen_fit();
        let top = fit.y + fit.h;
        let mut r = EdgeResist::new(100.0);
        r.set_keepout(false);
        for _ in 0..60 {
            r.step((0.0, top), -10.0, -10.0, fit, false);
        }
        assert_eq!(
            r.acc,
            (0.0, 0.0),
            "a corner never releases, so it banks nothing"
        );
        for x in [40.0, 80.0, 120.0, 200.0] {
            r.step((x, top), 20.0, 0.0, fit, false);
        }
        assert!(
            !r.step((200.0, top), 0.0, -1.0, fit, false).free,
            "and a 1 pt nudge afterwards buys 1 pt, not a breakthrough"
        );
    }

    #[test]
    fn today_hugging_any_edge_keeps_resistance_disarmed_everywhere() {
        // Re-arm demands clearance from all four edges at once, so ordinary guest work along a
        // side panel leaves every edge unresisted for as long as it lasts — silently.
        let fit = screen_fit();
        let top = fit.y + fit.h;
        let mut r = EdgeResist::new(100.0);
        r.set_keepout(false);
        for _ in 0..20 {
            r.step((700.0, top), 0.0, -30.0, fit, false);
        }
        assert!(r.through, "broke out of the top");
        r.step((2.0, top - 400.0), 0.0, 0.0, fit, false);
        assert!(
            r.through,
            "still disarmed: x=2 is inside the left edge's release margin"
        );
    }

    #[test]
    fn a_fresh_boundary_crossing_is_still_resisted() {
        // The regression the first cut of the escape guard caused (dev-mac, 2026-08-02): crossing
        // to another display never lands ON the edge — the window server moves the cursor 50-70 pt
        // in one step — so "outside now" is true for the very first event of an ordinary exit.
        // Reading that as escaped deleted side resistance entirely and made the guest's own
        // right-edge UI unreachable. The caller now asks whether the motion could have put the
        // pointer there; a 60 pt event landing 40 pt out could.
        let fit = screen_fit();
        let mut r = EdgeResist::new(100.0);
        r.set_keepout(false);
        let step = r.step((fit.x + fit.w + 40.0, 400.0), 60.0, 0.0, fit, false);
        assert!(
            !step.free,
            "one step past the edge is a crossing, not an escape"
        );
        assert_eq!(
            step.pos.0,
            fit.x + fit.w,
            "held at the edge so guest edge UI is reachable"
        );
    }

    #[test]
    fn a_pointer_on_another_display_is_left_alone() {
        // The reported symptom (2026-08-02): "the mouse jumps around while I test focus
        // changes". `resist_edges` resets on every focus loss, which clears `through`; when the
        // window becomes key again with the pointer parked on the other display, that pointer is
        // 800 pt past an edge and `against` is trivially true, so outward motion is absorbed and
        // the cursor warped home. Leftward motion is unaffected, which is why it reads as
        // erratic jumping rather than a wall.
        let fit = screen_fit();
        let outside = fit.x + fit.w + 800.0;
        let mut r = EdgeResist::new(100.0);
        r.set_keepout(false);
        let step = r.step((outside, 400.0), 20.0, 0.0, fit, false);
        assert!(!step.free, "resisted, 800 pt outside the content");
        assert_eq!(
            step.pos.0,
            fit.x + fit.w,
            "and yanked back to the guest's edge"
        );
    }

    #[test]
    fn a_content_area_thinner_than_the_keepout_does_not_panic() {
        // Degenerate but reachable mid-resize: the keep-out band would invert the clamp range.
        let mut r = EdgeResist::new(100.0);
        let fit = FitRect::full(3.0, 1.0);
        let step = r.step((3.0, 1.0), 5.0, -5.0, fit, false);
        assert_eq!(step.pos, (1.5, 0.5), "collapsed to the centre");
    }

    #[test]
    fn exact_match_is_the_full_view() {
        let f = aspect_fit(1728, 1117, 1728.0, 1117.0);
        assert_eq!(f, FitRect::full(1728.0, 1117.0));
        assert_eq!(f.x, 0.0);
        assert_eq!(f.y, 0.0);
    }

    #[test]
    fn wider_view_pillarboxes_centered() {
        // 4:3 guest in a doubly-wide view: full height, centered horizontally.
        let f = aspect_fit(400, 300, 1600.0, 600.0);
        assert_eq!((f.w, f.h), (800.0, 600.0));
        assert_eq!((f.x, f.y), (400.0, 0.0));
    }

    #[test]
    fn taller_view_letterboxes_centered() {
        let f = aspect_fit(800, 600, 800.0, 1000.0);
        assert_eq!((f.w, f.h), (800.0, 600.0));
        assert_eq!((f.x, f.y), (0.0, 200.0));
    }

    #[test]
    fn small_guest_upscales() {
        let f = aspect_fit(640, 480, 1280.0, 1080.0);
        assert_eq!((f.w, f.h), (1280.0, 960.0));
        assert_eq!((f.x, f.y), (0.0, 60.0));
    }

    #[test]
    fn degenerate_inputs_fall_back_to_full_view() {
        assert_eq!(
            aspect_fit(0, 600, 800.0, 600.0),
            FitRect::full(800.0, 600.0)
        );
        assert_eq!(
            aspect_fit(800, 0, 800.0, 600.0),
            FitRect::full(800.0, 600.0)
        );
        assert_eq!(aspect_fit(800, 600, 0.0, 600.0), FitRect::full(0.0, 600.0));
        assert_eq!(
            aspect_fit(800, 600, 800.0, -1.0),
            FitRect::full(800.0, -1.0)
        );
    }

    #[test]
    fn odd_sizes_never_overflow_the_view() {
        let f = aspect_fit(1366, 768, 1117.0, 903.0);
        assert!(f.w <= 1117.0 && f.h <= 903.0);
        assert!(f.x >= 0.0 && f.y >= 0.0);
        assert!(
            (f.w / f.h - 1366.0 / 768.0).abs() < 1e-9,
            "aspect preserved"
        );
    }

    #[test]
    fn abs_corners_map_to_extremes() {
        let fit = FitRect {
            x: 100.0,
            y: 50.0,
            w: 800.0,
            h: 600.0,
        };
        // Bottom-left of the content = X min, Y MAX (evdev Y grows downward).
        assert_eq!(abs_through_fit(100.0, 50.0, fit, ABS_MAX), (0, ABS_MAX));
        // Top-right of the content = X max, Y min.
        assert_eq!(abs_through_fit(900.0, 650.0, fit, ABS_MAX), (ABS_MAX, 0));
        // Center maps to the middle of both axes.
        let (cx, cy) = abs_through_fit(500.0, 350.0, fit, ABS_MAX);
        assert_eq!((cx, cy), (ABS_MAX / 2 + 1, ABS_MAX / 2 + 1));
    }

    #[test]
    fn bar_points_clamp_to_content_edges() {
        let fit = FitRect {
            x: 100.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        // A point in the left pillarbox bar clamps to X = 0.
        assert_eq!(abs_through_fit(20.0, 300.0, fit, ABS_MAX).0, 0);
        // A point in the right bar clamps to X = max.
        assert_eq!(abs_through_fit(950.0, 300.0, fit, ABS_MAX).0, ABS_MAX);
    }

    #[test]
    fn full_view_fit_matches_the_legacy_mapping() {
        // In dynamic mode fit ≡ the full view; the transform must equal the historic
        // (p / bounds, Y-flipped) mapping bit-for-bit.
        let fit = FitRect::full(1024.0, 768.0);
        let (x, y) = abs_through_fit(512.0, 192.0, fit, ABS_MAX);
        let fx = (512.0 / 1024.0f64).clamp(0.0, 1.0);
        let fy = (1.0 - 192.0 / 768.0f64).clamp(0.0, 1.0);
        assert_eq!(x, (fx * f64::from(ABS_MAX)).round() as i32);
        assert_eq!(y, (fy * f64::from(ABS_MAX)).round() as i32);
    }

    #[test]
    fn default_window_is_half_the_screen_area_at_guest_aspect() {
        // 16:9 guest on a 2560×1440 screen: half of 3,686,400 pt² at 16:9.
        let (w, h) = default_window_content((2560, 1440), (2560.0, 1440.0), (2560.0, 1415.0));
        assert_eq!((w, h), (1810, 1018));
        let area = f64::from(w) * f64::from(h);
        assert!((area / (2560.0 * 1440.0) - 0.5).abs() < 0.01, "≈ half area");

        // A 4:3 fixed guest keeps ITS aspect (no letterbox on first show).
        let (w, h) = default_window_content((800, 600), (2560.0, 1440.0), (2560.0, 1415.0));
        assert!((f64::from(w) / f64::from(h) - 4.0 / 3.0).abs() < 0.01);

        // A guest much wider than the screen clamps into the visible frame.
        let (w, h) = default_window_content((10000, 1000), (1440.0, 900.0), (1440.0, 875.0));
        assert!(f64::from(w) <= 1440.0 && f64::from(h) <= 875.0 - 28.0);
        assert!(
            (f64::from(w) / f64::from(h) - 10.0).abs() < 0.1,
            "aspect kept"
        );

        // Degenerate inputs floor at 64 and never panic.
        let (w, h) = default_window_content((0, 0), (0.0, 0.0), (0.0, 0.0));
        assert!(w >= 64 && h >= 64);
    }

    #[test]
    fn reshape_preserves_area_and_takes_the_new_aspect() {
        // A 16:9 window (1600×900) migrated to a 16:10 display: keep ~the same area, adopt 16:10.
        let visible = (2560.0, 1415.0);
        let (w, h) = reshape_to_aspect((1600.0, 900.0), (1920, 1200), visible);
        assert!(
            (f64::from(w) / f64::from(h) - 16.0 / 10.0).abs() < 0.01,
            "adopts the new aspect"
        );
        let (before, after) = (1600.0 * 900.0, f64::from(w) * f64::from(h));
        assert!((after / before - 1.0).abs() < 0.02, "area preserved");

        // Migrating onto a small display clamps into its visible frame (aspect kept).
        let (w, h) = reshape_to_aspect((3000.0, 2000.0), (1512, 982), (1512.0, 950.0));
        assert!(f64::from(w) <= 1512.0 && f64::from(h) <= 950.0 - 28.0);
        assert!((f64::from(w) / f64::from(h) - 1512.0 / 982.0).abs() < 0.02);

        // Degenerate inputs floor at 64 and never panic.
        let (w, h) = reshape_to_aspect((0.0, 0.0), (0, 0), (0.0, 0.0));
        assert!(w >= 64 && h >= 64);
    }

    #[test]
    fn capture_step_moves_in_view_space_with_y_down() {
        let fit = FitRect {
            x: 100.0,
            y: 50.0,
            w: 800.0,
            h: 600.0,
        };
        // A downward AppKit delta (positive dy) lowers y in this bottom-left space; motion
        // inside the content has no overflow.
        let s = capture_step(Some((500.0, 350.0)), 10.0, 20.0, fit);
        assert_eq!(s.pos, (510.0, 330.0));
        assert_eq!(s.overflow, (0.0, 0.0));
    }

    #[test]
    fn capture_step_seeds_at_the_content_centre() {
        let fit = FitRect {
            x: 100.0,
            y: 50.0,
            w: 800.0,
            h: 600.0,
        };
        assert_eq!(capture_step(None, 0.0, 0.0, fit).pos, (500.0, 350.0));
    }

    #[test]
    fn capture_step_clamps_to_the_content_edges() {
        let fit = FitRect {
            x: 100.0,
            y: 50.0,
            w: 800.0,
            h: 600.0,
        };
        // A huge delta pins to the edge — the guest edge contains the cursor like the host
        // screen edge does outside capture.
        let s = capture_step(Some((500.0, 350.0)), 1e6, -1e6, fit);
        assert_eq!(s.pos, (900.0, 650.0));
        let s = capture_step(Some((500.0, 350.0)), -1e6, 1e6, fit);
        assert_eq!(s.pos, (100.0, 50.0));
    }

    #[test]
    fn capture_step_reports_the_clamped_off_overflow_as_deltas() {
        let fit = FitRect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        // Pinned at the left edge, pushing further left: the position stays put and the
        // eaten leftward component comes back as a negative-x delta (pressure for the guest).
        let s = capture_step(Some((0.0, 300.0)), -30.0, 0.0, fit);
        assert_eq!(s.pos, (0.0, 300.0));
        assert_eq!(s.overflow, (-30.0, 0.0));
        // Pinned at the top (view y = h), pushing further up (negative AppKit dy): overflow
        // comes back in delta convention, negative y = up.
        let s = capture_step(Some((400.0, 600.0)), 0.0, -25.0, fit);
        assert_eq!(s.pos, (400.0, 600.0));
        assert_eq!(s.overflow, (0.0, -25.0));
        // A diagonal shove into the top-left corner keeps both components.
        let s = capture_step(Some((0.0, 600.0)), -10.0, -15.0, fit);
        assert_eq!(s.pos, (0.0, 600.0));
        assert_eq!(s.overflow, (-10.0, -15.0));
        // Partial overflow: only the past-the-edge part is reported.
        let s = capture_step(Some((5.0, 300.0)), -30.0, 0.0, fit);
        assert_eq!(s.pos, (0.0, 300.0));
        assert_eq!(s.overflow, (-25.0, 0.0));
    }

    #[test]
    fn capture_step_repins_a_stale_position_into_a_new_fit() {
        // The fit shrank (e.g. fullscreen toggled off mid-capture): a zero-delta step
        // re-pins the old position into the new content — and the repin distance must NOT
        // masquerade as motion overflow (it wasn't input).
        let fit = FitRect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 300.0,
        };
        let s = capture_step(Some((1000.0, 900.0)), 0.0, 0.0, fit);
        assert_eq!(s.pos, (400.0, 300.0));
        assert_eq!(s.overflow, (0.0, 0.0));
    }

    #[test]
    fn capture_step_survives_a_degenerate_fit() {
        let degenerate = FitRect::default();
        assert_eq!(capture_step(None, 5.0, 5.0, degenerate).pos, (0.0, 0.0));
    }

    #[test]
    fn point_in_fit_excludes_the_bars() {
        let fit = FitRect {
            x: 100.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        assert!(point_in_fit(100.0, 0.0, fit));
        assert!(point_in_fit(900.0, 600.0, fit));
        assert!(point_in_fit(500.0, 300.0, fit));
        assert!(!point_in_fit(99.0, 300.0, fit), "left bar");
        assert!(!point_in_fit(901.0, 300.0, fit), "right bar");
        let degenerate = FitRect::default();
        assert!(!point_in_fit(0.0, 0.0, degenerate));
    }

    #[test]
    fn scale_converts_points_and_pixels_round_trip() {
        let two = Scale::new(2.0, true);
        assert_eq!(two.to_guest((1512.0, 982.0)), (3024, 1964));
        assert_eq!(two.to_points((3024, 1964)), (1512.0, 982.0));

        // A 1x display is unaffected even with HiDPI on.
        let one = Scale::new(1.0, true);
        assert_eq!(one.to_guest((2560.0, 1440.0)), (2560, 1440));

        // HiDPI off pins the historical point-for-pixel behavior on any panel.
        assert_eq!(Scale::new(2.0, false), Scale::none());
        assert_eq!(
            Scale::new(2.0, false).to_guest((1512.0, 982.0)),
            (1512, 982)
        );
    }

    /// AppKit is the source of the backing factor, so a nonsense value must degrade to the
    /// historical behavior rather than produce a zero- or NaN-sized guest mode.
    #[test]
    fn a_degenerate_backing_factor_falls_back_to_one() {
        for bad in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            assert_eq!(Scale::new(bad, true), Scale::none(), "backing {bad}");
        }
        assert_eq!(Scale::new(2.0, true).to_guest((0.0, 0.0)), (1, 1));
    }
}
