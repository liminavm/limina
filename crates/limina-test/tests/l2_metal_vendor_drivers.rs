// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina's vendor GPU drivers take a guest's desktop on every virtio-gpu, driving it through
//! KosmicKrisp when the host offers the transport and through virgl and venus when it does not.
//!
//! The drivers are installed beside the system's Mesa and nothing in the environment selects
//! them: every entry point asks the device what it is (docs/design/metal-transport-delivery.md
//! §1). Both answers matter. A driver that never accepts the transport is dead weight; one that
//! takes the transport's path on a host without it breaks a guest that merely moved, which the
//! two-tier guarantee forbids. So the same installed image boots twice, and each boot must land
//! wholly on one side:
//!
//! - **With the transport:** gnome-shell on our EGL vendor and GBM backend, GL clients on zink
//!   over KosmicKrisp in the guest, and KosmicKrisp enumerating a Vulkan device.
//! - **Without it:** gnome-shell on the same EGL vendor and GBM backend, GL clients on our virgl,
//!   Vulkan's first device our venus, KosmicKrisp enumerating nothing, and the renderer serving
//!   the boot without refusing any work.
//!
//! Both must also put a real desktop on screen with its colours the right way round. The F44
//! default wallpaper's lower part is blue; a scanout whose red and blue channels are swapped reads
//! it orange, which is what an unconverted BGRA surface looked like.
//!
//! What each oracle reads, and why the look-alikes on the other side do not pass it, is in
//! [`limina_test::metal`].

use std::time::Duration;

use limina_test::metal::Stack;
use limina_test::{CapturedFrame, Guest, GuestConfig};

/// A seated desktop shows hundreds of distinct colours; a black, flat or half-drawn frame shows
/// a handful.
const MIN_COLORS: usize = 200;

/// The mean red and blue of the frame's lower third.
fn lower_third_red_blue(frame: &CapturedFrame) -> (f64, f64) {
    let row = frame.width as usize * 4;
    let start = row * (frame.height as usize * 2 / 3);
    let (mut r, mut b, mut n) = (0u64, 0u64, 0u64);
    for px in frame.rgba[start..].as_chunks::<4>().0 {
        r += px[0] as u64;
        b += px[2] as u64;
        n += 1;
    }
    (r as f64 / n as f64, b as f64 / n as f64)
}

/// Boot `cfg`, wait for the seated desktop, and read what it runs on and what it shows.
fn boot_and_probe(test: &str, cfg: GuestConfig) -> Option<(Guest, Stack, CapturedFrame)> {
    if !limina_test::require_hvf_or_skip(test) {
        return None;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!("SKIPPED {test}: no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk");
        return None;
    }
    let cfg = cfg
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log();
    let mut g = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = g
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable through the forward");
    eprintln!("guest SSH up: {banner}");
    g.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .unwrap_or_else(|e| panic!("gnome-shell never appeared: {e}"));
    // The shell has drawn its first frames by the time its session bus answers; give the
    // startup animation time to finish so the capture is a settled desktop.
    std::thread::sleep(Duration::from_secs(15));
    let stack = Stack::probe(&g).expect("probing the guest's GPU stack");
    eprintln!("{stack:#?}");
    let frame = g.read_capture().expect("no scanout capture");
    let colors = limina_test::landmarks::color_diversity(&frame);
    let (r, b) = lower_third_red_blue(&frame);
    eprintln!(
        "capture {}x{}: {colors} colours; lower third red {r:.0} blue {b:.0}",
        frame.width, frame.height
    );
    assert!(
        stack.module_is_ours(),
        "virtio_gpu is not DKMS's extended module — deliver the vendor drivers with \
         scripts/provision/deliver-metal-transport.sh: {stack:#?}"
    );
    assert!(
        colors >= MIN_COLORS,
        "the desktop shows only {colors} distinct colours — not a seated frame"
    );
    assert!(
        b > r,
        "the wallpaper's blue reads orange (lower third red {r:.0}, blue {b:.0}): the scanout's \
         red and blue are swapped"
    );
    Some((g, stack, frame))
}

#[test]
fn vendor_drivers_take_the_desktop_on_the_transport() {
    const TEST: &str = "vendor_drivers_take_the_desktop_on_the_transport";
    let cfg = match GuestConfig::metal_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {TEST}: {e}");
            return;
        }
    };
    let Some((g, stack, _)) = boot_and_probe(TEST, cfg) else {
        return;
    };
    assert!(
        stack.extension,
        "the module did not find the limina extension (is LIMINA_GPU_METAL reaching the worker?)"
    );
    assert!(
        stack.shell_on_vendor_drivers(),
        "gnome-shell is not on the vendor EGL and GBM drivers: {:#?}",
        stack.shell_maps
    );
    assert!(
        stack.gl_on_vendor_driver(),
        "a GL client is not on zink over KosmicKrisp in the guest: {:?}",
        stack.gl_renderer
    );
    assert!(
        stack.vk_has_kosmickrisp(),
        "KosmicKrisp enumerates no device on the transport: {:?}",
        stack.vk_drivers
    );
    let outcome = g.shutdown(Duration::from_secs(30)).expect("shutting down");
    eprintln!("teardown outcome: {outcome:?}");
}

#[test]
fn vendor_drivers_serve_virgl_and_venus_without_the_transport() {
    const TEST: &str = "vendor_drivers_serve_virgl_and_venus_without_the_transport";
    let cfg = match GuestConfig::metal_image_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {TEST}: {e}");
            return;
        }
    };
    let Some((g, stack, _)) = boot_and_probe(TEST, cfg) else {
        return;
    };
    assert!(
        !stack.extension,
        "the module found the limina extension on a host that does not offer it"
    );
    assert!(
        stack.shell_on_vendor_drivers(),
        "gnome-shell is not on the vendor EGL and GBM drivers: {:#?}",
        stack.shell_maps
    );
    assert!(
        stack.gl_on_virgl(),
        "a GL client is not on virgl: {:?}",
        stack.gl_renderer
    );
    assert!(
        stack.vk_first_is_vendor_venus(),
        "Vulkan's first device is not the vendor drivers' venus: {:?} {:?}",
        stack.vk_drivers,
        stack.vk_driver_infos
    );
    assert!(
        !stack.vk_has_kosmickrisp(),
        "KosmicKrisp enumerates a device without the transport: {:?}",
        stack.vk_drivers
    );
    limina_test::renderer::assert_renderer_served(&g.supervisor_log(), "on the virgl desktop");
    let outcome = g.shutdown(Duration::from_secs(30)).expect("shutting down");
    eprintln!("teardown outcome: {outcome:?}");
}
