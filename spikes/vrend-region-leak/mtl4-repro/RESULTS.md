# Standalone MTL4 repro — RESULTS

## ✅ PREMISE PROBE for the structural fix (`reuse-probe.m`, 2026-08-09)

The proposed pool design (no destroy outside device teardown, high-water bounded by
construction) rests on one documented sentence: *"You can safely reuse command allocators after
ending the command buffer using it by calling `endCommandBuffer()`"* — note that, unlike
`reset()`, it does **not** require GPU completion. Four tests, no VM, seconds to run. Two runs,
numbers within 0.03 MiB of each other.

### T1 — reuse-while-in-flight is legal, AND self-regulating (the surprise)

8 command buffers × 20 000 dispatches onto **one** allocator, committed back-to-back with no
waits and no reset:

| cb | allocatedSize | delta | completed so far |
|---|---|---|---|
| 0 | 2.896 MiB | +2.859 | 0 |
| 1 | 5.755 MiB | +2.859 | 0 |
| 2 | 6.427 MiB | +0.672 | 1 |
| 3 | 6.958 MiB | +0.531 | 3 |
| 4–7 | 6.958 MiB | **+0.000** | 3→6 |

No errors, no corruption. The premise holds — but the shape is better than the design assumed:
growth does **not** accumulate per command buffer. It tracks the **in-flight working set** and
then goes flat, because Metal recycles heaps from *completed* command buffers back into the
allocator's free list **without any `reset`**. Two cbs in flight ⇒ ~2 cbs' worth of heap; once
completions keep pace, the size stops moving entirely.

**Consequence for the diagnosis.** An allocator's natural high-water is bounded by *its own*
concurrent in-flight work, which is small. The ~11 GiB therefore is not any single allocator
misbehaving — it is ~378 allocators each independently paying for its own in-flight working set
and never sharing. That is the population axis, and it is exactly the structural claim: **fewer,
shared allocators should reduce the total roughly in proportion.** This is the strongest support
the structural fix has received, and it came from a vehicle with no VM in it.

### ⚠ CORRECTION: the self-regulation is COMPUTE-ONLY. Render ratchets linearly.

The plateau above was read as a general property and stated that way. It is not. Scaling the
no-reset run shows two completely different behaviours:

| cbs, no reset | compute (T1) | render (T3) |
|---|---|---|
| 8 | 6.96 MiB | +15.9 MiB |
| 64 | 7.66 MiB | +73.4 MiB |
| 300 | — | +142.2 MiB |
| 600 | **5.82 MiB, flat from cb15** | +323.6 MiB |
| 1 200 | — | +531.6 MiB |
| 2 400 | — | **+1 020.7 MiB** |

Compute plateaus hard and stays there through 600 command buffers. **Render never plateaus** —
it decelerates early but runs ~0.42 MiB per pass indefinitely, reaching a gigabyte on ONE
allocator by 2 400 passes. So the earlier claim that "an allocator's natural high-water is
bounded by its own in-flight working set" holds only for compute; do not generalise it.

This also resolves the contradiction with the `NORESET=1` row further down (600 render passes →
216.7 MiB): that run was render, this one was compute. Same knob, different subsystem.

**And it makes the real-stack numbers add up for the first time.** `cs_start_render` mints a
fresh Metal command buffer per render pass but begins them all on the same `cmd->gfx.allocator`,
which is reset only at the next `vkBeginCommandBuffer`. So an allocator accumulates *every render
pass in a Vulkan command buffer epoch* at ~0.42 MiB each — and ~1 800 passes in one heavy epoch
is exactly the observed 753 MiB maximum. Reset does reclaim (`RESET_EVERY=1` with `RENDER=1`
stays flat at 25 regions, below); it simply does not run often enough.

**Consequence for the design: reset is load-bearing, not optional.** The pool must reset returned
allocators once drained, and the bound is `pool_size × (passes between resets × ~0.42 MiB)`. A
"never reset, let it self-regulate" design would have been correct for compute and catastrophic
for render — which is the whole workload.

### T2 — `reset` genuinely reuses heaps

Reset after the epoch drained, then encoded the identical work again: **6.958 → 6.958 MiB,
+0.000**. Reset is not the problem, and never was.

### T3 — ⚠ cross-type reuse does NOT happen

Render work on a compute-warmed allocator: **6.958 → 24.568 MiB (+17.6)**. A type switch does
not reuse the existing heaps; the allocator ends up holding the **union** of the render and
compute working sets. A shared pool that lets any allocator serve either kind pays this union on
every pool member. The design must segregate by encoder type — which KK's structure already
suggests, since `gfx` is render and `pre_gfx`/`post_gfx` are compute.

### T4 — feedback handlers do NOT accumulate

One tagged handler added to a reused `MTL4CommitOptions` before the first commit, then 6 more
commits with no handler added: **fired exactly 1 time**. Metal snapshots the handler list per
commit. This settles empirically what the pending-at-reset instrument had to assume, confirms its
discharge timing was right, and makes the `fired`/generation guard dead code (kept anyway — it
costs nothing and the semantics are undocumented).

### Running it

```bash
clang -fno-objc-arc -O1 -g -o /tmp/reuse-probe reuse-probe.m \
  -framework Foundation -framework Metal -framework QuartzCore && /tmp/reuse-probe
# DISPATCHES=n work per cb (default 20000), CBS=n cbs per allocator (8), COMMITS=n for T4 (6)
```

---

## ✅ IT REPRODUCES — the differential is the command-allocator reset (2026-08-09)

After the interposer named the site (`zink_blit → vk_meta blit → begin render pass →
`IOGPUResourceCreate`, see ../README.md), the missing knob was obvious in hindsight: **every
earlier variant reset the allocator once per iteration.** KK does not. `cs_start_render`
(`kk_cmd_buffer.c:265`) mints a **fresh `MTL4CommandBuffer` per render pass** but begins every
one of them on the **same long-lived `cmd->gfx.allocator`**, which is only `reset` in
`kk_reset_cmd_buffer_internal` — i.e. once per `vkResetCommandBuffer`, with arbitrarily many
render passes in between.

600 render passes, one colour attachment, no VM and nothing above Metal:

| variant | regions | footprint | AGXResource |
|---|---|---|---|
| reset every iteration | 4 → **25** | 5.4 M → 19.4 M | +29 |
| **`NORESET=1`** | 4 → **1 605** | 5.4 M → **216.7 M** | **+1 451** |

~2.7 leaked regions per render pass, and nothing comes back at settle.

### Bounded by the reset interval — `RESET_EVERY=n`, 1 200 iterations

| n | regions | shape |
|---|---|---|
| 1 | 25 | flat |
| 50 | **1 005** | plateaus hard by iteration 200, dead flat through 1 200 |
| 200 | **2 963** | 1 445 → 2 085 → 2 543 → 2 803 → 2 885 → 2 963, decelerating toward a plateau |

So `[MTL4CommandAllocator reset]` **does** reclaim — the earlier "unmeasured premise" holds — but
the pool **retains its high-water** and never returns it: `RESET_EVERY=50` holds **+911
`AGXResource` permanently** while flat. The persistent cost is therefore
*(number of live allocators) × (largest render-pass burst each ever saw between resets)*.

⚠ That is a **bounded** leak per allocator, while the in-VM ratchet is **linear with no
plateau**. So this mechanism is necessary but not yet sufficient to explain the real fault —
something must also grow the allocator population or the burst size over time. Do not write this
up as the whole answer until that multiplier is measured. (`AGXG13XFamilyCommandAllocator_mtlnext`
read 108 → 396 → 450 across an earlier run, which is itself decelerating — so it is not obviously
the multiplier either.)

---

**Everything below is the earlier negative pass**, kept because the eliminations still stand:
plain Metal, driven in KosmicKrisp's shape *with a reset every iteration*, does NOT ratchet.
This is a negative result, and a load-bearing one: it eliminates a whole family of hypotheses
that the in-VM data could not separate.

Vehicle: `mtl4cycle.m` (no VM, no virglrenderer, no vrend, no guest). Mirrors KK's submit shape —
one `MTL4CommandQueue` + one `MTL4CommitOptions` created once and reused (`kk_queue.c:152`),
three `MTL4CommandAllocator`s created once and `reset` per iteration (`kk_cmd_buffer.c:172`), a
fresh `MTL4CommandBuffer` per encoder per iteration (`:314`), and a fresh feedback handler
appended to the reused commit options on every submit (`kk_queue.c:91`).

## Two lenses, deliberately

Every row reports **both** the worker-style `vmmap --summary` `IOAccelerator (graphics)` region
count **and** the kernel-side `AGXResource` count from `ioclasscount`. "Regions flat" alone only
proves nothing leaked *into that vmmap tag* — an allocation under a different tag, or one with no
user-space mapping, would read flat while still ratcheting kernel objects. `AGXResource` is the
counter that tracked the in-VM leak 1:1, so it is the one that must also stay flat before
"clean" means anything. Both agree in every run below.

## Results (ITERS=600)

| variant | regions | AGXResource | reading |
|---|---|---|---|
| baseline (KK's shape, 1 800 cmd buffers / 600 commits) | 4 → **10 flat** | **7474 flat** | |
| `NOHANDLER=1` | 4 → **10 flat** | flat | **feedback-handler accumulation is DEAD** |
| `FRESHOPTS=1` (new commit options each iteration) | 4 → **10 flat** | flat | same — reuse is not the issue |
| `ALLOC=10` — 6 000 full `kk_alloc_bo`/`kk_destroy_bo` cycles (placement heap, shared, untracked, sparse-16, buffer from heap, residency add/commit/requestResidency → remove/commit/release) | 4 → **11 flat** | flat | **releasing an MTLHeap DOES return its kernel resource** |
| `TEX=10` — 6 000 MTLTexture create+release, with residency | 4 → **10 flat** | flat | textures clean |
| `TEX=10 NORESIDENCY=1` | 4 → **10 flat** | flat | the residency set is not the retainer |
| `ENCODE=1` — real compute pipeline + dispatch per command buffer | 4 → **52 flat** | 7530 flat | allocator chunk consumption is clean |
| `RENDER=1` — real render pass, colour attachment, vertex+fragment pipeline, draw | 4 → **25 flat** | 7506 flat | **tiler/parameter buffers are returned** |

Regions rise once to a small plateau (the working set) and then never move.

## What this eliminates

- **The feedback-handler hypothesis.** KK appends a new handler to a queue-lifetime
  `MTL4CommitOptions` on every submit, and whether Metal clears that list at commit is
  undocumented. It does not matter: identical with the handler, without it, and with fresh
  options each iteration.
- **"The ObjC wrapper dies while the kernel resource lives."** This was the leading hypothesis
  after the sentinel census showed objects dying while `ioclasscount` showed ~25k AGX resources
  per cycle surviving. Tested head-on for both resource types — heaps and textures, with and
  without a residency set — and the kernel resource comes back every time.
- **Allocator chunk retention across `reset`.** `[MTL4CommandAllocator reset]` reclaiming chunk
  memory was an unmeasured premise. With real encoding and real dispatches it holds.
- **Per-render-pass tiler allocations.** The prime suspect on shape (the leaked bytes are
  ~1 197 *pairs* of 1024K+768K regions per cycle, and 40 s at ~30 fps is ~1 200 frames — exactly
  render-pass shaped). Real render passes return them.

## What it means

The leak is **not in the Metal usage pattern**. Something about how the real stack runs differs
from this vehicle, and the remaining differences are the search space:

1. **Imported IOSurface-backed textures** — `newTextureWithDescriptor:iosurface:plane:`
   (`kk_image.c`, the `LIMINA-KK-IMPORT` lines). Cross-process, and the one resource kind this
   vehicle does not create.
2. **Multithreaded submission** — KK submits from several threads; this vehicle is
   single-threaded.
3. **Scale of live state** — thousands of simultaneously-live resources and a residency set with
   thousands of allocations, versus tens here. Commit cost and internal bookkeeping both scale
   with set size.
4. **The guest/virtio side being load-bearing** after all — i.e. the leak tracks guest resource
   churn rather than frames or submits.

⚠ Do not read "does not reproduce" as "Metal is exonerated". It narrows *how* Metal is being
driven, not whether Metal holds the memory — the kernel objects are still AGX resources.

## Running it

```bash
spikes/vrend-region-leak/mtl4-repro/run.sh                  # baseline
ENCODE=1 spikes/vrend-region-leak/mtl4-repro/run.sh         # one knob at a time
RENDER=1 ENCODERS=1 spikes/vrend-region-leak/mtl4-repro/run.sh
```

It is a GPU client, so it perturbs system-wide `ioclasscount` — run it when no VM snapshot series
is in flight, and trust its self-`vmmap` over any system-wide counter. `ENCODERS=0` aborts:
Metal rejects a commit of zero command buffers (the same complaint `kk_queue.c` guards with
`count > 0`).
