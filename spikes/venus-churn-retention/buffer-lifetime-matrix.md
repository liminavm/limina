# Handoff: buffer lifetime past client death — test matrix + design notes

**Action after the GPU-memory budget feature ships.** Written by a fork; no repo files were
touched. Move into `docs/hardening-backlog.md` (or a `spikes/` RESULTS.md) when actioned.

## 1. The trigger

User observation (2026-08-06, recalled 08-07): **host memory did not shrink when the Vulkan
compositor was quit.** This is the §6 discriminator that `spikes/wallpaper-backdrop-leak/RESULTS.md`
left open — guest compositor vs. our release path — and it points at the **host side**.

The reasoning: if the compositor were simply never destroying its backdrop texture, quitting it
tears down its DRM fd, and everything it owned should be released. It wasn't. That is much harder
to explain as a pure guest-side leak than as a failure in our release path — the virgl `0015`
class ("release the backing IOSurface on device/context teardown, not just `vkDestroyImage`"),
plus the IOSurfaceRef leak fixlet inside `0031`.

**Two caveats before treating it as settled** (this investigation has been burned by exactly this
kind of premise):

1. Confirm the context actually died — venus teardown is asynchronous relative to the guest
   process exiting. Look for `destroying context N` in the worker log, and rule out a successor
   inheriting the DRM fd (a lingering session, or gdm respawning the compositor and immediately
   re-allocating, would mask a real drop).
2. Judge the shrink over a **settled** window — two consecutive equal readings, the discipline
   `crates/limina-test/tests/venus_fd_census.rs` already uses.

## 2. Why buffers outlive their client here

They are not held by the compositor process. They are held by its host-side proxies in the
**worker**, a long-lived singleton that survives every guest process exit. Four holders:

| holder | where | released by |
|---|---|---|
| `vkr_context_destroy` bare-`free()` path | `src/venus/vkr_context.c` | **nothing** — see below |
| our scoped-IOSurface registry (`+1`) | `limina_registry_insert`, `src/venus/vkr_metal_helpers.m` | `vkr_mtl_iosurface_free` |
| supervisor's Mach send right for published scanout surfaces | another **process** | supervisor drop |
| cross-context import (`+1` on the exporter's IOSurface) | `mem->imported_iosurface` | `vkr_device_memory_release` |

The first is the important one and is **already documented in-tree as a leak canary**: with no
live `VkInstance` left to sweep, whatever remains in the object table gets a plain `free()` and
its host allocations (mtl_shm carrier fd, IOSurface refs, gbm bo, udmabuf fd) leak. That path is
**normal, not exceptional** — the same file notes few clients destroy their `VkInstance` before
exit, so teardown-by-DRM-fd-cleanup fires on essentially every venus client exit. Same shape as
the 2026-07-10 incident (12k orphaned PSXSHM fds starving a fresh session at login).

## 3. vrend × venus cross-imports — the asymmetric case

This is the path shipped 2026-08-06 (virgl `034f7086`, non-scanout import) and its ownership is
**asymmetric**, unlike venus↔venus:

- vrend **owns** the IOSurface — allocated in `vrend_resource_iosurface_init`, freed at resource
  destroy.
- venus holds a **borrowed `+1`** — `vkr_mtl_iosurface_lookup` returns `+1`, parked in
  `mem->imported_iosurface`, dropped only in `vkr_device_memory_release`.

Failure mode: if the venus context dies through the bare-`free()` branch,
`vkr_device_memory_release` never runs, the `+1` never drops, and the IOSurface **outlives both
renderers with no owner left to free it**. That is exactly the compositor-quit shape observed.

## 4. Test matrix — bunch by teardown PATH × holder

The cases collapse along one axis: they share a *teardown path*, and the path is what breaks, not
the combination. One test per path; split only when one goes red.

**Paths (4):**
1. Clean exit — client destroys `VkInstance`/`VkDevice` and frees its memory. Baseline; residual
   must be zero.
2. Abrupt exit (SIGKILL, no `VkInstance` destroy) — DRM-fd cleanup into the bare-`free()` branch.
   **The common case.**
3. Error-path partial — the `-1000158000`
   (`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`) create failure that fired 38 s before
   the jetsam kill. A partially-built object on an error path is the classic dropped release.
4. Ring-FATAL / poisoned context — teardown taking a different branch than the healthy one.

**Holders (2):** venus-side ref, vrend-side ref.

Plus two ordering cases that are **not bugs** and must not be "fixed":

- Live cross-context import, **exporter exits first** → storage staying alive is CORRECT, and
  looks identical from outside to the leak.
- Same, **importer exits first**.

And one global: **guest reboot** (every context dying at once) — the cheapest way to see whether
residuals are per-context or global.

## 5. Design constraint

Design the fix only **after** the enumeration. At least one case (live import) must *not* be
fixed: a blunt "release everything at context destroy" would break legitimate cross-context
sharing. The answer is probably per-case ownership rules, not one sweep.

## 6. Oracle — and a known limitation to fix first

The budget ledger being added now is the right oracle: a context torn down with bytes still
charged prints

```
ctx N [name] destroyed with X still charged — host GPU memory was not released at teardown
```

with the size histogram attached. That turns each test's assertion into one log check instead of
`vmmap` sampling plus settle-polling a noisy footprint.

**Limitation:** `vkr_budget_set_context` is called only from the **venus** dispatch entry points
(`vkr_context_submit_cmd`, `vkr_ring_submit_cmd`). vrend commands come through a different
dispatch, so a **vrend-allocated IOSurface is charged to whatever context that thread last
dispatched for, or `0`**. Totals stay correct (charge and credit hit the same ledger), but
per-context attribution is unreliable on the vrend path. Irrelevant for "did the total return to
baseline"; it matters if a test blames a specific context. Cheap fix: bind the TLS at the vrend
dispatch entry too. **Do this before writing the vrend-holder tests.**

## 7. Sequencing

Budget feature ships → fix the vrend TLS attribution → write the path × holder tests → then
design per-case ownership rules from whatever goes red.
