// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Landmark comparison: does the guest's desktop still LOOK the way it looked?
//!
//! A restore can leave every process alive, every host-side counter green and every content
//! floor satisfied while the desktop on screen is wrong — windows displaced by a monitor the
//! guest re-probed onto, or surfaces whose pixels did not survive. The only oracle that sees
//! that is the frame itself.
//!
//! The comparison dices the frame into a grid and compares per-cell mean colour. A cell is a
//! big enough sample to average away sub-pixel text rendering and dithering, and small enough
//! that a window moving by any visible amount changes many of them at once. It needs no golden
//! image: the reference is the guest's own earlier frame.
//!
//! **This only works against a workload that holds still.** Anything animating has to be
//! excluded from the comparison, and an excluded region is a region nothing checks — which is
//! how the most interesting pixels on the screen (a client's GL canvas, a Vulkan surface)
//! become the blind spots. Tests using this are expected to drive still content on purpose;
//! [`settled_capture`] is what makes "still" observable rather than assumed.

use std::time::{Duration, Instant};

use crate::{CapturedFrame, Guest};

/// Grid resolution. Fine enough that a displaced window moves many cells, coarse enough that a
/// cell averages away sub-pixel text rendering jitter.
pub const GRID_COLS: u32 = 40;
pub const GRID_ROWS: u32 = 25;

/// Total cells in the grid.
pub const GRID_CELLS: usize = (GRID_COLS * GRID_ROWS) as usize;

/// Per-channel distance under which two cell means count as the same colour. Absorbs dithering
/// and the compositor's own frame-to-frame noise; far below the change a moved window or a
/// blanked surface makes.
pub const CELL_TOL: i32 = 10;

/// Share of cells that must agree between consecutive reads for a frame to count as settled.
/// Not 100%: a panel clock ticks on its own schedule, and holding out for it would never settle
/// on a desktop that is otherwise motionless.
const SETTLE_AGREE_PCT: usize = 99;

/// Mean RGB of every grid cell, row-major.
pub fn cell_means(frame: &CapturedFrame) -> Vec<[i32; 3]> {
    let mut sums = vec![[0i64; 4]; GRID_CELLS];
    for y in (0..frame.height).step_by(2) {
        let cy = (y * GRID_ROWS / frame.height).min(GRID_ROWS - 1);
        for x in (0..frame.width).step_by(2) {
            let cx = (x * GRID_COLS / frame.width).min(GRID_COLS - 1);
            let px = frame.pixel(x, y);
            let c = &mut sums[(cy * GRID_COLS + cx) as usize];
            c[0] += px[0] as i64;
            c[1] += px[1] as i64;
            c[2] += px[2] as i64;
            c[3] += 1;
        }
    }
    sums.into_iter()
        .map(|c| {
            let n = c[3].max(1);
            [(c[0] / n) as i32, (c[1] / n) as i32, (c[2] / n) as i32]
        })
        .collect()
}

/// Largest per-channel difference between two cell means.
pub fn cell_delta(a: [i32; 3], b: [i32; 3]) -> i32 {
    (a[0] - b[0])
        .abs()
        .max((a[1] - b[1]).abs())
        .max((a[2] - b[2]).abs())
}

/// Indices where two frames' cells differ by more than [`CELL_TOL`].
pub fn moved_cells(a: &[[i32; 3]], b: &[[i32; 3]]) -> Vec<usize> {
    if a.len() != b.len() {
        return (0..a.len()).collect();
    }
    (0..a.len())
        .filter(|&i| cell_delta(a[i], b[i]) > CELL_TOL)
        .collect()
}

/// Count distinct quantized colours (4 bits/channel, every 4th pixel) — the content-loss floor.
/// A desktop whose textures came back flat collapses this by orders of magnitude.
pub fn color_diversity(frame: &CapturedFrame) -> usize {
    let mut seen = std::collections::HashSet::new();
    for px in frame.rgba.as_chunks::<4>().0.iter().step_by(4) {
        seen.insert(((px[0] as u16 >> 4) << 8) | ((px[1] as u16 >> 4) << 4) | (px[2] as u16 >> 4));
    }
    seen.len()
}

/// Group cell indices by grid row — a displaced window clusters, a blanked desktop spreads
/// everywhere, and the row histogram tells those apart at a glance in a failure message.
pub fn by_row(cells: &[usize]) -> std::collections::BTreeMap<u32, usize> {
    let mut rows = std::collections::BTreeMap::new();
    for i in cells {
        *rows.entry(*i as u32 / GRID_COLS).or_insert(0) += 1;
    }
    rows
}

/// Read the capture once its CONTENT has stopped changing: return the first frame that agrees
/// with its predecessor almost everywhere.
///
/// The obvious version of this waits for the guest to stop *presenting*, and that is wrong — a
/// client can hold a fixed picture while still submitting a frame every vblank (vkmark's
/// `clear` does exactly that), so a present-cadence test never settles on a desktop that is as
/// still as it will ever be. What the comparison needs is stable pixels, so that is what this
/// waits for.
pub fn settled_capture(guest: &Guest, timeout: Duration) -> anyhow::Result<CapturedFrame> {
    let deadline = Instant::now() + timeout;
    let mut prev: Option<Vec<[i32; 3]>> = None;
    loop {
        if let Ok(frame) = guest.read_capture() {
            let means = cell_means(&frame);
            if let Some(pmeans) = &prev {
                let agree = GRID_CELLS - moved_cells(&means, pmeans).len();
                if agree * 100 >= GRID_CELLS * SETTLE_AGREE_PCT {
                    return Ok(frame);
                }
            }
            prev = Some(means);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the captured frame never held still ({SETTLE_AGREE_PCT}% of cells agreeing \
                 with the previous read) within {timeout:?} — either nothing is being \
                 presented, or something on this desktop is animating and a landmark \
                 comparison against it would be meaningless"
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
