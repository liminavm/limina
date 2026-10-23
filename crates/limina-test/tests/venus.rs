// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Enhanced-tier 3D tests: venus works on our custom 16 KiB-page kernel.
//!
//! `venus_enumerates_on_16k_kernel` drives the real `limina` supervisor to direct-boot the
//! stock image's btrfs root with our **16 KiB-page** test kernel (`Image-16k`) — the
//! 16k-kernel-on-stock-userspace mix — with the coexist (venus) GPU and user-mode NAT; the two
//! seated tests EFI-boot the enhanced golden on its own installed 16k kernel instead.
//! The first test SSHes in and asks `vulkaninfo`: a 16 KiB guest places host-visible virtio-gpu
//! blobs on 16 KiB boundaries, so `hv_vm_map` accepts them and venus enumerates the real
//! host GPU — whereas the stock 4 KiB Fedora kernel can't map those blobs and falls back to
//! llvmpipe (see memory `limina-tier2-venus`, roadmap M4).
//!
//! Prereqs: the 16 KiB test kernel (`scripts/build-test-kernel.sh PAGESIZE=16k`) and the stock
//! image for the first test; the enhanced golden and the GOP firmware for the seated ones. Each
//! SKIPs cleanly if its prerequisite is missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`. This is a heavy test (full Fedora desktop boot on a custom kernel).

use limina_test::VENUS_ICD;
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
        .wait_for_ssh(Duration::from_secs(180))
        .expect("guest sshd never became reachable through the forward");
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
        .ssh_exec(&format!(
            "VK_DRIVER_FILES={VENUS_ICD} vulkaninfo --summary 2>&1"
        ))
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

/// The **productized** enhanced tier: limina's GPU drivers (under `/opt/limina`, beside the distro's
/// own Mesa) on the enhanced golden (`enhanced.test.raw`) must serve a live seated GNOME session.
/// Where [`venus_enumerates_on_16k_kernel`] proves *stock* mesa's venus on an external 16k kernel,
/// and `venus_replay` proves trace-replay render correctness (guest-side pixels), this validates the
/// shipped product's identity: the 16k kernel, the distro's Mesa left in place, and the session's
/// compositor running on our drivers rather than on it — with our venus and our VA driver where
/// the system's loaders find them. Heavy (full enhanced desktop boot); SKIPs without KK or the
/// enhanced disk.
#[test]
fn our_drivers_serve_seated_desktop() {
    if !limina_test::require_hvf_or_skip("our_drivers_serve_seated_desktop") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED our_drivers_serve_seated_desktop: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }

    // The seated enhanced golden (enhanced.test.raw) + coexist venus display + NAT. Guest::boot
    // wires KosmicKrisp automatically for the coexist display.
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        // LIMINA_PRESENT_COPY=1 mirrors the product launcher's default (boot-seated-efi.sh) and
        // venus_replay, so we boot the seated desktop the way limina actually ships it. (It does NOT
        // silence the harness-only `present_surface -2` host-present gap — that persists either way and
        // is non-fatal background noise; this test deliberately asserts no HOST present, see below.)
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_env("LIMINA_PRESENT_COPY", "1"),
        Err(e) => {
            eprintln!("SKIPPED our_drivers_serve_seated_desktop: {e}");
            return;
        }
    };
    eprintln!(
        "booting the enhanced seated desktop via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable through the forward");
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

    // ... the distro's own Mesa at /usr, untouched: our drivers install beside it ...
    let mesa = guest
        .ssh_exec("rpm -qa 'mesa*' --qf '%{NAME}-%{VERSION}-%{RELEASE}\\n' 2>&1")
        .expect("querying the guest mesa RPMs");
    eprintln!("--- guest mesa RPMs ---\n{mesa}");
    assert!(
        mesa.contains("mesa-") && !mesa.contains("limina"),
        "expected the distro's mesa RPMs, none of ours.\n{mesa}"
    );

    // ... our venus and VA driver where the system's loaders find them ...
    let found = guest
        .ssh_exec(
            "test -e /etc/vulkan/icd.d/limina-venus.json && echo venus-icd; \
             . /etc/environment.d/90-limina-va.conf 2>/dev/null; \
             test -e \"${LIBVA_DRIVERS_PATH%%:*}/virtio_gpu_drv_video.so\" && \
             case $LIBVA_DRIVERS_PATH in /opt/limina/*) echo va-driver ;; esac",
        )
        .expect("checking for our venus ICD and VA driver");
    assert!(
        found.contains("venus-icd") && found.contains("va-driver"),
        "our venus ICD or VA driver is not installed — got {found:?}"
    );

    // ... and the seated session's compositor running on OUR GL: gnome-shell maps the libgallium
    // under /opt/limina, not the distro's. The greeter's shell may come first, so poll until a
    // shell maps ours. Pixel-level render correctness is `venus_replay`'s job; the HOST present
    // path (read_capture) is intentionally NOT asserted here.
    guest
        .ssh_poll(
            "for p in $(pgrep -x gnome-shell); do \
               sudo grep -q '/opt/limina/[0-9]*/lib/libgallium' /proc/$p/maps && exit 0; \
             done; exit 1",
            Duration::from_secs(180),
        )
        .expect("no gnome-shell maps our libgallium — the session is not on limina's drivers");

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
/// a real rendered venus desktop and becomes the regression guard; [`our_drivers_serve_seated_desktop`]
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

    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
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
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable through the forward");
    // The seated session must be up and actively presenting before we read the host capture.
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell process never appeared — the seated session didn't come up");

    // Read the presented venus frame back through the host `--display-capture` sink.
    let frame = guest
        .wait_for_rich_capture(Duration::from_secs(120), 1000, 0.90)
        .unwrap_or_else(|e| panic!("the venus desktop never presented a rich frame through the host capture sink — the \
                 present_surface -2 gap: {e:#}"));
    let (colors, dominance) = frame.richness();
    eprintln!(
        "first rich venus frame via host capture: {colors} distinct colors, dominant {dominance:.2}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
