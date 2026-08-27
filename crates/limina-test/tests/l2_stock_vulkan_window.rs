// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 guard: a **stock** guest's Vulkan client must composite its own pixels, not black.
//!
//! A stock distro's virgl gallium driver implements `is_dmabuf_modifier_supported` but not
//! `query_dmabuf_modifiers`, so its compositor can only advertise `DRM_FORMAT_MOD_INVALID`
//! and a Vulkan client falls onto mesa WSI's **prime-blit** path: it renders into a private
//! image and blits into a linear staging **VkBuffer**, and it is that buffer's memory the
//! compositor imports as the `wl_buffer`. Our enhanced guest mesa never gets here — patch
//! `patches/mesa-guest/0001` rewrites INVALID to LINEAR so the client takes the native
//! single-memory path straight onto an IOSurface.
//!
//! Host-side that buffer memory is not bound to any image, so it has no IOSurface, and vrend's
//! `set_type` used to hand the compositor a zeroed placeholder texture: every Vulkan window
//! composited **solid black** while the rest of the desktop was fine. That breaks the two-tier
//! guarantee — the stock tier is allowed to be slower, not blank. The fix backs such an
//! export's memory with its own shm carrier and uploads those bytes into the compositor's
//! texture (a CPU copy; the enhanced tier keeps the zero-copy IOSurface path).
//!
//! Oracle: real pixels, differentially, so no wallpaper or window-placement assumption is
//! baked in. Capture the settled desktop, launch `vkcube`, capture again, and look only at
//! the pixels that CHANGED. RED = a large changed region that is overwhelmingly pure black
//! (the placeholder). GREEN = a large changed region that is almost never pure black —
//! measured on a real enhanced-tier capture, a rendering `vkcube` produces **zero** pure-black
//! pixels, against 490k on the stock capture that motivated this test
//! (`spikes/stock-venus-black-windows/`).
//!
//! Stock disk + our GOP firmware + a coexist (venus) display. SKIPs cleanly without the
//! KosmicKrisp ICD or the image. Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{CapturedFrame, Guest, GuestConfig};

/// Pixels that differ between two frames, and how many of those became pure black.
fn changed_and_black(before: &CapturedFrame, after: &CapturedFrame) -> (usize, usize) {
    if before.width != after.width || before.height != after.height {
        return (0, 0);
    }
    let mut changed = 0usize;
    let mut black = 0usize;
    for (b, a) in before
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after.rgba.as_chunks::<4>().0.iter())
    {
        if b != a {
            changed += 1;
            if a[0] == 0 && a[1] == 0 && a[2] == 0 {
                black += 1;
            }
        }
    }
    (changed, black)
}

#[test]
fn stock_guest_vulkan_client_composites_its_own_pixels() {
    if !limina_test::require_hvf_or_skip("stock_guest_vulkan_client_composites_its_own_pixels") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED stock_guest_vulkan_client_composites_its_own_pixels: no KosmicKrisp ICD \
             under /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    // The STOCK image is the whole point: no limina guest components, so no WSI modifier
    // patch, so the client takes the prime-blit path this guard exists for.
    let cfg = match GuestConfig::fedora_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED stock_guest_vulkan_client_composites_its_own_pixels: {e:#}");
            return;
        }
    };
    eprintln!(
        "booting the STOCK Fedora desktop with a coexist venus display: {:?}",
        cfg.boot
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest never reached sshd over the EFI path");

    guest
        .ssh_poll(
            "sudo journalctl -b _COMM=gnome-shell --no-pager 2>/dev/null \
             | grep -q 'GNOME Shell started'",
            Duration::from_secs(240),
        )
        .expect("gnome-shell never started on the stock guest");

    // Let the desktop settle, so the baseline is a painted desktop rather than a fade-in.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut baseline = None;
    while Instant::now() < deadline {
        if let Ok(frame) = guest.read_capture() {
            let (_, dominance) = frame.dominant_color();
            if frame.distinct_colors() >= 1000 && dominance < 0.90 {
                baseline = Some(frame);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let baseline = baseline.expect("the stock GNOME desktop never painted a settled frame");

    let session = "env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0";
    guest
        .ssh_exec(&format!(
            "{session} sh -c 'command -v vkcube' >/dev/null || \
             {{ echo NOVKCUBE; exit 1; }}"
        ))
        .expect("vkcube is not installed on the stock image — the vehicle is missing");
    guest
        .ssh_exec(&format!(
            "{session} nohup vkcube >/tmp/vkcube.log 2>&1 & sleep 1; echo started"
        ))
        .expect("could not launch vkcube in the guest session");

    // The window has to map, be composited, and reach a captured frame.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut best: Option<(usize, usize)> = None;
    while Instant::now() < deadline {
        if let Ok(after) = guest.read_capture() {
            let (changed, black) = changed_and_black(&baseline, &after);
            // Only judge once the window is actually on screen: vkcube's default surface is
            // 500x500 = 250k pixels, so a real map changes far more than desktop noise.
            if changed >= 100_000 {
                let share = black as f64 / changed as f64;
                if best.is_none_or(|(_, b)| black < b) {
                    best = Some((changed, black));
                }
                if share < 0.10 {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let (changed, black) = best.expect(
        "vkcube's window never reached the captured scanout on the stock guest — the vehicle \
         failed before the oracle could judge it",
    );
    let share = black as f64 / changed as f64;
    eprintln!(
        "vkcube changed {changed} pixels, {black} of them pure black ({:.1}%)",
        share * 100.0
    );
    assert!(
        share < 0.10,
        "the stock guest's Vulkan window composited BLACK: {black} of {changed} changed pixels \
         ({:.1}%) are pure black. The compositor imported a placeholder texture instead of the \
         client's blit staging buffer — see spikes/stock-venus-black-windows/",
        share * 100.0
    );

    let outcome = guest
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
