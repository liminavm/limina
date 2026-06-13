// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Enhanced-tier regression: venus survives a virtio-gpu **device reset**.
//!
//! `virgl_renderer_init` is a process-global, init-once, thread-bound singleton (a successful
//! init leaves a static `INIT_ONCE` true forever; `VirglRenderer::drop` runs
//! `virgl_renderer_cleanup` but never clears it). When the guest re-initializes virtio-gpu —
//! the EFI→kernel firmware hand-off, a driver unbind/rebind, etc. — the device's `reset()` +
//! `activate()` used to drop and rebuild the renderer, so the *second* init returned
//! `AlreadyInUse` and the GPU silently degraded to software-2D (venus gone → llvmpipe). That
//! degrade is exactly what blocks the GOP firmware boot console and the EFI-booted enhanced
//! tier. The fix makes the renderer persist across a device reset.
//!
//! This test reproduces the reset via a guest-driven virtio-gpu **driver unbind/rebind** —
//! the same `reset()`→`activate()` host path as the firmware hand-off, but cheaply, without
//! needing GOP firmware or a BLS-bootable 16 KiB kernel. It boots the enhanced 16 KiB tier to
//! `multi-user.target` (no compositor pinning `card0`), confirms venus enumerates, unbinds and
//! rebinds the virtio-gpu device, and asserts venus **still** enumerates afterwards.
//!
//! Prereqs: the 16 KiB kernel (`scripts/build-test-kernel.sh PAGESIZE=16k`) and the Fedora
//! image (stock mesa venus ICD). SKIPs cleanly if either is missing. Gated behind
//! LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`. Heavy (full Fedora boot on a custom kernel).

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

/// The stock Fedora mesa venus ICD path (same as `venus.rs`). Forcing it avoids picking up a
/// dev-image override and keeps the check on the two-tier-baseline driver.
/// `|| true`: when venus is absent vulkaninfo exits non-zero, but its printed device list is
/// the oracle (per CLAUDE.md, exit codes lie) — the `Virtio-GPU Venus` substring check decides.
const VK_PROBE: &str = "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
                        vulkaninfo --summary 2>&1 || true";

#[test]
fn venus_survives_virtio_gpu_reset() {
    if !limina_test::require_hvf_or_skip("venus_survives_virtio_gpu_reset") {
        return;
    }

    // SKIP (not fail) without the enhanced-tier artifacts.
    let cfg = match GuestConfig::enhanced_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            // Boot straight to multi-user so no gnome-shell holds the GPU when we unbind it
            // (and so we never start the plain image's unstable gnome-shell-on-venus session).
            .with_cmdline_extra("systemd.unit=multi-user.target"),
        Err(e) => {
            eprintln!("SKIPPED venus_survives_virtio_gpu_reset: {e}");
            return;
        }
    };
    eprintln!(
        "booting enhanced 16 KiB tier (coexist venus, multi-user) via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    assert_eq!(
        guest
            .ssh_exec("getconf PAGE_SIZE")
            .expect("page size")
            .trim(),
        "16384",
        "expected a 16 KiB-page guest (4 KiB can't map venus blobs)"
    );

    // Baseline: venus enumerates before any reset.
    let vk_before = guest.ssh_exec(VK_PROBE).expect("vulkaninfo (baseline)");
    eprintln!("--- vulkaninfo (baseline) ---\n{vk_before}");
    assert!(
        vk_before.contains("Virtio-GPU Venus"),
        "venus did not enumerate at baseline; the test can't prove the reset path.\n{vk_before}"
    );

    // Discover the virtio-gpu device id (e.g. `virtio3`) — it's the lone `virtio*` entry in the
    // driver dir. Don't hardcode it; the bus index depends on device ordering.
    let dev = guest
        .ssh_exec("basename \"$(ls -d /sys/bus/virtio/drivers/virtio_gpu/virtio* | head -1)\"")
        .expect("finding the virtio-gpu device")
        .trim()
        .to_string();
    assert!(
        dev.starts_with("virtio"),
        "unexpected virtio-gpu device id: {dev:?}"
    );
    eprintln!("virtio-gpu device = {dev}");

    // Force a device reset()+activate() the way the firmware→kernel hand-off does: unbind then
    // rebind the driver. `> sysfs` must run as a root shell (sudo -n; the redirection target is
    // root-owned). Both writes exit 0 even when the rebind later degrades the renderer.
    guest
        .ssh_exec(&format!(
            "sudo -n sh -c 'echo {dev} > /sys/bus/virtio/drivers/virtio_gpu/unbind'"
        ))
        .expect("unbinding virtio-gpu");
    guest
        .ssh_exec(&format!(
            "sudo -n sh -c 'echo {dev} > /sys/bus/virtio/drivers/virtio_gpu/bind'"
        ))
        .expect("rebinding virtio-gpu");

    // Wait for the render node to come back (in the degraded case the kernel spends ~6s timing
    // out on cap set 0 before it reappears, so poll rather than a fixed sleep).
    guest
        .ssh_poll("test -e /dev/dri/renderD128", Duration::from_secs(30))
        .expect("renderD128 never reappeared after rebind");

    // The decisive check: venus is STILL the renderer after the reset (not degraded to llvmpipe).
    let vk_after = guest.ssh_exec(VK_PROBE).expect("vulkaninfo (after reset)");
    eprintln!("--- vulkaninfo (after unbind/rebind) ---\n{vk_after}");
    assert!(
        vk_after.contains("Virtio-GPU Venus"),
        "venus did NOT survive the virtio-gpu reset — degraded to software-2D/llvmpipe \
         (the renderer-singleton bug; check the worker log for `AlreadyInUse`).\n{vk_after}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
