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

/// Where the housing-strip window goes, and where its copy of the guest layer sits inside it.
///
/// The `extend` strip is a second window covering only the camera-housing band, showing the top
/// `inset` points of the *same* guest image the carrier is showing below it. Both windows give
/// their layer the identical geometry in **panel** space — origin at the bottom-left of the glass
/// — and let each window's own bounds do the clipping. The carrier's view stops `inset` short of
/// the top, so the top of the image falls outside it; the strip's window covers exactly that band,
/// so the layer inside it is the same rect shifted down by the carrier's height.
///
/// Returns `(strip_frame_in_screen_points, layer_frame_in_strip_points)`. Screen coordinates are
/// Cocoa's (bottom-left origin), which is why the strip sits at `screen.height - inset`.
pub(crate) fn notch_strip_frames(
    screen: (f64, f64, f64, f64),
    carrier_height: f64,
    inset: f64,
    layer: FitRect,
) -> ((f64, f64, f64, f64), FitRect) {
    let (sx, sy, sw, sh) = screen;
    let strip = (sx, sy + sh - inset, sw, inset);
    let shifted = FitRect {
        x: layer.x,
        y: layer.y - carrier_height,
        w: layer.w,
        h: layer.h,
    };
    (strip, shifted)
}

/// The area the guest is drawn into, in points: the carrier's content view, plus the housing band
/// when the `extend` strip is covering it. One number decides the letterbox fit, the pointer
/// mapping and both layer frames, so the guest cannot disagree with itself about how tall it is.
pub(crate) fn panel_size(view: (f64, f64), strip_inset: f64) -> (f64, f64) {
    (view.0, view.1 + strip_inset.max(0.0))
}

/// The scales a desktop can offer for a given resolution, as exact rationals. Every one of these
/// was observed in a guest GNOME's `GetCurrentState` output; the list is a *ruler* for comparing
/// two candidate resolutions, not a claim about any particular compositor's menu.
const SCALE_CANDIDATES: [(u32, u32); 13] = [
    (1, 1),
    (5, 4),
    (4, 3),
    (3, 2),
    (5, 3),
    (7, 4),
    (2, 1),
    (9, 4),
    (7, 3),
    (5, 2),
    (8, 3),
    (11, 4),
    (3, 1),
];

/// How many of [`SCALE_CANDIDATES`] a `w`×`h` guest can actually be scaled by.
///
/// A compositor will only offer a scale whose *logical* size is whole — fractional logical pixels
/// have nowhere to land — so `a/b` is offerable exactly when `a` divides both dimensions after
/// multiplying through by `b`.
pub(crate) fn offerable_scales(w: u32, h: u32) -> u32 {
    SCALE_CANDIDATES
        .iter()
        .filter(|(a, b)| (w * b) % a == 0 && (h * b) % a == 0)
        .count() as u32
}

/// Give up at most this many pixels of guest height to earn a better scale menu. Four is
/// deliberately small: it is two points on a Retina panel — a letterbox band nobody can see — and
/// a wider budget starts *losing* useful scales rather than gaining them (at 8 px the 14" panel
/// snaps to 1890, which trades 133% away for 175%).
const SNAP_BUDGET: u32 = 4;

/// Trim a guest resolution to one the guest's desktop can actually scale.
///
/// The exact fullscreen height of a panel is whatever the glass and the camera housing add up to,
/// and it is often hostile: the 14" MacBook Pro's 3024×1898 shares only a factor of two between
/// its dimensions, so GNOME can offer 100% and 200% and nothing else, while a 2560×1440 monitor
/// beside it offers seven scales. Two pixels off the height turns that into six.
///
/// Only the height moves, only downward, and only when it strictly buys something — a resolution
/// that already scales well keeps its **exact** fit and its zero letterbox. The cost when it does
/// move is a band up to [`SNAP_BUDGET`] px tall, which the host-mode letterbox already draws.
pub(crate) fn snap_to_scalable(size: (u32, u32)) -> (u32, u32) {
    let (w, h) = size;
    if w == 0 || h <= 64 + SNAP_BUDGET {
        return size;
    }
    let mut best = (h, offerable_scales(w, h));
    for candidate in (h - SNAP_BUDGET..h).rev() {
        let scales = offerable_scales(w, candidate);
        if scales > best.1 {
            best = (candidate, scales);
        }
    }
    (w, best.0)
}

/// The migration reshape as a *policy*: [`reshape_to_aspect`], but only when the window is
/// ours to reshape. `None` in native fullscreen — there the window's shape IS the screen's,
/// AppKit owns it, and the visible frame the clamp uses (screen minus menu bar and Dock)
/// describes an area the window is deliberately not confined to.
///
/// Reshaping a fullscreen window is not the no-op it looks like: `setContentSize` shrinks the
/// content view *inside* the fullscreen window, which is also the view
/// [`fullscreen_inset_measurement`] measures the camera-housing inset from. The two then chase
/// each other — shorter view → bigger apparent inset → shorter guest → shorter view — settling
/// at an arbitrary fixed point well short of the panel. Observed 2026-08-08: a host-display
/// hotplug (and, it turned out, any fullscreen restore) drove eight modesets converging on
/// 2560×1326 instead of 2560×1440, and the drifting final size then stopped matching the mode
/// GNOME had remembered for that monitor, so the guest lost its saved scale too.
pub(crate) fn migration_reshape(
    current: (f64, f64),
    aspect: (u32, u32),
    visible: (f64, f64),
    fullscreen: bool,
) -> Option<(u32, u32)> {
    (!fullscreen).then(|| reshape_to_aspect(current, aspect, visible))
}

/// What a fullscreen content view says the camera housing costs — `None` when it says nothing.
///
/// The sensor is only honest when the window is actually filling the panel, so a view that is
/// not the screen's full width is rejected outright: mid-transition ticks (a display coming or
/// going) and any transient reshape report a short view whose height has nothing to do with the
/// housing. A plain magnitude bound does not catch those — 151 pt of "inset" is a perfectly
/// plausible-looking number. Returns points, `0.0` on a notchless display being a real answer.
pub(crate) fn fullscreen_inset_measurement(screen: (f64, f64), view: (f64, f64)) -> Option<f64> {
    const WIDTH_TOLERANCE: f64 = 1.0;
    if !(screen.0.is_finite() && screen.1 > 0.0 && view.0.is_finite() && view.1 > 0.0) {
        return None;
    }
    if (screen.0 - view.0).abs() > WIDTH_TOLERANCE {
        return None;
    }
    let inset = screen.1 - view.1;
    (0.0..=200.0).contains(&inset).then_some(inset)
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

/// How far inside the content the captured cursor is parked. Clear of any screen-edge trigger
/// (menu bar, Dock, hot corner) with room to spare, and small enough that the park point is a
/// short hop from wherever the pointer already was.
///
/// **Tied to [`REGRAB_MARGIN`], not chosen independently.** The automatic re-grab only fires on a
/// pointer that is already that margin clear of every edge, so an inset no larger than it leaves
/// nothing to pull and makes every policy grab a zero-length warp *by construction*. When this was
/// the larger of the two (64 against 40) a re-grab could still warp up to 24 points, and a warp's
/// whole vector arrives as guest motion — the cursor skipping the instant the grab took hold. See
/// `a_pointer_the_policy_may_regrab_is_already_its_own_park_point`.
pub(crate) const PARK_INSET: f64 = REGRAB_MARGIN;

/// Where to park the hidden host cursor while captured: where it already is, pulled far enough
/// inside the content that no screen-edge trigger can reach it.
///
/// **Not the main display's centre**, which is what this replaces, and the difference is not
/// cosmetic. A warp posts a motion event whose delta is the whole vector of the warp, and the
/// captured path integrates deltas into the virtual cursor — so parking somewhere far away
/// injects that entire distance as phantom guest motion. Measured on a two-display Mac whose VM
/// was fullscreen on the display *left of and below* the main one: every grab injected 1400-2400
/// points, flinging the guest cursor into a corner (`[GRAB]` trace).
///
/// Parking where the cursor already is makes the grab warp zero-length, so there is no delta to
/// inject and nothing to detect afterwards. The per-event re-pin that follows is zero-length too,
/// since a disassociated cursor does not move.
pub(crate) fn park_point(pos: Option<(f64, f64)>, fit: FitRect) -> (f64, f64) {
    pull_inside(capture_step(pos, 0.0, 0.0, fit).pos, fit, PARK_INSET)
}

/// Clamp a view point to be at least `inset` inside the content on every side.
///
/// Content thinner than two insets collapses to its own centre rather than producing an
/// inside-out range — `f64::clamp` panics when min > max, and a mid-resize tick can get there.
pub(crate) fn pull_inside(p: (f64, f64), fit: FitRect, inset: f64) -> (f64, f64) {
    let pull = |v: f64, lo: f64, len: f64| {
        if len <= 2.0 * inset {
            lo + len.max(0.0) / 2.0
        } else {
            v.clamp(lo + inset, lo + len - inset)
        }
    };
    (pull(p.0, fit.x, fit.w), pull(p.1, fit.y, fit.h))
}

/// How far inside the content an *in-place* release puts the cursor.
///
/// One point, and it is not cosmetic. A view point on the content's own boundary can map to a
/// global pixel one past the display: the bottom row of a 982-point window at CG y ∈ [879, 1861]
/// converts to y = 1861, and the display's last row is 1860. `CGWarpMouseCursorPosition` does not
/// reject that — it clamps into the display union, so a bottom-edge release landed on the
/// neighbouring screen (measured eight times in one session). Releasing a point inside
/// the content cannot be off it.
pub(crate) const RELEASE_INSET: f64 = 1.0;

/// How close to a corner counts as being *at* it — where the grab is never released and the
/// chrome ask never arms, because corners belong to the guest (the top-left one is the GNOME
/// overview trigger). Shared by both policies so they cannot overlap by construction rather than
/// by tuning; see [`pressed_edge`] and `input::REVEAL_CORNER_KEEPOUT`.
pub(crate) const CORNER_ZONE: f64 = 32.0;

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
/// Two duties share that path and are otherwise unrelated: taking the pointer into the fullscreen
/// grab (`[display] edge-resistance`, a preference) and running the `notch = extend` chrome ask
/// (`InputState::reveal_step`, the only way back to the menu bar under the overlay). Sharing a
/// function made the second silently inherit the first's enable check, so `Edge resist: Off` left
/// an `extend` guest with no way to ask for the chrome — the third time the ask has been lost by
/// riding on a code path that exists for another reason. Deciding both here, from named inputs,
/// is what makes the independence testable.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct EdgeDuties {
    /// Feed this event to the chrome ask.
    pub ask: bool,
    /// Consider taking the pointer into the grab.
    pub grab: bool,
}

/// Decide both duties for one event. See [`EdgeDuties`].
pub(crate) fn edge_duties(fullscreen_and_key: bool, grab_enabled: bool) -> EdgeDuties {
    EdgeDuties {
        // Not conditioned on the grab: the ask is a `notch = extend` feature, and the overlay is
        // up (or not) regardless of what the preference says. With the grab OFF it is the only
        // way to the menu bar that exists.
        ask: fullscreen_and_key,
        grab: fullscreen_and_key && grab_enabled,
    }
}

/// Coordinate slop for "is the pointer against this edge". Positions arrive as f64 points that
/// have been through two affine conversions; an exact compare would miss by ulps.
const EPS: f64 = 0.001;

// This block is the pure half of the fullscreen pointer grab
// (`docs/design/fullscreen-pointer-grab.md`). It lands before the policy that calls it, on
// purpose: the review's likeliest-regression was re-grab oscillation, and the guard against it is
// only correct-by-construction if it is written and tested headless before anything is wired.
/// Which edge a press is against. `None` away from the edges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// The direction to place the released cursor, as a unit vector in view space (y grows up).
    pub(crate) fn outward(self) -> (f64, f64) {
        match self {
            Self::Left => (-1.0, 0.0),
            Self::Right => (1.0, 0.0),
            Self::Top => (0.0, 1.0),
            Self::Bottom => (0.0, -1.0),
        }
    }
}

/// How far outside the content the released cursor is placed.
///
/// Must clear [`REGRAB_MARGIN`] with room to spare, or the release lands inside the re-grab zone
/// and the grab takes the pointer straight back.
pub(crate) const RELEASE_OFFSET: f64 = 8.0;

/// How far inside the content the pointer must return before the grab may retake it.
///
/// The oscillation guard, and the reason it cannot be zero: on a single display a released cursor
/// **cannot leave the window**, because fullscreen *is* the screen and the window server pins the
/// pointer to the top row — which is still window territory. Re-grabbing on mere containment
/// would take the pointer back on the first
/// inward jitter, warp it to centre and hide it: worse than the flicking this design removes.
pub(crate) const REGRAB_MARGIN: f64 = 40.0;

/// How long the pointer must sit that far inside before the grab retakes it.
pub(crate) const REGRAB_DWELL: std::time::Duration = std::time::Duration::from_millis(250);

/// Which edge, if any, this position is pressed against, given the outward motion.
///
/// Both halves are required: being at an edge is just resting there, and outward motion away from
/// an edge is the ordinary business of crossing the content.
///
/// **Corners are never a press.** Inside [`CORNER_ZONE`] of one, this answers `None` however hard
/// the pointer pushes. The guest owns its corners — the top-left one opens the GNOME overview,
/// and reaching for it necessarily pushes into two edges at once — so a corner lean must charge
/// the guest's barrier (the clamped overflow keeps flowing to it) without ever costing the user
/// the pointer. Away from the corner, a diagonal still resolves to its dominant axis.
pub(crate) fn pressed_edge(cur: (f64, f64), dx: f64, dy: f64, fit: FitRect) -> Option<Edge> {
    if fit.w <= 0.0 || fit.h <= 0.0 {
        return None;
    }
    let near = |p: f64, lo: f64, len: f64| (p - lo).min(lo + len - p) <= CORNER_ZONE;
    if near(cur.0, fit.x, fit.w) && near(cur.1, fit.y, fit.h) {
        return None;
    }
    let at = |p: f64, lo: f64, hi: f64| (p <= lo + EPS, p >= hi - EPS);
    let (at_left, at_right) = at(cur.0, fit.x, fit.x + fit.w);
    let (at_bottom, at_top) = at(cur.1, fit.y, fit.y + fit.h);
    // View y grows up, deltas grow down: leaving over the top is a negative dy.
    let x_push = if at_right && dx > 0.0 {
        dx
    } else if at_left && dx < 0.0 {
        -dx
    } else {
        0.0
    };
    let y_push = if at_top && dy < 0.0 {
        -dy
    } else if at_bottom && dy > 0.0 {
        dy
    } else {
        0.0
    };
    if x_push <= 0.0 && y_push <= 0.0 {
        return None;
    }
    if x_push >= y_push {
        Some(if at_right { Edge::Right } else { Edge::Left })
    } else {
        Some(if at_top { Edge::Top } else { Edge::Bottom })
    }
}

/// Where to put the host cursor when the grab releases at `edge`: just outside the content, on
/// the axis of the press, keeping the other coordinate. One warp per release, not per event.
pub(crate) fn release_point(cur: (f64, f64), edge: Edge, fit: FitRect) -> (f64, f64) {
    let (ox, oy) = edge.outward();
    let x = match edge {
        Edge::Left => fit.x - RELEASE_OFFSET,
        Edge::Right => fit.x + fit.w + RELEASE_OFFSET,
        _ => cur.0,
    };
    let y = match edge {
        Edge::Top => fit.y + fit.h + RELEASE_OFFSET,
        Edge::Bottom => fit.y - RELEASE_OFFSET,
        _ => cur.1,
    };
    let _ = (ox, oy);
    (x, y)
}

/// Whether the pointer is [`REGRAB_MARGIN`] clear of every edge of the content — the "genuinely
/// back in the guest" test the grab arms on, and the thing whose *duration* the caller times.
pub(crate) fn deep_inside(cur: (f64, f64), fit: FitRect) -> bool {
    fit.w > 0.0
        && fit.h > 0.0
        && cur.0 >= fit.x + REGRAB_MARGIN
        && cur.0 <= fit.x + fit.w - REGRAB_MARGIN
        && cur.1 >= fit.y + REGRAB_MARGIN
        && cur.1 <= fit.y + fit.h - REGRAB_MARGIN
}

/// May the grab retake the pointer? Only well inside the content, after a dwell, with no button
/// held. See [`REGRAB_MARGIN`].
pub(crate) fn may_regrab(
    cur: (f64, f64),
    fit: FitRect,
    inside_for: Option<std::time::Duration>,
    buttons_down: bool,
) -> bool {
    !buttons_down && deep_inside(cur, fit) && inside_for.is_some_and(|d| d >= REGRAB_DWELL)
}

/// Per-event cap on the charge, so a long quiet interval cannot be banked as pushing.
pub(crate) const CHARGE_TICK_CAP: f64 = 0.05;

/// Idle longer than this and the charge is gone — the strokes have to belong to one gesture.
pub(crate) const CHARGE_DECAY: f64 = 0.4;

/// **Time spent pushing**, accumulated across the strokes of one gesture — the single currency
/// for every "press against an edge and mean it" decision in the app.
///
/// Distance is the wrong unit and this exists to stop it coming back. Distance rewards a hard
/// shove, and a hard shove is exactly what throwing the pointer at the top-left hot corner looks
/// like; it is also post-ballistics, so the same nominal push is either free or a wall depending
/// on how fast the hand moved (`spikes/edge-pressure/RESULTS.md` rounds 2-3). A duration is felt
/// directly and reads the same on a fast mouse and a slow one.
///
/// Charging *across* strokes rather than demanding one unbroken push is not a refinement either:
/// a trackpad stroke ends when the finger runs out of glass, so a run-based gesture is
/// unperformable on the hardware this app is used on. Every lift decays instead ([`CHARGE_DECAY`])
/// and every event is capped ([`CHARGE_TICK_CAP`]) so stillness cannot be banked as motion.
///
/// Two policies own instances of this — the `notch = extend` chrome ask
/// ([`super::input::InputState::reveal_step`]) and the fullscreen grab's edge release. They were
/// one gesture with two implementations once, and the copies drifted until a two-event flick could
/// summon the menu bar. One core, two thresholds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Charge {
    /// Seconds of motion actually spent pushing.
    charge: f64,
    /// Points of outward push accumulated over the same gesture — a floor against jitter, never
    /// the deciding quantity.
    push: f64,
    /// When the last pushing event arrived.
    last: Option<std::time::Instant>,
}

impl Charge {
    /// Account one pushing event: `distance` points of *outward* motion, arriving at `now`.
    /// Returns `(charge seconds, push points)` after the event.
    ///
    /// Only genuinely-pushing events belong here. An event that is pinned at the edge with a zero
    /// delta is neither a push nor a lapse — the caller simply does not call — and an event moving
    /// away is [`Self::lapse`].
    /// `decay` is the gesture's own grace period between strokes — [`CHARGE_DECAY`] for the chrome
    /// ask and the top edge, [`SIDE_DECAY`] for the sides (see [`edge_timing`]).
    pub(crate) fn push(
        &mut self,
        now: std::time::Instant,
        distance: f64,
        decay: f64,
    ) -> (f64, f64) {
        let idle = self
            .last
            .map_or(f64::INFINITY, |t| now.duration_since(t).as_secs_f64());
        if idle > decay {
            *self = Self::default();
        } else {
            self.charge += idle.min(CHARGE_TICK_CAP);
        }
        self.last = Some(now);
        self.push += distance.max(0.0);
        (self.charge, self.push)
    }

    /// Give the gesture up: the pointer moved away, or the policy is no longer interested.
    /// A finished gesture must leave nothing behind, or the next one inherits its charge and
    /// fires at once while a fresh one takes the full hold — the same gesture feeling different
    /// every time.
    pub(crate) fn lapse(&mut self) {
        *self = Self::default();
    }

    /// `(charge seconds, push points)` without accounting anything — for the trace and for
    /// threshold tests.
    pub(crate) fn get(&self) -> (f64, f64) {
        (self.charge, self.push)
    }
}

/// What fraction of the configured hold a press against a **side** edge has to earn.
///
/// The top edge and the sides are not the same gesture, which is why one number cannot serve both.
/// Pushing up asks for the macOS chrome and is aimed at a target the user can see; pushing sideways
/// is "let me out onto the other display" during ordinary travel, so it happens mid-motion with no
/// destination on screen to aim at. Dogfood, five rounds in: the top hold "feels great — I can
/// trigger it whenever I want and haven't done it accidentally", while the sides at the same
/// `Standard` felt "a bit too hard". The bottom counts as a side, not a third case.
pub(crate) const SIDE_HOLD_FACTOR: f64 = 0.6;

/// The grace period between the strokes of a **side** press — more forgiving than
/// [`CHARGE_DECAY`], for the same reason.
///
/// A side press is usually two or three shoves rather than one steady lean (the hand resets between
/// them), and at 0.4 s a natural reset threw the charge away and started the gesture over. This is
/// only the gap that may pass between pushes; stillness still cannot be *banked* as pushing, which
/// is [`CHARGE_TICK_CAP`]'s job.
pub(crate) const SIDE_DECAY: f64 = 0.9;

/// The side-edge feel, so a dogfood session can dial it without a rebuild (see `side_tuning` in
/// `capture_tap`). Defaults are [`SIDE_HOLD_FACTOR`] and [`SIDE_DECAY`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SideTuning {
    pub factor: f64,
    pub decay: f64,
}

impl Default for SideTuning {
    fn default() -> Self {
        Self {
            factor: SIDE_HOLD_FACTOR,
            decay: SIDE_DECAY,
        }
    }
}

/// `(hold seconds, decay seconds)` for a press against `edge`, given the configured hold.
///
/// One function so the asymmetry is stated once and testable, rather than living as a `match` in
/// the middle of the policy. The top keeps the configured hold exactly — that is the number dogfood
/// says is right — and the sides are scaled and given a longer grace period.
pub(crate) fn edge_timing(hold: f64, edge: Edge, side: SideTuning) -> (f64, f64) {
    match edge {
        Edge::Top => (hold, CHARGE_DECAY),
        Edge::Left | Edge::Right | Edge::Bottom => (hold * side.factor, side.decay),
    }
}

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

    /// A fullscreen-sized content area for the grab tests.
    fn screen_fit() -> FitRect {
        FitRect::full(1512.0, 949.0)
    }

    #[test]
    fn turning_the_grab_off_does_not_take_the_chrome_ask_with_it() {
        // `Edge resist: Off` + `notch = extend` + Accessibility granted used to leave no way to
        // reach the menu bar at all: the tap's edge path returned early on the disabled setting,
        // above its `reveal_step` call, and the local monitor stands down whenever the tap is
        // installed. The gesture worked only for users who had *not* granted Accessibility. It
        // matters more under the grab than it did under resistance: with the grab off there is no
        // edge-press release either, so this is the ONLY way out of the overlay.
        assert_eq!(
            edge_duties(true, false),
            EdgeDuties {
                ask: true,
                grab: false
            },
        );
        assert_eq!(
            edge_duties(true, true),
            EdgeDuties {
                ask: true,
                grab: true
            },
        );
    }

    #[test]
    fn neither_duty_applies_to_a_window_that_is_not_fullscreen_and_key() {
        // Windowed, the pointer must be free to leave (it is a window), and an unfocused window
        // has no business holding the pointer at all.
        for enabled in [true, false] {
            assert_eq!(
                edge_duties(false, enabled),
                EdgeDuties {
                    ask: false,
                    grab: false
                },
            );
        }
    }

    // --- fullscreen pointer grab (docs/design/fullscreen-pointer-grab.md) ------------------

    #[test]
    fn the_park_point_is_inside_our_own_content_never_another_display() {
        let fit = screen_fit();
        // Deep inside: parked exactly where the cursor already is, so the grab warp is a no-op.
        let deep = (700.0, 400.0);
        assert_eq!(park_point(Some(deep), fit), deep);
        // At an edge or corner: pulled clear of every screen-edge trigger, but only just — the
        // point of parking here rather than at the main display's centre is that the warp stays
        // short, because its length is injected as phantom guest motion.
        for corner in [
            (fit.x, fit.y),
            (fit.x + fit.w, fit.y + fit.h),
            (fit.x, fit.y + fit.h),
        ] {
            let p = park_point(Some(corner), fit);
            assert!(deep_inside(p, fit), "{corner:?} parked at {p:?}");
            let hop = (p.0 - corner.0).hypot(p.1 - corner.1);
            assert!(
                hop <= 2.0 * PARK_INSET,
                "{corner:?} -> {p:?} is a {hop} hop"
            );
        }
        // Never outside the content, whatever it is handed.
        let far = (99_999.0, -99_999.0);
        assert!(deep_inside(park_point(Some(far), fit), fit));
        assert!(deep_inside(park_point(None, fit), fit));
    }

    #[test]
    fn a_pointer_the_policy_may_regrab_is_already_its_own_park_point() {
        // The coupling that makes the automatic re-grab warp-free BY CONSTRUCTION rather than by
        // luck: the policy only re-grabs a pointer that is `REGRAB_MARGIN` clear of every edge, so
        // if the park inset is no larger than that margin, `park_point` has nothing to pull and the
        // grab warp is exactly zero length. When the inset was the larger of the two (64 vs 40) an
        // automatic re-grab could still warp up to 24 points — and a warp's whole vector arrives as
        // guest motion, which is the cursor jumping the instant the grab took hold. Dogfood found it
        // on re-entry from another display: reach the guest's top-right menu, start moving toward an
        // item, and the pointer skips.
        let fit = screen_fit();
        let long = Some(REGRAB_DWELL);
        for p in [
            (fit.x + REGRAB_MARGIN, fit.y + REGRAB_MARGIN),
            (fit.x + fit.w - REGRAB_MARGIN, fit.y + REGRAB_MARGIN),
            (fit.x + REGRAB_MARGIN, fit.y + fit.h - REGRAB_MARGIN),
            (fit.x + fit.w - REGRAB_MARGIN, fit.y + fit.h - REGRAB_MARGIN),
            (fit.x + fit.w - REGRAB_MARGIN, 400.0),
            (700.0, 400.0),
        ] {
            assert!(may_regrab(p, fit, long, false), "{p:?} is not regrabbable");
            assert_eq!(park_point(Some(p), fit), p, "{p:?} would warp on re-grab");
        }
    }

    #[test]
    fn an_in_place_release_never_lands_on_the_content_boundary() {
        // The boundary is where the off-by-one lives: a view point on it can convert to a global
        // pixel one past the display, and the window server clamps that onto a neighbouring
        // screen. Every edge, not just the bottom that caught it.
        let fit = screen_fit();
        for p in [
            (400.0, fit.y),
            (400.0, fit.y + fit.h),
            (fit.x, 400.0),
            (fit.x + fit.w, 400.0),
        ] {
            let r = pull_inside(p, fit, RELEASE_INSET);
            assert!(point_in_fit(r.0, r.1, fit));
            assert!(r.0 > fit.x && r.0 < fit.x + fit.w, "{p:?} -> {r:?}");
            assert!(r.1 > fit.y && r.1 < fit.y + fit.h, "{p:?} -> {r:?}");
            // And it stays where the user pushed, to within the inset.
            assert!((r.0 - p.0).hypot(r.1 - p.1) <= RELEASE_INSET * 1.5);
        }
    }

    #[test]
    fn a_content_area_thinner_than_the_park_inset_does_not_panic() {
        // `f64::clamp` panics when min > max, and a mid-resize tick can produce a fit this thin.
        let thin = FitRect {
            x: 10.0,
            y: 10.0,
            w: 4.0,
            h: 4.0,
        };
        assert_eq!(park_point(Some((0.0, 0.0)), thin), (12.0, 12.0));
        assert_eq!(park_point(None, FitRect::default()), (0.0, 0.0));
    }

    #[test]
    fn a_corner_is_never_a_press_however_hard_it_is_pushed() {
        // The guest owns its corners. Leaning into the top-left one is how the GNOME overview is
        // opened, and it necessarily pushes into two edges at once — releasing the pointer there
        // would hand the user the other display at the exact moment they asked for the overview.
        // The clamped overflow still reaches the guest while the lean continues, so the barrier
        // charges; only the *release* is withheld.
        let fit = screen_fit();
        let (top, right) = (fit.y + fit.h, fit.x + fit.w);
        for (p, d) in [
            ((0.0, top), (-40.0, -40.0)),
            ((0.0, top), (-4.0, -40.0)),
            ((0.0, top), (-40.0, -4.0)),
            ((right, top), (40.0, -40.0)),
            ((0.0, 0.0), (-40.0, 40.0)),
            ((right, 0.0), (40.0, 40.0)),
        ] {
            assert_eq!(pressed_edge(p, d.0, d.1, fit), None, "{p:?} {d:?}");
        }
    }

    #[test]
    fn clear_of_the_corner_a_diagonal_resolves_to_its_dominant_axis() {
        let fit = screen_fit();
        let top = fit.y + fit.h;
        let mid_y = fit.y + fit.h / 2.0;
        let mid_x = fit.x + fit.w / 2.0;
        assert_eq!(
            pressed_edge((0.0, mid_y), -30.0, -4.0, fit),
            Some(Edge::Left)
        );
        assert_eq!(
            pressed_edge((mid_x, top), -4.0, -30.0, fit),
            Some(Edge::Top)
        );
        // And the corner zone is a band, not a point: just past it, the edge is pressable again.
        let just_clear = fit.y + fit.h - CORNER_ZONE - 1.0;
        assert_eq!(
            pressed_edge((0.0, just_clear), -30.0, 0.0, fit),
            Some(Edge::Left)
        );
    }

    #[test]
    fn resting_against_an_edge_is_not_a_press() {
        // Being there is not pushing; that distinction is what keeps a parked cursor from
        // releasing itself.
        let fit = screen_fit();
        assert_eq!(pressed_edge((0.0, 400.0), 0.0, 0.0, fit), None);
        assert_eq!(
            pressed_edge((0.0, 400.0), 30.0, 0.0, fit),
            None,
            "inward is not a press"
        );
        assert_eq!(
            pressed_edge((700.0, 400.0), -30.0, 0.0, fit),
            None,
            "not at an edge"
        );
    }

    #[test]
    fn the_release_point_clears_the_regrab_zone() {
        // The oscillation the review predicted: release the cursor 2 pt out, the first inward
        // jitter re-grabs it, warps to centre and hides it. The release must land outside, and
        // re-grab must want a real margin inside.
        let fit = screen_fit();
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let p = release_point((700.0, 400.0), edge, fit);
            assert!(
                !point_in_fit(p.0, p.1, fit),
                "{edge:?} release lands outside the content"
            );
            assert!(
                !may_regrab(p, fit, Some(std::time::Duration::from_secs(10)), false),
                "{edge:?} release point must not immediately satisfy re-grab"
            );
        }
    }

    #[test]
    fn regrab_needs_margin_dwell_and_no_button() {
        let fit = screen_fit();
        let deep = (700.0, 400.0);
        let long = Some(std::time::Duration::from_secs(1));
        assert!(may_regrab(deep, fit, long, false));
        assert!(!may_regrab(deep, fit, long, true), "never mid-drag");
        assert!(!may_regrab(
            deep,
            fit,
            Some(std::time::Duration::from_millis(50)),
            false
        ));
        assert!(!may_regrab(deep, fit, None, false));
        // Just inside the boundary is not "inside" — this is the single-display case where the
        // released cursor is pinned to the content's own top row.
        let pinned = (700.0, fit.y + fit.h);
        assert!(
            !may_regrab(pinned, fit, long, false),
            "the top row is not a return"
        );
    }

    /// Feed `n` events `dt` apart, each pushing `dist` points, and answer with the final charge.
    /// Baseline decay — the side edges' longer grace period has its own tests.
    fn charge_run(c: &mut Charge, t0: std::time::Instant, n: u32, dt_ms: u64, dist: f64) -> f64 {
        let mut charge = 0.0;
        for i in 1..=n {
            charge = c
                .push(
                    t0 + std::time::Duration::from_millis(dt_ms * u64::from(i)),
                    dist,
                    CHARGE_DECAY,
                )
                .0;
        }
        charge
    }

    #[test]
    fn charge_measures_time_pushing_not_distance_pushed() {
        let t0 = std::time::Instant::now();
        // One enormous shove in a single event: a flick. Nearly no time was spent pushing, so
        // nearly no charge — however far the pointer travelled.
        let mut flick = Charge::default();
        let (c, push) = flick.push(t0, 400.0, CHARGE_DECAY);
        assert_eq!(c, 0.0, "the first event of a gesture charges nothing");
        assert_eq!(push, 400.0, "but the distance floor still sees it");
        // A deliberate lean: many small events over the same distance.
        let mut lean = Charge::default();
        let charged = charge_run(&mut lean, t0, 20, 20, 20.0);
        assert!(
            charged > 0.35,
            "20 events x 20 ms should charge ~0.4 s: {charged}"
        );
    }

    #[test]
    fn a_quiet_interval_cannot_be_banked_as_pushing() {
        let t0 = std::time::Instant::now();
        let mut c = Charge::default();
        c.push(t0, 10.0, CHARGE_DECAY);
        // 300 ms of stillness, then one more event: inside the decay window, so the gesture
        // survives — but it may only bank the per-event cap, not the whole quiet interval.
        let (charge, _) = c.push(
            t0 + std::time::Duration::from_millis(300),
            10.0,
            CHARGE_DECAY,
        );
        assert_eq!(charge, CHARGE_TICK_CAP);
    }

    #[test]
    fn charge_survives_a_lift_but_not_a_pause() {
        let t0 = std::time::Instant::now();
        let mut c = Charge::default();
        let before = charge_run(&mut c, t0, 10, 20, 10.0);
        // A trackpad lift: 300 ms, under the decay. The strokes belong to one gesture.
        let after = c
            .push(
                t0 + std::time::Duration::from_millis(500),
                10.0,
                CHARGE_DECAY,
            )
            .0;
        assert!(after > before, "a lift must not throw the gesture away");
        // A real pause: past the decay, the gesture is over and starts from nothing.
        let fresh = c
            .push(
                t0 + std::time::Duration::from_millis(500)
                    + std::time::Duration::from_secs_f64(CHARGE_DECAY * 2.0),
                10.0,
                CHARGE_DECAY,
            )
            .0;
        assert_eq!(fresh, 0.0);
    }

    #[test]
    fn lapsing_leaves_nothing_for_the_next_gesture() {
        let t0 = std::time::Instant::now();
        let mut c = Charge::default();
        charge_run(&mut c, t0, 10, 20, 10.0);
        c.lapse();
        assert_eq!(c.get(), (0.0, 0.0));
        // And the next event is a first event, not a continuation of the old clock.
        assert_eq!(
            c.push(
                t0 + std::time::Duration::from_millis(210),
                10.0,
                CHARGE_DECAY
            )
            .0,
            0.0
        );
    }

    #[test]
    fn the_hold_presets_are_all_reachable_by_a_lean_and_none_by_a_flick() {
        let t0 = std::time::Instant::now();
        for hold in crate::vmlib::schema::EdgeHold::ALL {
            if hold == crate::vmlib::schema::EdgeHold::Off {
                continue;
            }
            // A flick: three events in 24 ms, the shape measured for a corner throw.
            let mut flick = Charge::default();
            assert!(
                charge_run(&mut flick, t0, 3, 8, 60.0) < hold.seconds(),
                "{hold:?} must not fire on a flick"
            );
            // A lean: 60 events at 16 ms (one per frame for a second).
            let mut lean = Charge::default();
            assert!(
                charge_run(&mut lean, t0, 60, 16, 8.0) >= hold.seconds(),
                "{hold:?} must be reachable by a one-second lean"
            );
        }
    }

    #[test]
    fn a_side_press_asks_less_than_a_top_one_and_forgives_a_longer_pause() {
        // Dogfood's verdict after five rounds: the top hold is right, the sides at the same number
        // are "a bit too hard". They are different gestures — pushing up aims at a visible target,
        // pushing sideways happens mid-travel — so one number cannot serve both.
        let side = SideTuning::default();
        let hold = crate::vmlib::schema::EdgeHold::Standard.seconds();
        let (top_hold, top_decay) = edge_timing(hold, Edge::Top, side);
        assert_eq!((top_hold, top_decay), (hold, CHARGE_DECAY), "top unchanged");
        for edge in [Edge::Left, Edge::Right, Edge::Bottom] {
            let (h, d) = edge_timing(hold, edge, side);
            assert!(h < top_hold, "{edge:?} must ask less than the top: {h}");
            assert!(d > top_decay, "{edge:?} must forgive longer: {d}");
        }
        // The bottom is a side, not a third case.
        assert_eq!(
            edge_timing(hold, Edge::Bottom, side),
            edge_timing(hold, Edge::Left, side)
        );
        // And the tuning knob is honoured, so a dogfood session can dial this without a rebuild.
        let dialled = SideTuning {
            factor: 0.5,
            decay: 1.5,
        };
        assert_eq!(edge_timing(1.0, Edge::Right, dialled), (0.5, 1.5));
    }

    #[test]
    fn the_longer_side_grace_keeps_a_multi_shove_gesture_alive() {
        // A side press is two or three shoves with a hand reset between them, not one steady lean.
        // At the baseline decay that reset threw the charge away; at the side decay it survives.
        let t0 = std::time::Instant::now();
        let gap = std::time::Duration::from_secs_f64((CHARGE_DECAY + SIDE_DECAY) / 2.0);
        let mut side = Charge::default();
        let before = charge_run(&mut side, t0, 10, 16, 8.0);
        let after = side.push(t0 + gap, 8.0, SIDE_DECAY).0;
        assert!(
            after > before,
            "a hand reset must not end a side press: {before} -> {after}"
        );
        // Same gap, baseline decay: the gesture is over. This is the difference, stated as a test.
        let mut top = Charge::default();
        charge_run(&mut top, t0, 10, 16, 8.0);
        assert_eq!(top.push(t0 + gap, 8.0, CHARGE_DECAY).0, 0.0);
        // Stillness is still not bankable, however long the grace period is.
        let mut rest = Charge::default();
        rest.push(t0, 8.0, SIDE_DECAY);
        assert_eq!(rest.push(t0 + gap, 8.0, SIDE_DECAY).0, CHARGE_TICK_CAP);
    }

    #[test]
    fn no_side_preset_becomes_reachable_by_a_flick() {
        // The reduction must buy forgiveness, not accidents: the scaled hold still has to reject
        // the three-event corner throw that the full hold rejects.
        let t0 = std::time::Instant::now();
        let side = SideTuning::default();
        for hold in crate::vmlib::schema::EdgeHold::ALL {
            if hold == crate::vmlib::schema::EdgeHold::Off {
                continue;
            }
            let (scaled, _) = edge_timing(hold.seconds(), Edge::Right, side);
            let mut flick = Charge::default();
            assert!(
                charge_run(&mut flick, t0, 3, 8, 60.0) < scaled,
                "{hold:?} at the side ({scaled} s) must not fire on a flick"
            );
        }
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

    /// The `extend` strip shows the top of the same image the carrier shows the rest of, so the
    /// two layers must land on one continuous picture across the seam. Numbers are the 14" panel:
    /// 1512×982 glass, 33 pt of housing, so the carrier's fullscreen view is 949 tall.
    #[test]
    fn the_strip_continues_the_carrier_across_the_seam() {
        let (screen_w, screen_h) = (1512.0, 982.0);
        let carrier_h = screen_h - NOTCH; // 949 — what AppKit gives a fullscreen window
        let guest = FitRect::full(screen_w, screen_h); // extend: the guest fills the glass

        let (strip, layer) =
            notch_strip_frames((0.0, 0.0, screen_w, screen_h), carrier_h, NOTCH, guest);

        // The strip window covers exactly the band the carrier cannot reach.
        assert_eq!(strip, (0.0, carrier_h, screen_w, NOTCH));
        // Inside it, the layer is the same rect shifted down by the carrier's height, so the
        // image's row at panel-y 949 lands at the strip's own y=0 — no seam, no doubled row.
        assert_eq!(layer.y, -carrier_h);
        assert_eq!((layer.x, layer.w, layer.h), (guest.x, guest.w, guest.h));
        // The bottom of the strip's copy and the top of the carrier's copy meet exactly.
        assert_eq!(layer.y + layer.h, guest.h - carrier_h);

        // A screen that is not at the origin (a second display) keeps the same relationship.
        let (strip, _) = notch_strip_frames(
            (-1512.0, 300.0, screen_w, screen_h),
            carrier_h,
            NOTCH,
            guest,
        );
        assert_eq!(strip, (-1512.0, 300.0 + carrier_h, screen_w, NOTCH));
    }

    /// The guest is as tall as the area it is actually drawn into — and that changes when the
    /// strip stands down (for a dialog, or the chrome ask), which must letterbox rather than
    /// clip the top off.
    #[test]
    fn the_panel_is_the_view_plus_the_strip_when_the_strip_is_up() {
        assert_eq!(panel_size((1512.0, 949.0), NOTCH), (1512.0, 982.0));
        assert_eq!(panel_size((1512.0, 949.0), 0.0), (1512.0, 949.0));
        // A notchless display never has a strip, so the view is the whole story.
        assert_eq!(panel_size((2560.0, 1440.0), 0.0), (2560.0, 1440.0));
    }

    /// GNOME only offers a scale whose *logical* size comes out whole, so the scale menu the user
    /// gets is decided by the resolution we hand the guest. The 14" panel's exact fullscreen
    /// height, 1898 px, shares only a factor of 2 with its 3024 width — GNOME could offer 100% and
    /// 200% and nothing else, while the 2560×1440 BenQ beside it offered seven scales. Two pixels
    /// off the height buys 133/150/267/300% for a letterbox band one point tall.
    #[test]
    fn a_hostile_height_is_snapped_to_one_the_guest_can_scale() {
        // The 14" built-in at 2×, the case that prompted this.
        assert_eq!(snap_to_scalable((3024, 1898)), (3024, 1896));
        // …and at 1× (`--no-hidpi`), where the same panel is just as hostile.
        assert_eq!(snap_to_scalable((1512, 949)), (1512, 945));

        // Resolutions that already scale well are left EXACTLY alone: there is nothing to buy,
        // and an exact fit means no letterbox at all.
        for already_fine in [(2560, 1440), (3456, 2160), (1920, 1080), (3024, 1896)] {
            assert_eq!(snap_to_scalable(already_fine), already_fine);
        }

        // The budget is a hard ceiling — a snap never costs more than 4 px of height, never
        // touches the width, and never grows the guest (which would crop rather than letterbox).
        for h in 900..1000 {
            let (w, snapped) = snap_to_scalable((3024, h));
            assert_eq!(w, 3024, "width is never touched");
            assert!(snapped <= h && h - snapped <= 4, "{h} -> {snapped}");
            assert!(offerable_scales(3024, snapped) >= offerable_scales(3024, h));
        }

        // Degenerate and tiny sizes are returned untouched rather than shrunk toward zero.
        assert_eq!(snap_to_scalable((0, 0)), (0, 0));
        assert_eq!(snap_to_scalable((64, 64)), (64, 64));
    }

    /// A fullscreen window's shape is the screen's — AppKit owns it, and the visible frame
    /// (which excludes the menu bar and Dock) does not apply. Reshaping there anyway shrank the
    /// content view *inside* the fullscreen window, which poisoned the very measurement the
    /// guest's height is derived from ([`fullscreen_inset_measurement`]) and sent the two into a
    /// feedback loop: eight modesets converging 114 guest px short of the panel.
    ///
    /// Numbers verbatim from the `LIMINA_DISPLAY_TRACE=1` boot of 2026-08-08 that caught it — a
    /// fullscreen restore on the 14" built-in, no hotplug needed.
    #[test]
    fn a_fullscreen_window_is_never_reshaped() {
        // The first poisoned tick: a full-panel content view (1512×949) "reshaped" to 1324×831,
        // because the clamp used the visible frame (859 pt) rather than the panel.
        assert_eq!(
            migration_reshape((1512.0, 949.0), (3024, 1898), (1512.0, 859.0), true),
            None,
            "fullscreen: leave the window alone"
        );
        // Windowed, the reshape is exactly the shipped behavior — this is the case it exists for.
        assert_eq!(
            migration_reshape((1600.0, 900.0), (1920, 1200), (2560.0, 1415.0), false),
            Some(reshape_to_aspect(
                (1600.0, 900.0),
                (1920, 1200),
                (2560.0, 1415.0)
            )),
        );
    }

    /// The inset sensor only reads true when the window really is filling the panel. A view that
    /// is narrower than the screen is mid-transition (or mid-reshape) and its height says nothing
    /// about what the camera housing costs — believing it is what let one bad tick ratchet into
    /// the loop above, and 151 pt sailed through the plain 0..200 bounds check.
    #[test]
    fn the_inset_is_only_learned_from_a_full_width_fullscreen_view() {
        // The honest measurement: full-width view, 33 pt of housing.
        assert_eq!(
            fullscreen_inset_measurement((1512.0, 982.0), (1512.0, 949.0)),
            Some(33.0)
        );
        // The poisoned ones, both from the trace. Narrower than the panel ⇒ not a measurement.
        assert_eq!(
            fullscreen_inset_measurement((1512.0, 982.0), (1324.0, 831.0)),
            None
        );
        assert_eq!(
            fullscreen_inset_measurement((1512.0, 982.0), (1415.0, 778.0)),
            None
        );
        // A notchless external display in fullscreen measures zero, and zero is a real answer.
        assert_eq!(
            fullscreen_inset_measurement((2560.0, 1440.0), (2560.0, 1440.0)),
            Some(0.0)
        );
        // Nonsense never reaches the cache: a taller-than-screen view, or a degenerate screen.
        assert_eq!(
            fullscreen_inset_measurement((1512.0, 982.0), (1512.0, 1000.0)),
            None
        );
        assert_eq!(fullscreen_inset_measurement((0.0, 0.0), (0.0, 0.0)), None);
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
