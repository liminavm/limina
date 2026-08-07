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
its host allocations (mtl_shm carrier fd, IOSurface refs, gbm bo, udmabuf fd) leak. Same shape as
the 2026-07-10 incident (12k orphaned PSXSHM fds starving a fresh session at login).

> **CORRECTION (2026-08-07, verified in source while building the M3 vehicle).** The paragraph
> above originally continued "few clients destroy their `VkInstance` before exit, so
> teardown-by-DRM-fd-cleanup fires on essentially every venus client exit", and §4 built path 2
> on it. **That is backwards.** `vkr_context_destroy` gates the sweep on the instance being
> *live*:
>
> ```c
> if (ctx->instance)                                    /* vkr_context.c:1005 */
>    vkr_instance_destroy(ctx, ctx->instance, false);
> ```
>
> A `SIGKILL`ed client never calls `vkDestroyInstance`, so `ctx->instance` is **still set** and
> the full sweep runs: `vkr_context.c:1006` → `vkr_physical_device_destroy`
> (`vkr_physical_device.c:94`) → `vkr_device_destroy` → `vkr_device_object_destroy`, whose
> "always cleanup vkr allocs" branch (`vkr_device.c:332-341`) calls `vkr_device_memory_release`
> and **drops the `+1`**. limina builds with `-Drender-server-worker=thread`
> (`scripts/build-virglrenderer.sh:61`), so the real `vk->FreeMemory` runs too.
>
> The bare-`free()` branch (`vkr_context.c:1067`) is reached by what *survives* that sweep —
> orphans — and by contexts whose instance was already gone. So **abrupt exit is the path that
> cleans up, and clean exit is the one that can leave orphans.** Path 2 may well come back green
> at the vkr layer; if it does, the compositor-quit residual came from somewhere else (path 4's
> poisoned teardown, a sweep orphan, the supervisor's Mach send right, or caveat #1 in §7 — the
> context never actually died). The discriminator is already in-tree and should be recorded for
> every teardown test:
>
> ```
> destroying context %u (%s): instance was %s, %u objects and %u resources left in the tables
> ```
>
> (`vkr_context.c:1013`). M3b is therefore written as a **discriminator, not a confirmation** —
> see §8.

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

Note **which side has to die** for that: the holder of the borrowed `+1` is the **importer** —
the compositor. A test that only kills *clients* exercises exporter-side death and never reaches
this. M3b kills testcomp itself, with its import cache populated.

Reaching this case at all also needs a **vrend-allocated** client buffer, not a venus one: a
venus↔venus import has both refs on the same side and is symmetric. Since the 2026-08-04
`MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` flip that is also the *realistic* shape — a GL client
allocates classic gbm resources. That arm is M3c (see §8); it is the one that needs the
`vkr_budget_set_context` prerequisite in §6.

The plumbing M3c needs is in place: **both** halves of the classic-gbm→venus import ship — the
SCANOUT half 2026-08-05 (virgl `bc03f705`/`37bb9d6c`) and the non-scanout half 2026-08-06
(limina `1a4a442`, virgl `034f7086`, the `VIRGL_BIND_SHARED` gate). So a testcomp client can
allocate through gbm under `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` and have the compositor
import it — which is exactly the asymmetric holder this section describes.

## 4. Test matrix — bunch by teardown PATH × holder

The cases collapse along one axis: they share a *teardown path*, and the path is what breaks, not
the combination. One test per path; split only when one goes red.

**Paths (4):**
1. Clean exit — client destroys `VkInstance`/`VkDevice` and frees its memory. Baseline; residual
   must be zero.
2. Abrupt exit (SIGKILL, no `VkInstance` destroy). **The common case** — but see the correction
   in §2: this leaves the instance *live*, so it is the path that runs the **full sweep**, not
   the one that skips it. Expect it to be green at the vkr layer and treat a residual here as
   the interesting result rather than the expected one.
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

## 8. The vehicle for these tests

`kmschurn.py` cannot reach §4: it has no clients, so nothing can outlive one. `testcomp/` is
the vehicle being built for that — a small but realistic compositor, bottom-up.

**Milestone 1 landed 2026-08-07** (limina `c645a86`, `e6d6609`): KMS + a Vulkan-allocated
scanout, page-flipped, no Wayland yet. It matches `kmschurn.py churn-vk` on a healthy host
(resting values within 1 MiB) *and* detects the retention bug it was validated against
(+4.17 GiB cap-lifted vs +361 MiB shipped). `testcomp/README.md` carries the numbers and two
measurement traps.

**M3 landed 2026-08-07** (limina `fcc43dc`, `a23a10e`, `ccda080`, `ae32255`), which is when this
matrix became testable. `testcomp/teardown-matrix.sh` drives paths 1, 2 and a new **2b** — the
importer's death, which the original path list omitted and which is the side that actually holds
the borrowed `+1`. Results and the oracle work-up are in `testcomp/README.md`:

- **Paths 1, 2, 2b: clean.** IOSurface `alive` returns to baseline in all three, and path 2
  logs `destroying context N ... with a valid instance`, confirming the §2 correction on real
  hardware rather than only from source.
- **The vehicle earned the client-dmabuf class**: a deliberate `--leak-imports` compositor
  against 40 distinct client dmabufs retains +41 IOSurfaces where the shipped path retains 0.
  Re-runnable as `testcomp/teardown-matrix.sh redgreen`.
- **The oracle is `DEALLOC iosurface N (alive M)`** from `vkr_mtl_refcount_census`, corroborated
  by `lookup A/F (+N)` on the same census line — that one counts precisely the outstanding
  borrowed `+1`s. Three others (`owned unmapped`, the `vmmap` `IOSurface` row, and the *other*
  `(+N)` counter on that same line, `iosurface A/F`) read *identical* in both arms. Do not reuse
  M1's oracle here, and do not say "the census counter" — the line carries two that disagree.

**And a result the matrix did not ask for: the sweep survives a maximally-misbehaving guest.**
Killing the `--leak-imports` compositor reclaimed all 41 retained imports — the RED peak reads
`lookup 506/465 (+41)` / `alive 43`, ctx 3 is torn down `with a valid instance`, and the next
census reads `lookup 508/508 (+0)` / `alive 1`. So **a venus↔venus client-buffer lifetime bug
cannot produce §1's compositor-quit residual**, however badly the guest behaves. That removes a
whole column from suspicion and leaves the paths that do *not* route through this sweep.

**Still untested, each needing its own RED first:** paths 3 and 4, and the whole vrend-holder
column (§3) — that one is M3c and wants the §6 `vkr_budget_set_context` fix ahead of it. Plus
the two non-vkr suspects §1 always carried: the supervisor's Mach send right, and caveat #1
(the context never actually dying).
