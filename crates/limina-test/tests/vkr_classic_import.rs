// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Classic-virgl-resource → venus import regression test (the synoik stall class).
//!
//! A Vulkan compositor allocates its KMS buffers with gbm; since the
//! drop-guest-zink GL flip, gbm resolves to the **virgl dri backend**, so those
//! buffers are classic vrend pipe resources — not venus blobs. Importing one into
//! venus (`vkGetMemoryFdPropertiesKHR` on the exported dmabuf, then
//! `vkAllocateMemory` with `VkImportMemoryFdInfoKHR`) used to fail on macOS:
//! `proxy_context_attach_resource` could not export a dmabuf from a pipe resource
//! and silently dropped the attach, so vkr answered every import with
//! "invalid res_id" / `VK_ERROR_INVALID_EXTERNAL_HANDLE` — a live
//! stall (synoik/gnome-shell-rs rendering zero frames while the window
//! kept Plymouth's last image; memory `limina-vrend-context-poison` relatives).
//!
//! The fix attaches IOSurface-backed classic resources fd-lessly by surface id
//! (a SCANOUT-bound gbm buffer's vrend texture IS the display IOSurface since the
//! EGLImage scanout work), and the venus side imports the same bytes as a host
//! pointer. This test drives the exact guest-visible chain and — decisively — the
//! ALIAS check: a pattern written through gbm must read back through the imported
//! venus memory via a GPU copy, proving the two views share storage ("pixel-verify;
//! proxies lie").
//!
//! Vehicle: `guest/vkclassicimport.py` (pure python3 + ctypes on libgbm +
//! libvulkan; nothing to install). `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` is
//! forced so gbm takes the virgl driver even if the image's session env changes —
//! with zink selectors the buffer would be a venus blob and the test vacuous (the
//! probe refuses to run in that config). Same prereqs as the other venus L2 tests:
//! enhanced.test disk + KosmicKrisp; SKIPs cleanly if missing. Gated behind
//! LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const VKCLASSICIMPORT: &str = include_str!("../guest/vkclassicimport.py");

#[test]
fn classic_virgl_gbm_buffer_imports_into_venus() {
    if !limina_test::require_hvf_or_skip("classic_virgl_gbm_buffer_imports_into_venus") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED classic_virgl_gbm_buffer_imports_into_venus: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED classic_virgl_gbm_buffer_imports_into_venus: {e}");
            return;
        }
    };
    eprintln!(
        "booting the seated enhanced desktop (coexist + NAT) via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // The probe drives its own gbm+venus stack; the seated session only needs to be
    // up so the GPU stack is fully initialized (and to mirror the compositor shape).
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated enhanced session didn't come up");

    guest
        .ssh_exec(&format!(
            "cat > /tmp/vkclassicimport.py <<'VKCLASSICIMPORT_PY_EOF'\n{VKCLASSICIMPORT}\nVKCLASSICIMPORT_PY_EOF"
        ))
        .expect("staging vkclassicimport.py in the guest");

    let out = guest
        .ssh_exec(
            "MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu GALLIUM_DRIVER=virgl \
             VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
             timeout 120 python3 /tmp/vkclassicimport.py Venus 2>&1 || true",
        )
        .expect("running vkclassicimport in the guest");
    eprintln!("--- vkclassicimport ---\n{out}");

    assert!(
        out.contains("CLASSICIMPORT PASS"),
        "classic-virgl gbm buffer failed to import into venus — the synoik/Vulkan-compositor \
         KMS-buffer chain is broken (PROPS FAIL -1000072003 here means the fd-less \
         IOSurface attach regressed; check the worker log for 'invalid res_id').\n{out}"
    );
    // The content proof is load-bearing, not optional: binding without aliasing would
    // composite garbage. If a stack change makes this legitimately unreachable, that
    // is a conscious decision to make here, not a silent skip.
    assert!(
        out.contains("ALIAS OK"),
        "import succeeded but the imported venus memory did not read back the pattern \
         written through gbm — the two views do not alias the same storage.\n{out}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
