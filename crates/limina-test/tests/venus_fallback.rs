// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Vulkan graceful degradation on the STOCK tier: lavapipe is the floor, and a
//! venus that cannot work must not wedge the guest.
//!
//! A stock 4 KiB guest on the coexist GPU cannot reliably map venus's host-visible
//! blobs (16 KiB-misaligned window offsets — memory `limina-blob-map-16k-alignment`;
//! fixed only by the guest-side alignment: guest/virtio-gpu-dkms/0001 or the
//! limina-virtio-gpu DKMS module). Mesa's venus then fails vkCreateInstance with
//! VK_ERROR_OUT_OF_HOST_MEMORY, and the Vulkan loader treats OOM as fatal for the
//! whole instance chain — masking a perfectly healthy lavapipe (observed live;
//! the loader only *skips* an ICD for INCOMPATIBLE_DRIVER, cf. dzn).
//! The fix is authored — patches/mesa/0012 makes venus degrade to its stub
//! instance (ships in our enhanced mesa; upstreaming it is the long-term plan,
//! see docs/hardening-backlog.md) — but until Fedora ships it, the DEFAULT
//! loader path on stock may legitimately have no usable device.
//!
//! What this test therefore asserts (the truthful, guarding contract):
//! 1. FLOOR — lavapipe, selected explicitly, fully works: instance + device +
//!    a packed odd-size host-visible allocate/map/readback stress.
//! 2. DEFAULT — the all-ICDs path must complete *structuredly* (a VkResult, not a
//!    hang or crash). If it ever starts succeeding (venus fixed or skipped), the
//!    stress on llvmpipe through the default instance must pass — so this test
//!    automatically tightens as the stack improves.
//! 3. The seated GNOME session survives the probing (venus's failure is contained).
//!
//! Vehicle: the stock 4 KiB F44 autologin baseline on the coexist GPU (same SKIP
//! set as virgl.rs — GL-side degradation lives there). The probe is
//! spikes/venus-4k-dkms/vkprobe.py, pushed over SSH: pure python3 + libvulkan, so
//! a pristine stock image needs no extra packages. Gated behind LIMINA_HVF_TESTS;
//! run via scripts/test-boot.sh.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const VKPROBE: &str = include_str!("../../../spikes/venus-4k-dkms/vkprobe.py");

#[test]
fn stock_vulkan_floor_survives_broken_venus() {
    if !limina_test::require_hvf_or_skip("stock_vulkan_floor_survives_broken_venus") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED stock_vulkan_floor_survives_broken_venus: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk"
        );
        return;
    }
    if limina_test::zink_kk_mesa_prefix().is_none() {
        eprintln!("SKIPPED stock_vulkan_floor_survives_broken_venus: no zink-on-KK Mesa prefix");
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED stock_vulkan_floor_survives_broken_venus: {e}");
            return;
        }
    };
    eprintln!("booting stock 4 KiB F44 (coexist GPU, NAT) for the Vulkan floor probe");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // Stage the dependency-free probe (pure python3 + libvulkan — nothing to install).
    guest
        .ssh_exec(&format!(
            "cat > /tmp/vkprobe.py <<'VKPROBE_PY_EOF'\n{VKPROBE}\nVKPROBE_PY_EOF"
        ))
        .expect("staging vkprobe.py in the guest");

    // 1) THE FLOOR: lavapipe selected explicitly must fully work — instance, device,
    //    and a packed odd-size host-visible map stress (48 allocations).
    let floor = guest
        .ssh_exec(
            "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json \
             timeout 180 python3 /tmp/vkprobe.py llvmpipe 2>&1 || true",
        )
        .expect("ssh to the guest failed");
    eprintln!("floor probe:\n{}", floor.trim());
    assert!(
        floor.contains("INSTANCE OK"),
        "lavapipe alone could not create a Vulkan instance:\n{floor}"
    );
    assert!(
        floor.contains("MAPPASS") && floor.contains("llvmpipe"),
        "the lavapipe floor failed the host-visible map stress:\n{floor}"
    );

    // 2) THE DEFAULT PATH: must complete with a structured VkResult — never a hang
    //    (timeout kills it → empty tail) or a crash (no INSTANCE line). Today venus's
    //    OOM makes this `INSTANCE ERR`; once venus is fixed or skipped upstream it
    //    becomes `INSTANCE OK`, and then the llvmpipe stress through the default
    //    instance must pass too.
    let default = guest
        .ssh_exec("timeout 120 python3 /tmp/vkprobe.py llvmpipe 2>&1 || true")
        .expect("ssh to the guest failed");
    eprintln!("default probe:\n{}", default.trim());
    assert!(
        default.contains("INSTANCE OK") || default.contains("INSTANCE ERR"),
        "the default Vulkan path hung or crashed instead of failing structuredly:\n{default}"
    );
    if default.contains("INSTANCE OK") {
        assert!(
            default.contains("MAPPASS"),
            "default instance succeeded but the llvmpipe stress failed — a half-working \
             venus is masking the floor:\n{default}"
        );
    }

    // 3) The seated session survived the probing: venus's failure stays contained to
    //    the probe process (autologin GNOME keeps running).
    let shell = guest
        .ssh_exec("pgrep -c gnome-shell || true")
        .expect("ssh to the guest failed");
    assert!(
        shell.trim().parse::<u32>().unwrap_or(0) >= 1,
        "gnome-shell died during the Vulkan probing (got {shell:?})"
    );

    // And the GPU worker itself must not have panicked.
    let log = guest.supervisor_log();
    assert!(
        !log.contains("gpu worker' panicked"),
        "the GPU worker panicked during the Vulkan fallback probing"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
