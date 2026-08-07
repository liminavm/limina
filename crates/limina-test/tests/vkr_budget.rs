// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Host GPU-memory budget: a runaway guest client dies, not the VM.
//!
//! Host memory allocated on the guest's behalf is invisible to the guest. A guest that
//! leaks `VkDeviceMemory` grows the *worker*, so the guest's own OOM killer never fires
//! and its memory graphs stay flat; what happens instead is that macOS eventually picks
//! the worker as the largest compressed process and SIGKILLs it — the VM dies with no
//! guest backtrace, no crash report, and at a moment unrelated to the allocation at
//! fault. That is exactly how a dogfood VM died on 2026-08-06 (a Vulkan compositor
//! re-allocating a 4K backdrop texture, ~51 GB/hour, killed at 142 GB — see
//! `spikes/wallpaper-backdrop-leak/`).
//!
//! `vkr_budget.c` bounds it: past the cap the offending venus context is refused and
//! killed deliberately, so one guest client loses its GPU context while the VM and every
//! other client keep running.
//!
//! **The oracle is the worker log, not the guest.** venus submits `vkAllocateMemory`
//! asynchronously (`vn_device_memory_alloc_simple` returns `VK_SUCCESS` as soon as the
//! command is on the ring) and never reads the host's `VkResult` back — so the guest
//! cannot observe the refusal as an error code, and an early version of this test failed
//! for exactly that reason. What the guest observes is its context dying, in whatever
//! form the ring death happens to take. Asserting on a specific guest-side VkResult would
//! be asserting on a value the transport discards.
//!
//! Four properties, each of which the feature is useless without:
//!
//! 1. **The cap is configured** — the boot log names it, which pins the env plumbing
//!    independently of whether any refusal fires.
//! 2. **The cap refuses and says so.** A context death that doesn't trace back to a log
//!    line owning it would be misdiagnosed as a transport bug (`limina-vulkan-oom-lies`),
//!    and the per-context size histogram is the part that names the runaway allocation —
//!    the 2026-08-06 leak was identified from exactly such a histogram.
//! 3. **The VM survives** — the whole point is that the blast radius is one client.
//! 4. **A fresh client works afterwards.** This is the ledger's credit path: charges from
//!    the killed context must be released. A ledger that only counts up would satisfy
//!    every assertion above and still break any guest that merely churns memory.
//!
//! Vehicle: `guest/vkbudget.py` (pure python3 + ctypes on libvulkan, nothing to install).
//! Boots the enhanced tier without a seated session (as `venus.rs` does) so the probe is
//! effectively the only Vulkan client and the cap is a property of its own allocations.
//! Same prereqs; SKIPs cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const VKBUDGET: &str = include_str!("../guest/vkbudget.py");

/// Cap for this boot, in MiB. Well above what an idle enhanced guest holds, so nothing
/// else in the session is affected, and well below what the hog below asks for.
const BUDGET_MIB: usize = 2048;
/// Per-allocation size, in MiB — big enough to reach the cap in a handful of calls.
const CHUNK_MIB: usize = 256;
/// Allocations the hog attempts. `CHUNK_MIB * MAX_CHUNKS` must exceed `BUDGET_MIB` by a
/// wide margin, or "no refusal" would be ambiguous between a broken cap and a short loop.
const MAX_CHUNKS: usize = 32;

const ICD: &str = "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json";

#[test]
fn runaway_guest_allocation_kills_the_client_not_the_vm() {
    if !limina_test::require_hvf_or_skip("runaway_guest_allocation_kills_the_client_not_the_vm") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED runaway_guest_allocation_kills_the_client_not_the_vm: no KosmicKrisp ICD \
             under /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::enhanced_fedora_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log()
            .with_env("LIMINA_GPU_MEM_BUDGET_MIB", &BUDGET_MIB.to_string()),
        Err(e) => {
            eprintln!("SKIPPED runaway_guest_allocation_kills_the_client_not_the_vm: {e}");
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
            "cat > /tmp/vkbudget.py <<'VKBUDGET_PY_EOF'\n{VKBUDGET}\nVKBUDGET_PY_EOF"
        ))
        .expect("staging vkbudget.py in the guest");

    // The hog is expected to die: past the cap its venus context is killed and mesa
    // aborts the process on ring death. `|| true` because a killed client is the SUCCESS
    // path here, and its exit status carries no information either way.
    let out = guest
        .ssh_exec(&format!(
            "{ICD} timeout 180 python3 /tmp/vkbudget.py Venus hog {CHUNK_MIB} {MAX_CHUNKS} \
             2>&1 || true"
        ))
        .expect("running the vkbudget hog in the guest");
    eprintln!("--- vkbudget hog ---\n{out}");

    let log = guest.supervisor_log();
    // Echo the host side of the story. The guest's own output is nearly content-free here
    // (it cannot see the refusal), so these lines are what anyone debugging this test — or
    // reading it to learn what the feature does — actually needs.
    eprintln!("--- worker budget lines ---");
    for line in log
        .lines()
        .filter(|l| l.contains("GPU budget") || l.contains("ring FATAL"))
    {
        eprintln!("{line}");
    }

    // (1) The cap reached the worker. Checked first because it is the one failure that
    // would make every assertion below meaningless — the first version of this test read a
    // "no refusal" result as a broken cap when the truth was elsewhere. It is asserted
    // AFTER the hog, not at boot: venus initializes lazily, when the guest creates its
    // first context, so before that there is nothing to announce the cap.
    assert!(
        log.contains(&format!("limina GPU budget: cap {BUDGET_MIB} MiB")),
        "the worker never announced a GPU-memory cap, so LIMINA_GPU_MEM_BUDGET_MIB did not \
         reach it — nothing below would be testing enforcement.\n{log}"
    );

    // (2) The refusal fired and identifies itself.
    assert!(
        log.contains("limina GPU budget: REFUSING"),
        "the guest allocated {} MiB of host GPU memory against a {BUDGET_MIB} MiB cap \
         without ever being refused — the budget is not being enforced, so a runaway guest \
         still takes the whole VM down with the worker.\nguest said:\n{out}",
        CHUNK_MIB * MAX_CHUNKS
    );
    assert!(
        log.contains("live of"),
        "the refusal did not print the per-context live breakdown, which is the part that \
         names WHICH allocation is running away (the 2026-08-06 leak was identified from \
         exactly such a size histogram)."
    );
    assert!(
        log.contains("ring FATAL set at"),
        "the allocation was refused but the context was not actually killed — on venus a \
         refusal that only returns an error changes nothing, because the guest never reads \
         the result (see vkr_budget.h)."
    );

    // (3) One client died, not the VM.
    let alive = guest
        .ssh_exec("echo still-alive")
        .expect("the guest stopped answering ssh after a client hit the GPU-memory cap");
    assert!(
        alive.contains("still-alive"),
        "the guest is no longer usable after one client hit the cap: {alive}"
    );

    // (4) The dead context's charges were credited back. Without that, the ledger climbs
    // forever and this fresh, modest allocation is refused too.
    let probe = guest
        .ssh_exec(&format!(
            "{ICD} timeout 120 python3 /tmp/vkbudget.py Venus probe {CHUNK_MIB} 2>&1 || true"
        ))
        .expect("running the vkbudget recovery probe in the guest");
    eprintln!("--- vkbudget probe ---\n{probe}");
    assert!(
        probe.contains("PROBE OK"),
        "a fresh Vulkan client could not allocate after the runaway one was killed — the \
         budget ledger is not crediting a dead context's charges back, which would break \
         every guest that merely churns memory.\n{probe}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
