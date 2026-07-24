// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 guard: a guest empty-VkClearRect must not abort the host worker/VMM.
//!
//! A guest `vkCmdClearAttachments` with a zero-extent `VkClearRect`
//! (`rect.extent = {0,0}`, invalid usage per
//! VUID-vkCmdClearAttachments-rect-02682/-02683 but guest-controlled) flows
//! guest-mesa-venus (encode, no validation) → host virglrenderer vkr (decode) →
//! KosmicKrisp, which replays it at `vkQueueSubmit` into
//! `vk_meta_clear_attachments` → `setup_viewport_scissor`, whose
//! `assert(x0 < x1 && y0 < y1)` aborts the whole worker process — a
//! guest-triggerable host DoS (the whole VM dies).
//!
//! Two independent host fixes close it: vkr drops the degenerate rect at the
//! trust boundary (virglrenderer patch 0045), and KK's `vk_meta_clear` skips
//! empty rects as a backstop (kosmickrisp patch 0009). This guard drives the
//! **real stack** end to end: `guest/vkclearrect.py` (pure python3 + ctypes over
//! the guest's venus ICD — nothing to install/compile on the guest) issues the
//! poisoned clear and submits.
//!
//! Oracle: worker liveness. RED (unfixed) = the worker SIGABRTs during the
//! submit (the VM dies, `worker_pid()` no longer resolves). GREEN = the vehicle
//! prints `CLEARRECT PASS` and the worker is the SAME live pid afterwards. The
//! direct host-driver RED (KK abort on the identical clear) is proven by
//! `spikes/kk-empty-clear-rect` (the probe calls KK directly). Same prereqs as
//! the other venus L2s: 16 KiB kernel + enhanced disk + KosmicKrisp; SKIPs
//! cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const VKFDCYCLE: &str = include_str!("../guest/vkfdcycle.py");
const VKCLEARRECT: &str = include_str!("../guest/vkclearrect.py");

#[test]
fn empty_clear_rect_does_not_abort_the_worker() {
    if !limina_test::require_hvf_or_skip("empty_clear_rect_does_not_abort_the_worker") {
        return;
    }
    // venus's one supported host backend is KosmicKrisp; without it the coexist display
    // degrades to software-2D and venus can't enumerate — SKIP rather than misreport.
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED empty_clear_rect_does_not_abort_the_worker: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::enhanced_fedora_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED empty_clear_rect_does_not_abort_the_worker: {e}");
            return;
        }
    };
    eprintln!(
        "booting Fedora on the 16 KiB kernel (coexist venus + NAT) via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // Stage the vehicle + its ctypes plumbing (dependency-free: python3 + libvulkan).
    guest
        .ssh_exec(&format!(
            "cat > /tmp/vkfdcycle.py <<'VKFDCYCLE_PY_EOF'\n{VKFDCYCLE}\nVKFDCYCLE_PY_EOF"
        ))
        .expect("staging vkfdcycle.py in the guest");
    guest
        .ssh_exec(&format!(
            "cat > /tmp/vkclearrect.py <<'VKCLEARRECT_PY_EOF'\n{VKCLEARRECT}\nVKCLEARRECT_PY_EOF"
        ))
        .expect("staging vkclearrect.py in the guest");

    // The worker pid before the poisoned submit. If the worker SIGABRTs, this pid dies with
    // the whole VM (the test harness does not auto-restart it), so a resolvable SAME pid after
    // is the liveness oracle.
    let worker_before = guest.worker_pid().expect("finding the limina-vmm worker");
    eprintln!("worker pid before: {worker_before}");

    // Run the vehicle over the guest's venus ICD. If the worker aborts mid-submit the VM drops
    // and ssh returns an error / partial output; we assert on both the output and worker liveness.
    let out = guest
        .ssh_exec_timeout(
            "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
             timeout 60 python3 /tmp/vkclearrect.py Venus 2>&1 || true",
            Duration::from_secs(90),
        )
        .expect("running vkclearrect in the guest");
    eprintln!("--- vkclearrect ---\n{out}");

    // The vehicle must have actually reached the poisoned submit — otherwise the guard is
    // vacuous (e.g. venus didn't enumerate, or setup failed before the clear).
    assert!(
        out.contains("Virtio-GPU Venus") || out.contains("DEVICE"),
        "the vehicle never enumerated a venus device — guard would be vacuous.\n{out}"
    );
    assert!(
        out.contains("CLEARRECT SUBMIT"),
        "the vehicle never reached the poisoned vkCmdClearAttachments submit — the guard \
         would be vacuous.\n{out}"
    );
    assert!(
        out.contains("CLEARRECT PASS"),
        "the empty-rect clear did not complete cleanly (worker aborted, or a setup error).\n{out}"
    );

    // The decisive check: the worker survived the poisoned submit — same live pid.
    let worker_after = guest
        .worker_pid()
        .expect("the worker pid no longer resolves — it was killed by the empty-rect clear");
    assert_eq!(
        worker_before, worker_after,
        "the worker pid changed ({worker_before} -> {worker_after}) — the empty VkClearRect \
         aborted the worker and a new session/VM replaced it (guest-triggerable host DoS)"
    );
    eprintln!("worker pid after: {worker_after} (survived)");

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
