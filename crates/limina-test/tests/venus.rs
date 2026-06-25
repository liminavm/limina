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

use std::time::{Duration, Instant};

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

/// The **productized** enhanced tier: our *shipped* mesa (the versionlocked `*.limina` RPM — mesa 26.2
/// zink+venus at `/usr`) on the enhanced golden (`enhanced.test.raw`) must enumerate venus AND bring up
/// a live seated GNOME session on it. Where [`venus_enumerates_on_16k_kernel`] proves *stock* mesa's
/// venus on an external 16k kernel, and `venus_replay` proves trace-replay render correctness (guest-side
/// pixels), this validates the actual shipped product: our RPM mesa driving a real autologin session
/// through zink→venus→KosmicKrisp→Metal. Heavy (full enhanced desktop boot); SKIPs without KK or the
/// enhanced disk.
#[test]
fn our_mesa_venus_renders_seated_desktop() {
    if !limina_test::require_hvf_or_skip("our_mesa_venus_renders_seated_desktop") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED our_mesa_venus_renders_seated_desktop: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }

    // The seated enhanced golden (enhanced.test.raw) + coexist venus display + NAT. Guest::boot
    // wires KosmicKrisp automatically for the coexist display.
    let cfg = match GuestConfig::seated_fedora_from_env() {
        // LIMINA_PRESENT_COPY=1 mirrors the product launcher's default (boot-seated-efi.sh) and
        // venus_replay, so we boot the seated desktop the way limina actually ships it. (It does NOT
        // silence the harness-only `present_surface -2` host-present gap — that persists either way and
        // is non-fatal background noise; this test deliberately asserts no HOST present, see below.)
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_env("LIMINA_PRESENT_COPY", "1"),
        Err(e) => {
            eprintln!("SKIPPED our_mesa_venus_renders_seated_desktop: {e}");
            return;
        }
    };
    eprintln!(
        "booting the enhanced (our-mesa) seated venus desktop via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // 16 KiB pages (the venus prerequisite) ...
    let pagesize = guest
        .ssh_exec("getconf PAGE_SIZE")
        .expect("reading guest page size");
    assert_eq!(
        pagesize.trim(),
        "16384",
        "expected a 16 KiB-page guest, got PAGE_SIZE={pagesize:?}"
    );

    // ... and it is OUR mesa (the versionlocked `*.limina` RPM set), not stock — the distinction
    // from the stock-mesa `venus_enumerates_on_16k_kernel`.
    let mesa = guest
        .ssh_exec("rpm -qa 'mesa*' 2>&1")
        .expect("querying the guest mesa RPMs");
    eprintln!("--- guest mesa RPMs ---\n{mesa}");
    assert!(
        mesa.contains("limina"),
        "expected our '*.limina' mesa RPM set (is this really the enhanced image?).\n{mesa}"
    );

    // ... and our mesa ships the **venus (virtio) Vulkan ICD** — the driver zink loads to reach the
    // host GPU. We assert the ICD is installed rather than run `vulkaninfo`: vulkan-tools isn't on the
    // image, and full venus enumeration (`GL_RENDERER = Virtio-GPU Venus`) + pixel-exact render on this
    // SAME image are already proven by `venus_replay` — re-running them here would only duplicate it.
    // This test's distinct job is the productization IDENTITY guard: our 16k kernel + our `*.limina`
    // mesa with venus shipped, booting a real seated session.
    let icd = guest
        .ssh_exec(
            "test -f /usr/share/vulkan/icd.d/virtio_icd.aarch64.json && echo PRESENT || echo MISSING",
        )
        .expect("checking for the venus virtio ICD");
    assert!(
        icd.contains("PRESENT"),
        "our mesa did not install the venus virtio ICD at \
         /usr/share/vulkan/icd.d/virtio_icd.aarch64.json — got {icd:?}"
    );

    // The seated GNOME session actually came up on the enhanced stack (autologin → gnome-shell), i.e.
    // our 16k kernel + our mesa bring up a real desktop. We poll for the running gnome-shell PROCESS
    // (what venus_replay implicitly relies on via the session's Xwayland auth) rather than grep the
    // journal for a message string — robust across the system/user journal split. Pixel-level render
    // correctness THROUGH venus is `venus_replay`'s job; the HOST present path (read_capture) is
    // intentionally NOT asserted here.
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect(
            "gnome-shell process never appeared — the productized seated session didn't come up",
        );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}

/// RED reproducer for the **`present_surface -2` host-present gap**. Pixel-verify a VENUS desktop
/// through the HOST capture sink: boot the seated venus desktop and read the *presented* frame back
/// via [`Guest::read_capture`].
///
/// This currently FAILS by design. Presenting a venus (Metal-backed IOSurface) scanout into the
/// harness's `--display-capture` sink returns `present_surface … -2 ("Backend implementation
/// error")`, so no frame reaches the capture PNG and the read-back stays blank/stale. A software-2D
/// scanout (`fedora_stock_image_..._renders_desktop`) and the windowed app both present fine — the
/// gap is specifically (venus blob × capture sink). When the host present path is fixed this asserts
/// a real rendered venus desktop and becomes the regression guard; [`our_mesa_venus_renders_seated_desktop`]
/// covers the same boot but deliberately stops at session-up because of this gap.
#[test]
fn venus_desktop_pixel_verifies_through_host_capture() {
    if !limina_test::require_hvf_or_skip("venus_desktop_pixel_verifies_through_host_capture") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED venus_desktop_pixel_verifies_through_host_capture: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }

    let cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_env("LIMINA_PRESENT_COPY", "1"),
        Err(e) => {
            eprintln!("SKIPPED venus_desktop_pixel_verifies_through_host_capture: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    // The seated session must be up and actively presenting before we read the host capture.
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell process never appeared — the seated session didn't come up");

    // Read the presented venus frame back through the host `--display-capture` sink.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut best_colors = 0usize;
    let mut best_dominance = 1.0f64;
    while Instant::now() < deadline {
        if let Ok(frame) = guest.read_capture() {
            let colors = frame.distinct_colors();
            let (_, dominance) = frame.dominant_color();
            if colors > best_colors {
                best_colors = colors;
                best_dominance = dominance;
            }
            if best_colors >= 1000 && best_dominance < 0.90 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!(
        "richest venus frame via host capture: {best_colors} distinct colors, dominant {best_dominance:.2}"
    );
    assert!(
        best_colors >= 1000 && best_dominance < 0.90,
        "the venus desktop never presented a rich frame through the host capture sink (richest: \
         {best_colors} colors, {best_dominance:.2} dominant) — the present_surface -2 gap"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
