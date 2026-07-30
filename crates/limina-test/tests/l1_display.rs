// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1 display test: boot the tiny guest with a virtio-gpu display and assert on a real
//! captured frame.
//!
//! This is the headless display oracle for M2 (Tier 1, the software-2D scanout path). The
//! guest kernel brings up virtio-gpu DRM + fbdev (see scripts/build-test-kernel.sh) and
//! the L1 init draws a deterministic pattern — top half red, bottom half blue — to
//! `/dev/fb0`, then forces a flush. libkrun's limina software-2D patch presents it (stock
//! libkrun routes 2D through virgl GL, which is dead on macOS); our capture backend writes
//! the presented frame to a PNG. Here we decode it and assert dimensions, byte order, and
//! the exact pattern — a true end-to-end pixel oracle for the whole display pipeline.
//!
//! Build the guest first: `scripts/build-test-guest.sh` (needs the DRM kernel + the init
//! that draws). Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{CapturedFrame, Guest, GuestConfig};

const WIDTH: u32 = 1024;
const HEIGHT: u32 = 768;

#[test]
fn l1_guest_presents_a_nonblank_frame() {
    if !limina_test::require_hvf_or_skip("l1_guest_presents_a_nonblank_frame") {
        return;
    }

    let cfg = GuestConfig::l1_from_env()
        .expect("resolving L1 guest config")
        .with_display(WIDTH, HEIGHT);
    eprintln!(
        "booting L1 guest with display via {:?}: {:?}",
        cfg.limina_bin, cfg.boot
    );

    let guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // The guest presents a black modeset frame first, then draws the pattern and flushes,
    // then powers off (~0.5s total). The capture file always holds the latest present, so
    // poll until the pattern (a frame with >1 color) appears — before Drop removes it.
    let frame = wait_for_pattern(&guest, Duration::from_secs(15))
        .expect("no patterned scanout frame was captured (virtio-gpu / software-2D / backend)");
    eprintln!(
        "captured frame: {}x{}, {} distinct colors, dominant {:?}",
        frame.width,
        frame.height,
        frame.distinct_colors(),
        frame.dominant_color(),
    );

    // The scanout matches the advertised display mode.
    assert_eq!(
        (frame.width, frame.height),
        (WIDTH, HEIGHT),
        "captured scanout dimensions don't match the advertised display mode"
    );

    // The init drew two bands; the frame is not a single flat color.
    assert!(
        frame.distinct_colors() >= 2,
        "captured frame has {} distinct colors — expected the red/blue pattern",
        frame.distinct_colors()
    );

    // Top half red, bottom half blue — proves byte order (BGRX→RGBA), orientation, and
    // the whole guest→host present path end to end.
    let top = frame.pixel(WIDTH / 2, HEIGHT / 4);
    let bottom = frame.pixel(WIDTH / 2, HEIGHT * 3 / 4);
    assert!(
        top[0] > 200 && top[1] < 60 && top[2] < 60,
        "top band should be red, got RGBA {top:?}"
    );
    assert!(
        bottom[2] > 200 && bottom[0] < 60 && bottom[1] < 60,
        "bottom band should be blue, got RGBA {bottom:?}"
    );

    // The guest powers itself off cleanly after drawing — same guarantee as other L1 tests.
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
    assert!(
        !outcome.forced,
        "harness had to force teardown — guest power-off or supervisor is broken"
    );
    assert_eq!(
        outcome.code,
        Some(0),
        "expected a clean guest power-off (worker exit 0), got {outcome:?}"
    );
}

/// Poll the capture file until a frame with more than one color appears (the init's
/// pattern, as opposed to the initial black modeset frame), or `timeout` elapses.
fn wait_for_pattern(guest: &Guest, timeout: Duration) -> Option<CapturedFrame> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(frame) = guest.read_capture() {
            // Wait for the init's ACTUAL red/blue bands, not merely "more than one
            // color": under suite load the capture can catch an earlier boot-console
            // frame (text on black — 187 distinct colors, 99% black dominant), which
            // passed the old >=2-colors predicate and then failed the band asserts
            // (the flake that hit the 2026-07-30 suite runs twice). The guest powers
            // off ~0.5 s after drawing, so the pattern frame is the LAST one — keep
            // polling until it appears.
            if frame.width == WIDTH && frame.height == HEIGHT {
                let top = frame.pixel(WIDTH / 2, HEIGHT / 4);
                let bottom = frame.pixel(WIDTH / 2, HEIGHT * 3 / 4);
                if top[0] > 200 && bottom[2] > 200 {
                    return Some(frame);
                }
            }
        }
        if Instant::now() >= deadline {
            // Return whatever is there so the caller's asserts print the real pixels.
            return guest.read_capture().ok();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
