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
