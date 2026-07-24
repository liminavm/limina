// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Pure letterbox geometry: aspect-fit the guest scanout into the window's content
//! view, plus the inverse mapping the absolute-pointer path needs. No AppKit types —
//! the math unit-tests headless, and the window/input code shares one `FitRect` per
//! tick (via an `Rc<Cell<_>>`) so the pixels and the pointer can never disagree.

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
}
