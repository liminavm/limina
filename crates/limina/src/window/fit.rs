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

/// Fullscreen edge resistance: the pointer sticks at the guest's edge until *pushed* through.
///
/// In fullscreen the host cursor reaching the top edge instantly reveals the macOS menu bar and
/// title bar, and reaching a side edge leaves for the next display. Both are one careless flick
/// away, which makes a fullscreen guest feel leaky. Resistance makes leaving *deliberate*: motion
/// past an edge is absorbed (and forwarded to the guest as edge pressure, so the guest's own top
/// bar and hot corner keep working) until the accumulated outward push crosses `threshold`
/// points, at which point the pointer breaks through and behaves normally until it comes back
/// inside.
///
/// This is the uncaptured counterpart to pointer capture (Cmd-Ctrl-G), which prevents the same
/// escapes absolutely by parking the host cursor at screen centre.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EdgeResist {
    /// Points of accumulated outward push needed to break out. Zero (or negative) disables
    /// resistance entirely — every step comes back `free`.
    threshold: f64,
    /// Outward push accumulated at the current edge, per axis, as magnitudes.
    acc: (f64, f64),
    /// Broken through: stay out of the way until the pointer re-enters the content.
    through: bool,
}

impl EdgeResist {
    pub(crate) fn new(threshold: f64) -> Self {
        Self {
            threshold: if threshold.is_finite() {
                threshold
            } else {
                0.0
            },
            acc: (0.0, 0.0),
            through: false,
        }
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

    /// Advance by one motion delta from `pos` (view points, bottom-left origin; AppKit deltas,
    /// so positive `dy` is downward).
    pub(crate) fn step(&mut self, pos: (f64, f64), dx: f64, dy: f64, fit: FitRect) -> ResistStep {
        let unclamped = (pos.0 + dx, pos.1 - dy);
        let free = |pos| ResistStep {
            pos,
            overflow: (0.0, 0.0),
            free: true,
        };
        if !self.enabled() {
            return free(unclamped);
        }
        if self.through {
            // Already out. Re-arm only once the pointer is genuinely back inside the content —
            // re-arming at the boundary would fight the user on the way out.
            if point_in_fit(unclamped.0, unclamped.1, fit) {
                self.reset();
            } else {
                return free(unclamped);
            }
        }

        let step = capture_step(Some(pos), dx, dy, fit);
        if step.overflow == (0.0, 0.0) {
            // Not pushing against any edge: the accumulator is about a *sustained* shove, so an
            // event that doesn't push drains it. Without this, a hundred unrelated nudges over a
            // session would eventually add up to a breakthrough.
            self.acc = (0.0, 0.0);
            return free(unclamped);
        }

        self.acc = (
            self.acc.0 + step.overflow.0.abs(),
            self.acc.1 + step.overflow.1.abs(),
        );
        if self.acc.0.max(self.acc.1) >= self.threshold {
            self.through = true;
            self.acc = (0.0, 0.0);
            return free(unclamped);
        }
        ResistStep {
            pos: step.pos,
            overflow: step.overflow,
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

    /// A fullscreen-sized content area for the resistance tests.
    fn screen_fit() -> FitRect {
        FitRect::full(1512.0, 949.0)
    }

    #[test]
    fn resistance_pins_the_cursor_at_the_top_edge_and_reports_pressure() {
        let mut r = EdgeResist::new(100.0);
        // Sitting on the top edge, shoving up 10 pt (AppKit deltas: up is NEGATIVE dy).
        let step = r.step((700.0, 949.0), 0.0, -10.0, screen_fit());
        assert!(!step.free, "a 10 pt nudge must not reveal the menu bar");
        assert_eq!(
            step.pos,
            (700.0, 949.0),
            "the cursor stays pinned at the edge"
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
            assert!(!r.step((700.0, 949.0), 0.0, -10.0, fit).free);
        }
        // 10 x 10 pt reaches the threshold: the pointer is released and the chrome may appear.
        let out = r.step((700.0, 949.0), 0.0, -10.0, fit);
        assert!(out.free, "100 pt of push must break out");
        assert_eq!(out.pos, (700.0, 959.0), "and the cursor leaves the content");
        // Still out on the next event, without re-earning it.
        assert!(r.step((700.0, 959.0), 0.0, -10.0, fit).free);
    }

    #[test]
    fn sliding_along_an_edge_does_not_accumulate_a_breakthrough() {
        // The failure this guards: a purely horizontal drag along the top edge should never
        // trip the menu bar, no matter how long it goes on.
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        for _ in 0..50 {
            let step = r.step((700.0, 949.0), 12.0, 0.0, fit);
            assert!(
                step.free,
                "moving inside/along the content is never resisted"
            );
        }
        // ...and the accumulator is clean, so a fresh nudge is still resisted.
        assert!(!r.step((700.0, 949.0), 0.0, -10.0, fit).free);
    }

    #[test]
    fn pushes_separated_by_inward_motion_do_not_add_up() {
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        for _ in 0..9 {
            assert!(!r.step((700.0, 949.0), 0.0, -10.0, fit).free);
        }
        // Move back into the content: that drains the accumulator.
        assert!(r.step((700.0, 949.0), 0.0, 40.0, fit).free);
        // So the next nudge starts from zero rather than tipping over the edge.
        assert!(!r.step((700.0, 949.0), 0.0, -10.0, fit).free);
    }

    #[test]
    fn breaking_out_re_arms_only_after_returning_inside() {
        let mut r = EdgeResist::new(20.0);
        let fit = screen_fit();
        assert!(
            r.step((700.0, 949.0), 0.0, -25.0, fit).free,
            "one big shove is enough"
        );
        // Well outside, still free.
        assert!(r.step((700.0, 980.0), 0.0, -5.0, fit).free);
        // Back inside re-arms: the very next outward nudge is resisted again.
        assert!(r.step((700.0, 900.0), 0.0, 0.0, fit).free);
        assert!(!r.step((700.0, 949.0), 0.0, -5.0, fit).free);
    }

    #[test]
    fn resistance_also_holds_the_side_edges_for_multi_display_escapes() {
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        let step = r.step((1512.0, 400.0), 15.0, 0.0, fit);
        assert!(
            !step.free,
            "drifting right must not spill onto the next display"
        );
        assert_eq!(step.pos, (1512.0, 400.0));
        assert_eq!(step.overflow, (15.0, 0.0));
    }

    #[test]
    fn a_zero_threshold_disables_resistance_entirely() {
        let mut r = EdgeResist::new(0.0);
        assert!(!r.enabled());
        let step = r.step((700.0, 949.0), 0.0, -10.0, screen_fit());
        assert!(step.free);
        assert_eq!(step.pos, (700.0, 959.0), "the pointer leaves immediately");
    }

    #[test]
    fn reset_drops_a_half_earned_breakthrough() {
        let mut r = EdgeResist::new(100.0);
        let fit = screen_fit();
        for _ in 0..9 {
            r.step((700.0, 949.0), 0.0, -10.0, fit);
        }
        r.reset(); // e.g. left fullscreen, or the window lost key
        assert!(!r.step((700.0, 949.0), 0.0, -10.0, fit).free);
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
