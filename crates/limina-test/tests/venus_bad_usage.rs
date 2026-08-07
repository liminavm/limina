// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 guard: spec-invalid Vulkan calls from the guest must not abort the host worker.
//!
//! A guest app's own bug must not be able to take down the VM, and repeatedly it has. Our
//! KosmicKrisp is built `buildtype=debugoptimized` with `b_ndebug=if-release`, so the ~820
//! `assert()`s in Mesa's common Vulkan runtime and in KK are compiled IN — and an assert on
//! the vkr ring thread aborts the whole worker process. Many of those asserts are on values
//! the guest controls, which makes any of them a guest-triggerable host DoS.
//!
//! This is the standing guard for that class, one arm per incident:
//!
//! - **zero-buffer** — `vkCreateBuffer` with `size = 0`, invalid per
//!   VUID-VkBufferCreateInfo-size-00912. Killed the dogfood VM on 2026-08-07:
//!   `vkr_dispatch_vkCreateBuffer` → `kk_CreateBuffer` → `vk_buffer_create` →
//!   `vk_buffer_init` → `assert(pCreateInfo->size > 0)` → SIGABRT. Fixed by rejecting it at
//!   the trust boundary in the virglrenderer fork.
//!
//! The fix always belongs in **vkr**, not only in the driver: vkr is where the untrusted
//! guest stream enters the host, and compiling the assert out does not make the value valid
//! — it just lets it reach Metal (a size-0 `MTLBuffer` is nil) and fail somewhere less
//! obvious. Asserts-off is defence in depth for the shipped build, not the fix.
//!
//! Oracle: worker liveness. RED (unfixed) = the worker SIGABRTs during the call, the VM
//! dies, and `worker_pid()` no longer resolves. GREEN = the vehicle prints `BADUSAGE PASS`
//! and the worker is the SAME live pid afterwards. The probe cannot judge its own success
//! any other way — the failure mode is the host dying, not a bad return value, and any
//! `VkResult` (including success) is an acceptable answer from the driver.
//!
//! Vehicle: `guest/vkbadusage.py` (pure python3 + ctypes over the guest's venus ICD, nothing
//! to install). Same prereqs as the other venus L2s: 16 KiB kernel + enhanced disk +
//! KosmicKrisp; SKIPs cleanly if missing. Gated behind `LIMINA_HVF_TESTS`; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const VKBADUSAGE: &str = include_str!("../guest/vkbadusage.py");

/// Every arm of the vehicle, run against one booted guest — a VM boot costs far more than an
/// extra invalid call, and each arm is independent. Add the arm name here when adding one to
/// `vkbadusage.py`.
const ARMS: &[&str] = &["zero-buffer"];

#[test]
fn spec_invalid_guest_calls_do_not_abort_the_worker() {
    if !limina_test::require_hvf_or_skip("spec_invalid_guest_calls_do_not_abort_the_worker") {
        return;
    }
    // venus's one supported host backend is KosmicKrisp; without it the coexist display
    // degrades to software-2D and venus can't enumerate — SKIP rather than misreport.
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED spec_invalid_guest_calls_do_not_abort_the_worker: no KosmicKrisp ICD \
             under /Volumes/mesa-cs/build-kk"
        );
        return;
    }
    let cfg = match GuestConfig::enhanced_fedora_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED spec_invalid_guest_calls_do_not_abort_the_worker: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh_banner(Duration::from_secs(180))
        .expect("guest sshd never became reachable through gvproxy");

    guest
        .ssh_exec(&format!(
            "cat > /tmp/vkbadusage.py <<'VKBADUSAGE_PY_EOF'\n{VKBADUSAGE}\nVKBADUSAGE_PY_EOF"
        ))
        .expect("staging vkbadusage.py in the guest");

    // If the worker SIGABRTs, this pid dies with the whole VM (the harness does not restart
    // it), so a resolvable SAME pid after every arm is the liveness oracle.
    let worker_before = guest.worker_pid().expect("finding the limina-vmm worker");
    eprintln!("worker pid before: {worker_before}");

    for arm in ARMS {
        // `|| true` so a dead VM surfaces as our own assertion below rather than as an ssh
        // error, which would say much less about what happened.
        let out = guest
            .ssh_exec_timeout(
                &format!(
                    "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
                     timeout 60 python3 /tmp/vkbadusage.py {arm} Venus 2>&1 || true"
                ),
                Duration::from_secs(90),
            )
            .unwrap_or_else(|e| {
                // The commonest failure here is not an ssh problem: the worker aborted, the
                // VM went with it, and the connection died mid-command. Say which it was —
                // a bare ssh error reads like flakiness and buries the actual finding.
                let died = guest.worker_pid().is_err();
                panic!(
                    "the {arm} arm lost the guest connection ({e:#}){}",
                    if died {
                        " — and the worker is GONE: a spec-invalid guest call killed the VM. \
                         That is the bug this guard exists for; the fix belongs in the matching \
                         vkr_dispatch_* in the virglrenderer fork."
                    } else {
                        " — but the worker is still alive, so this is an ssh/guest problem, \
                         not the abort this guard is looking for."
                    }
                )
            });
        eprintln!("--- vkbadusage {arm} ---\n{out}");

        // The vehicle must have reached the invalid call, or the guard is vacuous — a probe
        // that failed to enumerate venus proves nothing about what venus does with bad input.
        assert!(
            out.contains("DEVICE "),
            "the {arm} arm never enumerated a venus device — guard would be vacuous.\n{out}"
        );
        assert!(
            out.contains(&format!("BADUSAGE ARM {arm}")),
            "the {arm} arm never reached the invalid call — guard would be vacuous.\n{out}"
        );
        assert!(
            out.contains(&format!("BADUSAGE PASS {arm}")),
            "the {arm} arm did not survive the invalid call — the guest process died with \
             the host, or the probe itself errored.\n{out}"
        );

        let worker_after = guest.worker_pid().unwrap_or_else(|e| {
            panic!(
                "the worker pid no longer resolves after the {arm} arm — a spec-invalid \
                    guest call killed the VM. That is the bug this guard exists for; the fix \
                    belongs in the matching vkr_dispatch_* in the virglrenderer fork. {e:#}"
            )
        });
        assert_eq!(
            worker_before, worker_after,
            "the worker was replaced during the {arm} arm — it died and something restarted it"
        );
    }

    eprintln!(
        "SURVIVED: {} invalid-usage arms, worker still {worker_before}",
        ARMS.len()
    );
}
