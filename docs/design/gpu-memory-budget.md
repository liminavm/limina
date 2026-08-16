# Host GPU-memory budget

**Status:** shipped 2026-08-07 (virglrenderer `24a3c2d9`). Accounting always on; the cap is
on by default with a generous ceiling set by the worker.

## The problem it solves

Host memory allocated on the guest's behalf is **invisible to the guest**. When a guest
leaks `VkDeviceMemory`, the bytes land in the limina worker's address space; the guest's
own OOM killer never fires, and the guest's memory graphs stay flat. On macOS the end state
is that the kernel eventually picks the worker as the largest compressed process and
SIGKILLs it — so the *entire VM* dies, with no guest backtrace, no crash report, and at a
moment unrelated to the allocation at fault.

That is not hypothetical: on 2026-08-06 a Vulkan compositor re-allocated a 4K backdrop
texture instead of reusing it (~51 GB/hour) and the dogfood VM was jetsam'd at 142 GB. The
investigation is in `spikes/wallpaper-backdrop-leak/`; the leak took a day to characterize
largely because nothing in the stack could say *which* allocation was growing.

## Shape

Two halves, in `third_party/virglrenderer/src/venus/vkr_budget.{c,h}`.

### Accounting (always on)

A per-context live total plus an **exact-size histogram**, charged at each host allocator
call site and credited at the matching free:

| site | charged | freed |
|---|---|---|
| the driver's own `vkAllocateMemory` | `allocationSize` | `vkr_device_memory_release` |
| `vkr_mtl_iosurface_alloc` / `_alloc_plain` | `IOSurfaceGetAllocSize` | `vkr_mtl_iosurface_free` |
| `vkr_mtl_shm_alloc` | the page-aligned size | `vkr_mtl_shm_free` |

**Charging at the allocator, not at the Vulkan entry point, is the rule that keeps this
honest.** A scanout memory that host-pointer-imports an IOSurface, or a cross-context
import that aliases the exporter's bytes, commits nothing new — those are skipped, so the
same 31.6 MiB is never billed twice.

**Sizes are exact, deliberately.** Rounding into buckets would have destroyed the signal
that cracked the backdrop leak: 767 allocations of *identical* size meant one repeated call
site, and `3840 × 2160 × 4 = 33,177,600 B` named it outright. Buckets are keyed by exact
size; when the table fills, dead buckets are evicted before live ones.

Three things the ledger prints:

- **80% watermark** — the full breakdown, once, while the VM is still healthy.
- **At refusal** — the same breakdown, naming the context.
- **At context destroy with charges outstanding** — `ctx N [name] destroyed with X still
  charged`. This is a direct statement from the release path that a footprint number can
  only hint at, and it is the guest-vs-host discriminator the leak investigation left open:
  a leak the ledger *sees* is the guest holding memory; a leak the ledger does *not* see
  while the process footprint climbs is ours.

### Enforcement (cap)

Past the cap, the allocation is refused and the offending context is **killed
deliberately**. One guest client loses its GPU context; the VM and every other client keep
running.

## Why it kills the context instead of returning an error

The obvious design — hand the guest `VK_ERROR_OUT_OF_DEVICE_MEMORY` and let it cope at the
call site — was tried first and **cannot work on venus**. From mesa's
`src/virtio/vulkan/vn_device_memory.c`:

```c
   /* vn_device_memory_alloc_simple */
   if (VN_PERF(NO_ASYNC_MEM_ALLOC)) {
      return vn_call_vkAllocateMemory(...);      /* synchronous — off by default */
   }
   vn_submit_vkAllocateMemory(..., &ring_submit); /* fire and forget */
   ...
   return VK_SUCCESS;                             /* before the host has even seen it */
```

The later `vn_device_memory_wait_alloc` waits on a **ring seqno**, not a result — no
`VkResult` ever travels back. Our `args->ret` is discarded. (`VN_PERF=no_async_mem_alloc`
makes it synchronous, but it is a guest-side, per-process opt-in with a round-trip cost, so
it cannot be the mechanism; the stock tier would be unprotected either way.)

The L2 test caught this: the guest cheerfully reported 32 successful allocations while the
host had charged 8 and killed the context after the 8th.

So the only refusal a host can make stick is to stop the context — and that is **strictly
better than the status quo**, because a failed host allocation already kills the context
today, just badly. The guest keeps a handle to a `VkDeviceMemory` that was never created
(the "ghost" the surrounding code warns about), and the next command touching it fails
object lookup and poisons the ring — delayed, misattributed, with a window in between where
the guest can touch the ghost. Doing it immediately closes that window and puts the reason
in the log.

`args->ret` is still set: correct under `VN_PERF=no_async_mem_alloc`, free otherwise.

## Policy

`LIMINA_GPU_MEM_BUDGET_MIB`, read once by the renderer:

- **unset** — accounting only, no cap. A bare virglrenderer is unaffected.
- **`0`** — accounting only, explicitly. The A/B lever and the escape hatch.
- **N** — cap at N MiB.

The worker sets the default (`default_gpu_mem_budget_mib`, `crates/limina-vmm/src/main.rs`):
`max(8 GiB, 2 × guest RAM)`. An explicit setting always wins.

`LIMINA_GPU_MEM_BUDGET_SOFT=1` refuses **without** killing the context. It is a debugging
mode, and it only works as a **pair** with `VN_PERF=no_async_mem_alloc` set on the guest
client you are investigating:

```sh
# host: cap low, don't kill
LIMINA_GPU_MEM_BUDGET_MIB=2048 LIMINA_GPU_MEM_BUDGET_SOFT=1 limina …
# guest: make the client's allocations synchronous
VN_PERF=no_async_mem_alloc ./the-leaking-app
```

That combination is the only way to get the thing you actually want when hunting *where* a
client allocates: `vkAllocateMemory` returns `VK_ERROR_OUT_OF_DEVICE_MEMORY` **at the call
site**, in the guest, with a live backtrace. Without the guest-side half, soft mode is worse
than useless — the guest ignores the refusal, keeps a ghost `VkDeviceMemory`, and poisons
its ring on the next use anyway, so you get the same context death with the cause several
commands in the past.

`LIMINA_GPU_MEM_BUDGET_CENSUS=<seconds>` logs the breakdown on a timer, cap or no cap. This
is the instrument for a guest-vs-host leak hunt, and **`vmmap` is not a substitute**: a 2 GiB
live set of venus images moved `owned unmapped` from 16.1M to 19.2M and back, which is noise
(`spikes/venus-churn-retention/`). Against a healthy compositor the census reads:

```
limina GPU budget: census — 126.9 MiB live of 16.0 GiB cap (0%)
limina GPU budget:   ctx 2 [synoik]: 126.9 MiB live — 4 x 14.1 MiB (IOSurface), …
```

`4 x 14.1 MiB` is a 4-slot swapchain at 2560×1440 — named, per context, no correlation
needed. Diff this series against the guest's own allocation census to localise a leak:
guest live flat while host live climbs means the retention is ours.

**Generous on purpose.** This is a runaway backstop, not a working-set limit. A healthy
desktop guest holds well under its own RAM in host GPU memory, and the two-tier guarantee
means a stock guest must never trip it in normal use — so the number sits far above any
legitimate workload and far below the footprint that gets a worker killed.

## Reading a refusal

```
vkr: limina GPU budget: cap 2048 MiB
vkr: limina GPU budget: 80% watermark crossed — 1.8 GiB live of 2.0 GiB cap (87%)
vkr: limina GPU budget:   ctx 2 [python3]: 1.8 GiB live — 7 x 256.0 MiB (device memory)
vkr: limina GPU budget: REFUSING a 256.0 MiB device memory allocation for ctx 2 [python3]
     and killing this context deliberately. This is limina's host-memory cap ...
vkr: limina GPU budget: at refusal — 2.0 GiB live of 2.0 GiB cap (100%)
vkr: limina GPU budget:   ctx 2 [python3]: 2.0 GiB live — 8 x 256.0 MiB (device memory)
vkr: context 2: ring FATAL set at vkr_dispatch_vkAllocateMemory:290
```

The refusal line is verbose by design. What the guest sees is a lost device or an aborted
process — which is also what a dozen unrelated venus transport failures look like
(`limina-vulkan-oom-lies`), so a refusal that did not name itself would be misdiagnosed as
a transport bug. **Never diagnose a venus context death from the guest symptom alone**;
read the worker log at that timestamp.

## Telling the guest, before we kill it

The section above is about *allocations*, and it is specific to them: venus discards our
`VkResult`, so a refusal can only be delivered by killing the context. A budget **query** is
not an allocation. The guest's `vn_GetPhysicalDeviceMemoryProperties2` issues a real
synchronous `vn_call_` round-trip whenever a `VkPhysicalDeviceMemoryBudgetPropertiesEXT` is
chained (`vn_physical_device.c`), so `VK_EXT_memory_budget` is the one backpressure channel
the transport does not throw away — the only way a client can learn to shrink its caches
*before* it loses its context.

`vkr_budget_answer_memory_budget` (`vkr_physical_device.c`) overwrites the driver's reply
from the ledger:

    heapUsage  = what the asking context holds
    heapBudget = cap − what every OTHER context holds     ("what is left for me")

clamped against the driver's own numbers and the heap size, floored at the caller's own
usage (the spec requires a non-zero budget and `budget ≥ usage`, which an exhausted cap
would otherwise violate — and memory you already hold *is* inside your budget). Only
`DEVICE_LOCAL` heaps are rewritten. With no cap configured nothing is touched and the
driver's answer stands, so a bare virglrenderer is unaffected.

Measured before the change, against a 2 GiB cap: the guest was told it had **13.8 GiB**, and
after allocating a further 1 GiB the reported budget went **up**, to 14.8 GiB — the
passthrough reports Metal's global state, which moves with unrelated host activity and is
anti-correlated with what the client just did. After: budget 2.00 GiB flat, usage
0 → 1.00 GiB.

The guest half is **configuration, not code**: venus advertises the extension only under
`VN_DEBUG(MEM_BUDGET)`, which is a runtime env gate (`vn_common.h:65`, read once per process
via `os_get_option`), so `install-enhanced.sh` ships `VN_DEBUG=mem_budget` in
`/etc/environment.d/90-limina-zink.conf` next to the driver selection. Two consequences
worth remembering: it must be in the client's environment *before* the process starts, and
**a non-login shell is not guaranteed to have `environment.d`'s variables** — on the current F44
enhanced image it does inherit them from the user manager, but that is an image property, not a
rule, so the test sets the variable explicitly rather than relying on it (`docs/graphics.md` §8).

## Known limits

- **Attribution on the vrend path.** `vkr_budget_set_context` is called from the venus
  dispatch entry points only, so an IOSurface allocated for a classic vrend resource used to
  be charged to whatever context that thread last dispatched for. vrend now binds a shared
  pseudo-context at its own entry (`vkr_budget_set_vrend`) — the honest answer rather than a
  fix, since at classic-resource creation time nobody owns the resource yet. Those bytes are
  billed to one "vrend" bucket instead of to an innocent guest client; totals are exact
  either way.
- **zink reads the wrong field.** `zink_query_memory_info` computes available memory as
  `heap.size − heapUsage` and ignores `heapBudget` entirely, so the GL-level
  `GL_NVX_gpu_memory_info` numbers still describe the host heap, not our cap. Clients that
  honour the extension as specified (`heapBudget`) do see the cap. Fixing zink is upstream
  work, not ours.
- **The cap is global, not per-context.** A well-behaved client can be refused because a
  badly-behaved one filled the budget. Per-context caps would be a fairer policy, but they
  need a fairness model nobody has needed yet; the histogram already names the culprit.
- **GL-only guests are unbounded.** vrend allocations are accounted (via IOSurfaces) but
  the cap is only enforced at `vkAllocateMemory`, which a pure-GL session never reaches.

## Test

`crates/limina-test/tests/vkr_budget.rs` + `guest/vkbudget.py`. Boots the enhanced tier
with a 2 GiB cap, hogs allocations until the context dies, and asserts four things: the cap
was configured, the refusal fired and printed a histogram and a ring-FATAL, the VM still
answers ssh, and a **fresh** client can still allocate. That last one is the credit path —
a ledger that only counted up would satisfy every other assertion and still break any guest
that merely churns memory.

The test asserts nothing about the guest-side `VkResult`, because the transport discards it.

A second test, `the_guest_sees_our_cap_through_vk_ext_memory_budget`, covers the reporting
half: it boots under the same cap, allocates well *under* it (the client must survive to ask
a second time), and asserts the reported budget is our cap rather than the GPU's heap and
that usage moves with what the client holds. Both assertions fail against a blind
passthrough, which is where the numbers quoted above came from.
