# Handoff: buffer lifetime past client death — test matrix + design notes

**ACTIONED 2026-08-07 — this file is now a record, not a queue.** Written by a fork as a design
handoff; §1's trigger is answered in §9 and its fix shipped. What is still open lives in the task
list (matrix paths 3 and 4), not here. Kept in place because §§2–4 are the holder taxonomy the
answer was found by eliminating, and §9 needs them to make sense.

## 1. The trigger

> **ANSWERED 2026-08-07 — see §9.** It was the supervisor's own hold on published scanout
> IOSurfaces, not any of the vkr paths this matrix was written to test. Read §9 before spending
> time on §§2–4; they are still correct, and all of them came back clean.

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

> **TESTED 2026-08-07 (M3c) — the failure mode above does not occur.** `testcomp` now has a gbm
> client arm; `teardown-matrix.sh matrix gbm` and `leakexit gbm` drive it. Every path returns to
> baseline, and the RED that makes that mean something is real: 40 leaked gbm imports hold
> **316.9 MiB in the vrend bucket** (40 × 7.9 MiB = 1920×1080×4) where the shipped path holds
> `0 B live`.
>
> The provenance question this section implies — *is the buffer really vrend-owned?* — has an
> exact answer, and it is the budget ledger: a vrend-allocated IOSurface bills to the shared
> `"vrend"` pseudo-context, a venus one to the client's own. Ironically §6 is what makes this
> work: the shared bucket exists precisely *because* per-client attribution is impossible here.
> Right oracle for provenance, wrong one for retention.
>
> What §3 predicted still holds as *mechanism* — the `+1` does pin the vrend-owned surface, which
> is why RED retains — it just never gets stranded, because the venus side always reaches
> `vkr_device_memory_release`.

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
   **RUN 2026-08-08 (§10): clean, and structurally so — nothing is allocated before the create.**
4. Ring-FATAL / poisoned context — teardown taking a different branch than the healthy one.
   **RUN 2026-08-08 (§10): the premise is wrong — there is no second branch. What FATAL changes
   is WHEN: nothing is released until the guest process dies.**

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

> **RESOLVED (2026-08-07) — and it dissolved rather than got fixed.** This section used to read
> "Cheap fix: bind the TLS at the vrend dispatch entry too. **Do this before writing the
> vrend-holder tests.**" That prescription was wrong in premise, and the shipped answer
> (virgl `53e660e6`, `vkr_budget_set_vrend()` called from `vrend_resource_iosurface_init`,
> `vrend_renderer.c:8819`) is a **shared `"vrend"` pseudo-context**, not real attribution.
>
> The reason is structural, not an omission: a classic resource is created through the VMM's
> **global** resource-create path, which carries no context at all — the guest attaches it to one
> only afterwards — and that path runs on the same thread that dispatches venus. There is nothing
> to bind. A shared bucket is the honest answer; per-client blame for a vrend-owned surface will
> never exist.
>
> **Consequence for the M3c tests: the budget ledger is the wrong oracle for this whole column.**
> Do not assert per-context budget lines for vrend-owned surfaces. The census pair
> (`lookup A/F (+N)` and `DEALLOC … (alive M)`) is context-agnostic and is the right instrument.
>
> Stale in the fork itself: the `KNOWN LIMIT — attribution on the vrend path` paragraph at
> `src/venus/vkr_budget.h:66-71` still describes the pre-`53e660e6` state and still says to bind
> the TLS. Not worth a manifest bump alone — batch the comment fix with the next virgl commit.

**Historical limitation (now resolved, above):** `vkr_budget_set_context` is called only from the
**venus** dispatch entry points (`vkr_context_submit_cmd`, `vkr_ring_submit_cmd`), so a
vrend-allocated IOSurface would be charged to whatever context that thread last dispatched for.
Totals always stayed correct — charge and credit hit the same ledger.

## 7. Sequencing

Budget feature ships → ~~fix the vrend TLS attribution~~ (dissolved, §6) → write the path ×
holder tests → then design per-case ownership rules from whatever goes red.

The venus-holder column is done (§8). What remains is the vrend column plus paths 3 and 4, and
nothing gates them any more.

**Two traps for whoever writes the vrend arm:**

- **Prove the client's buffers are actually classic, per arm, host-side.** `MESA_LOADER_DRIVER_
  OVERRIDE` selects gbm's backing driver: get it wrong and the client's gbm buffers are
  zink→venus **blobs**, so the test re-runs the venus column while its name says vrend. That is
  the five-exonerations invariance failure waiting to happen. The positive proof is the worker's
  classic-attach / import-by-surface-id line — `info` level, so the `RUST_LOG` trap applies —
  and client-side, `vkclassicimport.py`'s discipline of refusing to run when it would be vacuous.
- **§4's not-a-bug case is the default outcome here, not an edge case.** The gbm client dying
  first destroys the vrend resource while the compositor still holds the borrowed `+1`; the
  surface staying alive until eviction is CORRECT, and from outside it is indistinguishable from
  the leak. Decide which you are looking at before calling anything red.

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

**M3c landed the same day** (limina `713a7ad`), closing the vrend-holder column and adding a
teardown path the original list did not have:

- **The vrend column is clean too**, with its own RED (40 leaked gbm imports = 316.9 MiB held in
  the vrend bucket vs `0 B live` shipped) and positive provenance proof per arm.
- **Path 1' — clean exit while still holding leaked imports.** Every kill in paths 1/2/2b goes
  through the live-instance full sweep, so none of them probe the bare-`free()` branch at all.
  Path 1' does, and it comes back with `instance was gone, 0 objects and 0 resources left` —
  **the branch ran with nothing to free**, on both holders. Structurally so: `vkr_instance_destroy`
  sweeps the whole hierarchy under the instance, and a leaked import is reachable from it.

**So §1's trigger is not a client-buffer lifetime bug, on either holder, however deliberately the
guest misbehaves.** Both holder columns × four teardown paths, each gated on a RED, all clean.
What is left is what never routes through that sweep:

- **paths 3 and 4** — the `-1000158000` error-path partial, and ring-FATAL teardown (cheap
  deterministic vehicle: an over-cap allocation trips `vkr_budget_kills_context`,
  `vkr_device_memory.c:285`). Each still needs its own RED.
- **the supervisor's Mach send right** — a different *process*, which no vkr sweep can reach and
  no oracle here can see.
- **caveat #1 in §7** — the context never actually died. Note every clean run above ends in a
  `destroying context` line; §1's incident was never confirmed to have one.

The last two are now the *leading* suspects rather than the leftovers, and neither is a vkr bug.

## 9. FOUND: it was the supervisor, and it is fixed (2026-08-07)

**The third suspect was the answer.** `testcomp/supervisor-retention.sh` — churn 300 fresh venus
scanouts at 1920×1080, quit the compositor cleanly, leave the guest up, read `owned unmapped`
settled both sides:

| | before | after |
|---|---|---|
| residual over baseline, compositor gone | **+195.9 MiB** | **+23.8 MiB** |
| freed by SIGKILLing the supervisor alone | **229.7 MiB** | 39.0 MiB |

The second row is the attribution: with the worker still alive and no guest process left, killing
the supervisor alone dropped the worker's `owned unmapped` from 230.6 MiB to 896 K. **The
supervisor was holding it, and nothing was ever going to make it let go.**

The mechanism is duller than any of the teardown paths this matrix was built to test.
`SurfaceStore` evicts only on *arrival* and publishes happen once per `IOSurfaceCreate` — so a
compositor that quits publishes nothing more, evicts nothing, and its last `SURFACE_STORE_CAP`
framebuffers stay pinned for the life of the supervisor. Not a teardown bug at all: a **missing
release edge**. Fixed by wiring the guest's `RESOURCE_UNREF` through to a release on the surface
port (limina `93ff513`, libkrun `d9afca2`), plus a store clear when a dead worker is replaced
(`01de871`), since no release protocol can cover a worker that has exited.

**Three things worth carrying out of this.**

1. **§0.4 had the number a day earlier and it was read as a bound, not a bug.** "436.8 M is
   `SURFACE_STORE_CAP` × one framebuffer" was correct arithmetic and the wrong conclusion — the
   cap *was* holding, and the question nobody asked was whether it would ever stop. A quantity
   that is explained is not the same as a quantity that is acceptable.
2. **The parked release-notify work was parked for a real hazard with the wrong fix in mind.**
   Ids recycle, so a release naming a dead id can drop a live one — true, and it looked like it
   needed an epoch plumbed through vkr, rutabaga and the device. It needed a *channel* change
   instead: same Mach port as the publishes, sent before teardown, and the ordering closes it.
   See RESULTS.md §0.4.
3. **§2's holder table was right about this row all along** — "supervisor's Mach send right /
   another **process** / released by supervisor drop". It sat last in a four-row table for a day
   while the first and fourth rows got the whole investigation, because they were the ones a
   census could see. **The holder no instrument reaches is the one to test first, not last.**

What remained open was paths 3 and 4, and §7 caveat #1 — see §10; the matrix is now fully run.

## 10. Paths 3 and 4, and §7's caveat, measured (2026-08-08)

Neither path was still a suspect for §1 by the time it was run — §9 had closed that — so this is
the matrix being finished rather than a hunt. Full numbers, traps and per-arm evidence are in
`testcomp/README.md` §M3e and §M3f; `testcomp/teardown-matrix.sh path3 | shipped3 | path4` re-runs
all of it, and the REDs need `fault-inject-paths-3-4.patch` on the worker.

**Path 3 — the error-path partial: nothing is stranded, and the reason is structural.** The
trigger is real and deterministic (`limina-testcomp badmod` asks for a modifier no driver
implements; KosmicKrisp answers `-1000158000`, `[totem]`'s own error). 40 refused creates leave
the census exactly flat. But the flatness is only worth reading because of *why*: with KK's
native `VK_EXT_image_drm_format_modifier` a modifier-tiled create reaches the driver verbatim and
**vkr allocates nothing before it** — the KK-linear IOSurface is allocated only after
`args->ret == VK_SUCCESS`. There is no partially-built object on this path to strand. The RED had
to synthesize one (pre-create allocation, dropped on the error path: +40 alive), and that is what
makes the shipped zero a result rather than an untested arm.

Two things the run added that the matrix never asked for:

- **The failing create does not poison the ring.** The same context allocates and renders
  normally afterwards (`survived=true`, every run). venus answered *this* create synchronously,
  so the guest saw the error and never used a ghost handle — the incident's "ghost object" half
  is not reproduced by this vehicle, and the host's own warning ("its next use will ring-FATAL")
  is accurate only for an async create.
- **The error path is reachable by any guest, cheaply.** A client asking for an unsupported
  modifier gets a clean per-context error and nothing else. That is the right behaviour, and it
  is now a test rather than a hope.

**Path 4 — ring-FATAL teardown: the branch premise was wrong, and the real answer is about
timing.** `vkr_context_destroy` gates its sweep on `ctx->instance` and nothing else, so a FATAL
context tears down through exactly the same code as a healthy one — there is no second branch to
audit. What a FATAL changes is *when* teardown happens at all:

| moment | what the host holds |
|---|---|
| context FATAL, guest process still alive | **everything** — census flat, budget still counting every byte |
| guest process gone | released: the full sweep runs (`with a valid instance`) and the borrowed `+1`s come back |

That middle row is **§7's caveat #1 turned into a measurement**: a FATAL context releases nothing
until its guest process goes away — the state the 2026-08-06 incident sat in for 38 s. It is not
a leak (the memory is still owned by a live client, and a client whose ring is dead may still be
killed by its user), but it is the shape a leak wears, and anything reading host memory during
that window will mis-read it.

Both holders (venus and gbm/vrend, provenance proved per arm) behave identically, each against a
RED that drops the release at teardown and does accumulate.

**The lever worth remembering: the GPU-memory budget is a deterministic context killer.** Boot
with `LIMINA_GPU_MEM_BUDGET_MIB=2048`, have the guest ask for 4 GiB, and `vkr_budget_kills_context`
sets that context FATAL on demand — no ring fault to provoke, no timing to win. Put the
over-allocation in the process that *holds* the references, not in a client: the budget kills the
context that asked.
