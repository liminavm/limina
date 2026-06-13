// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Enhanced-tier 3D test: venus works on our custom 16 KiB-page kernel.
//!
//! Drives the real `limina` supervisor to direct-boot the in-repo Fedora image's btrfs root
//! with our **16 KiB-page** kernel (`Image-16k`), the coexist (venus) GPU, and user-mode NAT.
//! Then it SSHes in and asks `vulkaninfo`: a 16 KiB guest places host-visible virtio-gpu
//! blobs on 16 KiB boundaries, so `hv_vm_map` accepts them and venus enumerates the real
//! host GPU — whereas the stock 4 KiB Fedora kernel can't map those blobs and falls back to
//! llvmpipe (see memory `limina-tier2-venus`, roadmap M4).
//!
//! Prereqs: the 16 KiB kernel (`scripts/build-test-kernel.sh PAGESIZE=16k`) and the Fedora
//! image. The test SKIPs cleanly if either is missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`. This is a heavy test (full Fedora desktop boot on a custom kernel).

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

#[test]
fn venus_enumerates_on_16k_kernel() {
    if !limina_test::require_hvf_or_skip("venus_enumerates_on_16k_kernel") {
        return;
    }

    // venus's one supported host backend is KosmicKrisp; without it Guest::boot degrades the
    // coexist display to software-2D (never MoltenVK), on which venus can't enumerate — so SKIP
    // up front rather than fail the enumeration assert below.
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED venus_enumerates_on_16k_kernel: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }

    // SKIP (not fail) if the 16 KiB kernel or the Fedora disk isn't present — this is an
    // enhanced-tier test that needs an artifact built via `scripts/build-test-kernel.sh`.
    let cfg = match GuestConfig::enhanced_fedora_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED venus_enumerates_on_16k_kernel: {e}");
            return;
        }
    };
    eprintln!(
        "booting Fedora on the custom 16 KiB kernel (coexist venus + NAT) via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    // Full Fedora userspace boot on a custom kernel (no initramfs → systemd → NM → sshd)
    // takes a while; the inbound forward only yields a banner once sshd is reachable.
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // Prove it's actually our 16 KiB kernel (the whole point — 4 KiB would not map venus blobs).
    let pagesize = guest
        .ssh_exec("getconf PAGE_SIZE")
        .expect("reading guest page size");
    assert_eq!(
        pagesize.trim(),
        "16384",
        "expected a 16 KiB-page guest, got PAGE_SIZE={pagesize:?}"
    );

    // The decisive check: venus enumerates the real host GPU (not llvmpipe). vulkaninfo
    // creating an instance+device on venus is exactly the host-visible-blob map path that
    // fails on a 4 KiB guest.
    let vk = guest
        .ssh_exec(
            "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
             vulkaninfo --summary 2>&1",
        )
        .expect("running vulkaninfo in the guest");
    eprintln!("--- vulkaninfo --summary ---\n{vk}");
    assert!(
        vk.contains("Virtio-GPU Venus"),
        "venus did not enumerate (expected a 'Virtio-GPU Venus' device).\n{vk}"
    );
    assert!(
        vk.contains("driverName") && vk.contains("venus"),
        "expected driverName = venus in vulkaninfo output.\n{vk}"
    );

    // Clean teardown: the guest honors no GPIO power button (stock-ish userspace), so the
    // supervisor force-kills after the grace — expected, still a clean stop.
    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
