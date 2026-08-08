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

/// Parse one `BUDGET HEAP <i> <when> flags=.. size=.. budget=.. usage=..` line's numbers.
fn heap_line(out: &str, index: u32, when: &str) -> Option<(u64, u64)> {
    let prefix = format!("BUDGET HEAP {index} {when} ");
    let line = out.lines().find(|l| l.starts_with(&prefix))?;
    let field = |k: &str| -> Option<u64> {
        line.split_whitespace()
            .find_map(|f| f.strip_prefix(k))?
            .parse()
            .ok()
    };
    Some((field("budget=")?, field("usage=")?))
}

/// The guest is told the truth about *our* cap, not about Metal's heap.
///
/// The sibling test above exists because a refusal cannot be delivered: venus submits
/// `vkAllocateMemory` asynchronously and discards our `VkResult`, so the only refusal that
/// sticks is killing the context. That reasoning is specific to *allocations*. A budget
/// query is not an allocation — `vn_GetPhysicalDeviceMemoryProperties2` issues a real
/// synchronous `vn_call_` round-trip whenever the budget struct is chained
/// (`vn_physical_device.c`) — so `VK_EXT_memory_budget` is the one backpressure channel the
/// transport does not throw away, and the only way a well-behaved client can learn to drop
/// caches *before* we kill it.
///
/// Two properties, both of which are false if the host merely forwards the query to
/// KosmicKrisp (which is what it did before this test existed — the RED):
///
/// 1. **The budget reflects our cap**, not the host GPU's. A guest told it has tens of GiB
///    when we will kill it at 2 GiB is being actively misled: it will size its caches for
///    a budget that does not exist.
/// 2. **Usage tracks what this client actually holds**, so the number moves when the guest
///    allocates. A budget that never changes is indistinguishable from a constant, and a
///    client cannot back off against a constant.
///
/// `VN_DEBUG=mem_budget` is set explicitly in the client's environment: venus gates the
/// extension on it (`.EXT_memory_budget = VN_DEBUG(MEM_BUDGET)`) and reads it once per
/// process. The test deliberately does *not* rely on the `/etc/environment.d` drop-in that
/// ships this for real desktop clients — that file is sourced by the session, and this
/// probe runs over a non-login ssh shell which would not see it.
#[test]
fn the_guest_sees_our_cap_through_vk_ext_memory_budget() {
    if !limina_test::require_hvf_or_skip("the_guest_sees_our_cap_through_vk_ext_memory_budget") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED the_guest_sees_our_cap_through_vk_ext_memory_budget: no KosmicKrisp ICD \
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
            eprintln!("SKIPPED the_guest_sees_our_cap_through_vk_ext_memory_budget: {e}");
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

    // Stay well under the cap: this client must survive to make its second query. Four
    // chunks is 1 GiB against a 2 GiB cap.
    const CHUNKS: usize = 4;
    let out = guest
        .ssh_exec(&format!(
            "{ICD} VN_DEBUG=mem_budget timeout 180 python3 /tmp/vkbudget.py Venus budget \
             {CHUNK_MIB} {CHUNKS} 2>&1 || true"
        ))
        .expect("running the vkbudget budget query in the guest");
    eprintln!("--- vkbudget budget ---\n{out}");

    assert!(
        !out.contains("BUDGET NOEXT"),
        "venus did not advertise VK_EXT_memory_budget even with VN_DEBUG=mem_budget set, so \
         the guest cannot query a budget at all and nothing below is testable.\n{out}"
    );
    assert!(
        out.contains("BUDGET DONE budget"),
        "the guest-side budget probe did not run to completion.\n{out}"
    );

    let target: u32 = out
        .lines()
        .find_map(|l| l.strip_prefix("BUDGET TARGET "))
        .and_then(|v| v.trim().parse().ok())
        .expect("the probe did not name the heap its allocations land in");
    let (before_budget, before_usage) =
        heap_line(&out, target, "before").expect("no 'before' line for the target heap");
    let (after_budget, after_usage) =
        heap_line(&out, target, "after").expect("no 'after' line for the target heap");

    let cap = (BUDGET_MIB as u64) * 1024 * 1024;
    let allocated = (CHUNKS as u64) * (CHUNK_MIB as u64) * 1024 * 1024;
    eprintln!(
        "heap {target}: budget {before_budget} -> {after_budget}, usage {before_usage} -> \
         {after_usage} (cap {cap}, allocated {allocated})"
    );

    // (1) The budget is OUR cap, not the host GPU's heap. This is the assertion that fails
    // against a blind passthrough: KosmicKrisp answers with Metal's number, which is far
    // larger than the cap we will actually kill the client at.
    assert!(
        before_budget <= cap,
        "the guest was told it has {before_budget} bytes of budget while the host will kill \
         its context at {cap} — the budget query is being forwarded to the GPU driver \
         instead of answered from our ledger, so the guest sizes its caches against memory \
         it is not allowed to have.\n{out}"
    );

    // (2) The numbers move. Allocating must show up as usage; asserting on the delta rather
    // than the absolute keeps this honest about whatever else the session holds. The
    // tolerance is generous downward only — venus rounds allocations up, never down.
    let used = after_usage.saturating_sub(before_usage);
    assert!(
        used >= allocated / 2,
        "the client allocated {allocated} bytes but reported usage moved by only {used} — a \
         budget that does not track what the client holds is a constant, and a client \
         cannot back off against a constant.\n{out}"
    );
    assert!(
        after_budget <= before_budget,
        "the reported budget did not shrink after the client allocated {allocated} bytes \
         ({before_budget} -> {after_budget}).\n{out}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
