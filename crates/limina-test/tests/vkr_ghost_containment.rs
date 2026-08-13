// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A refused venus import must not kill the guest's GPU context.
//!
//! venus submits most object creation ASYNCHRONOUSLY — upstream's design, not our
//! configuration — so the guest is told `VK_SUCCESS` the moment the ring accepts
//! the command. When the host then refuses, the guest holds a handle nothing
//! backs, and the next command naming it used to miss vkr's object table, set the
//! ring FATAL, and take down every later command in the context (mesa aborts the
//! guest process on the FATAL bit). That is the 2026-08-13 dogfood totem crash:
//! GStreamer hands zink a **udmabuf** — plain guest pages wrapped as a dmabuf —
//! which is not attachable on a macOS host, zink imports it with no
//! `vkGetMemoryFdPropertiesKHR` gate, and the whole player died on the refusal.
//!
//! Two independent fixes have to hold, and this test drives both through the
//! shipped binaries:
//!
//!   * guest (mesa `limina-guest`): the import allocation is synchronous, so a
//!     refusal surfaces as `VK_ERROR_INVALID_EXTERNAL_HANDLE` at the
//!     `vkAllocateMemory` that can still handle it, and no ghost is minted.
//!   * host (virglrenderer `limina`): a failed create/import is TOMBSTONED, so a
//!     command naming the ghost of an async guest — every stock guest, forever —
//!     is dropped rather than poisoning the ring.
//!
//! The assertion is the same either way, which is the point: whatever the guest's
//! venus does, **the context survives a refused import**. `CONTEXT ALIVE` is the
//! probe's own proof — a plain allocate+map attempted AFTER the refusal.
//!
//! NOTE on what this can and cannot catch. On an enhanced image carrying the
//! guest fix, the refusal is synchronous and the host tombstone is never
//! exercised; the host half is proven only against an async (stock or older)
//! guest. Exercising it deterministically needs host-side fault injection —
//! backlogged in docs/hardening-backlog.md — so this test guards the invariant,
//! not the specific mechanism that upholds it.
//!
//! Vehicle: `guest/vkudmabufimport.py` (python3 + ctypes over /dev/udmabuf and
//! libvulkan; nothing to install). `forcealloc` mode skips the props gate exactly
//! as zink does, making `vkAllocateMemory` the first host refusal point. Same
//! prereqs as the other venus L2 tests: enhanced.test disk + KosmicKrisp; SKIPs
//! cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const VKUDMABUFIMPORT: &str = include_str!("../guest/vkudmabufimport.py");

#[test]
fn refused_venus_import_leaves_the_context_alive() {
    if !limina_test::require_hvf_or_skip("refused_venus_import_leaves_the_context_alive") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED refused_venus_import_leaves_the_context_alive: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED refused_venus_import_leaves_the_context_alive: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    guest
        .ssh_exec(&format!(
            "cat > /tmp/vkudmabufimport.py <<'VKUDMABUFIMPORT_PY_EOF'\n{VKUDMABUFIMPORT}\nVKUDMABUFIMPORT_PY_EOF"
        ))
        .expect("staging vkudmabufimport.py in the guest");

    // forcealloc: no props gate, so the host refusal lands on vkAllocateMemory —
    // the zink shape, and the one that mints a ghost on an async guest.
    let out = guest
        .ssh_exec(
            "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
             timeout 120 python3 /tmp/vkudmabufimport.py Venus forcealloc 2>&1 || true",
        )
        .expect("running vkudmabufimport in the guest");
    eprintln!("--- vkudmabufimport forcealloc ---\n{out}");

    // A guest kernel that cannot make the udmabuf at all would make the whole run
    // vacuous — the import under test would never be attempted.
    assert!(
        out.contains("UDMABUF OK") && out.contains("PRIME OK"),
        "the guest could not build the udmabuf under test, so nothing was imported — \
         check /dev/udmabuf and the virtio-gpu PRIME import.\n{out}"
    );

    // The invariant. CONTEXT DEAD means the ring was poisoned by the refusal:
    // either the guest minted a ghost (mesa limina-guest 0007 regressed) or vkr
    // failed to tombstone it (virglrenderer ghost containment regressed).
    assert!(
        out.contains("CONTEXT ALIVE"),
        "a refused venus import killed the GPU context — a plain allocate+map after \
         it failed. This is the totem crash class: one expected runtime refusal takes \
         down everything the guest process had on the GPU.\n{out}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
