# vrend GL region-leak repro

Repro oracle for the **open** regression found in the 2026-08-08 perf pass: the worker's
`IOAccelerator (graphics)` allocations ratchet ~9–12k regions / ~1.3 GB per GL workload
open/close cycle and never return. Full results and the scoping A/B:
`perf/2026-08-08-remeasure.md` §Memory.

## Running it

Boot the enhanced tier with `--net` and a seated desktop (see
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`), then:

```bash
spikes/vrend-region-leak/memcycle.sh        # shipped GL (vrend)   -> RATCHETS
spikes/vrend-region-leak/memcycle-zink.sh   # GL forced to zink->venus -> CLEAN
```

Each drives a 5 000-fish WebGL aquarium through `systemd-run --user --unit=ff-bench`, snapshots
`vmmap --summary` on the worker (and, in `memcycle.sh`, the supervisor) at open and at closed,
and settles 30 s after each close. Both assume ssh on port **2222** — `limina --net`
auto-allocates from 2222 up, so read the real port off the supervisor log and edit `SSHO`/`SSH`
if you are running more than one VM.

## Reading the output — the one trap

**Compare closed-to-closed, never open-to-closed.** Within a single cycle the close *looks*
like it releases (the numbers drop), which hides the ratchet completely. The signal is that
each successive *closed* state sits ~9–12k regions above the previous one:

```
closed 129 666 -> closed 138 876 -> closed 147 679 -> closed 160 028
```

A cache fills once and plateaus; this is linear. The fresh-cold-boot control is **3 851 regions
/ 600 MB / 4.3 GB footprint** — take one after a reboot to anchor the arithmetic.

`vmmap` takes minutes per snapshot once the region count is high, so a full three-cycle run is
slow — that slowness is itself part of the symptom.

## Narrowing so far (2026-08-08) — run `census-correlate.sh`

`census-correlate.sh` samples, at each open and closed state, the vmmap region count and
footprint alongside three accounting layers: virglrenderer's GPU-memory ledger, its Metal
refcount census, and the KosmicKrisp allocation census. Boot with both censuses on:

```bash
LIMINA_GPU_MEM_BUDGET_CENSUS=10 LIMINA_KK_BOCENSUS=10 \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
CYCLES=2 spikes/vrend-region-leak/census-correlate.sh
```

| stage | regions | footprint | KK bo live | KK tex live | virgl ledger |
|---|---|---|---|---|---|
| baseline | 3 834 | 4.4 G | 5 | 1 | 11.7 MiB |
| cycle 1 open | 26 272 | 8.2 G | 860 | 188 | 11.7 MiB |
| cycle 1 closed | 26 254 | 8.2 G | 854 | 146 | 11.7 MiB |
| cycle 2 open | 41 792 | 10.1 G | 823 | 53 | 11.7 MiB |
| cycle 2 closed | 41 767 | 10.0 G | 868 | **52** | 11.7 MiB |

Read it as a sequence of eliminations:

- **The virglrenderer ledger is blind.** Flat at 11.7 MiB / 4 charges throughout, and the Metal
  refcount census balances exactly (`iosurface 28/25 (+3), texture +0`). Nothing that leaks
  passes a charged call site. Per `vkr_budget.h`'s own discriminator, that puts the fault in a
  release path we own rather than in guest behaviour.
- **KK heaps/buffers — this elimination is CONTRADICTED, see below.** This run read live BOs
  854 → 868 closed-to-closed (+14) and that was called a plateau; the later sentinel run read
  +301 over the same protocol.
- **KK plane textures are not it.** Live textures *fall* across cycles, 188 → 146 → 53 → 52.

So ~15 500 regions per cycle are unaccounted while every counter that existed was flat or
falling — which is what motivated the per-class histogram below.

## RETRACTED, then re-measured: the heap claim was an instrument artefact

A first version of the per-class census counted a dealloc as "the `mtl_release` where the retain
count was 1". **That was wrong, and it produced a confident false finding.** A release-site
counter only sees deaths *we* cause; Metal objects routinely die in someone else's hands (an
allocation in a residency set is retained by the set and released at `commit`, after our
`mtl_release` has returned). Those deaths are invisible to it, so the object looks immortal —
indistinguishable from a leak.

It reported `AGXG13XFamilyHeap` with `made == live == 1 191` and zero deallocs, which was written
up as the cause and pointed at the residency set. **With a true dealloc sentinel** — an
associated object, released by the runtime when its host dies, whoever held the last reference —
**heaps plainly do die**: `made 845 / live 802`, `made 1412 / live 1103`. `MTLTextureDescriptorInternal`,
previously 2 000+ "immortal" objects, drops off the list entirely. Both were the same artefact.

The census now uses the sentinel, and prints a self-test first, because an instrument that never
fires reads identically to "nothing leaked".

## Where it actually stands

Sentinel-backed, at the closed state of each cycle:

| class | baseline | cycle 1 | cycle 2 | reading |
|---|---|---|---|---|
| `AGXG13XFamilyHeap` | 5 / 5 | 802 / 845 | 1 103 / 1 412 | dies normally; live tracks live BOs almost exactly (802/801, 1103/1102) |
| `AGXG13XFamilyCommandAllocator_mtlnext` | 108 | 396 | **450 / 450** | genuinely never deallocated |
| `AGXG13XFamilyArgumentTable_mtlnext` | 36 | 132 | **150 / 150** | genuinely never deallocated |
| `AGXG13XFamilyTexture` | — | 403 / 4 831 | 142 / 10 446 | recycles well |

The two `made == live` rows are real this time, but they are **not obviously a bug**: both are
released only in `kk_destroy_cmd_buffer`, which does not run while Vulkan command buffers are
pooled and recycled. They therefore track peak command-buffer count rather than work done.
(450 allocators = 150 pooled `VkCommandBuffer`s × 3 encoder states, `kk_cmd_buffer.c:130`.)
**But "bounded" there is asserted, not measured** — the retraction inverted. Each MTL4 allocator
also retains its chunk high-water across `reset`, so a bounded *count* need not mean bounded
*bytes*.

### ⚠ The heap row contradicts the BO plateau — unresolved

The two runs disagree about the same quantity. census-correlate read live BOs **854 → 868**
closed-to-closed (+14, published as "plateau, eliminated"); the sentinel run read live heaps
**802 → 1103** (+301), and its own note says heaps track live BOs almost exactly (801 → 1102).
Same protocol, ~20× apart, **and this write-up published both while keeping the elimination.**

+301 heaps/cycle at typical zink slab sizes (~10 MB) ≈ the ~3 GB/cycle byte growth — so this row
may well be **the bytes**, even though it cannot be the region count (300 ≪ 29 000). That points
at two co-travelling accumulations, not one. Resolve it before trusting either number: re-run
`census-correlate.sh` over ≥3 cycles reporting BO **bytes** at closed states.

**The open question is now a different one.** Regions grew **+29 000 in a single cycle**
(26 642 → 55 833, 3.1 → 6.5 GB) against roughly **+300** live heaps and falling textures. No live
object count comes within an order of magnitude of the region growth, so an `IOAccelerator
(graphics)` region is evidently **not** 1:1 with the Metal objects we mint. Working out what one
region actually corresponds to — and which of them are the 6.5 GB — is the next step, and it
should come before any further object-class guessing.

Every `mtl_new_*` creator in the bridge is wrapped (MTL4 command buffers `mtl_device.m:491`,
counter heaps `mtl_device.m:233`, texture views, heap sub-buffers `mtl_heap.m:71`), and the dump
sorts by live — so a leaked class of thousands would have surfaced. The hole is everything that
is **not a bridge-minted ObjC object**: MTL4 command-allocator chunk memory, per-commit AGX
kernel mappings, compiler/JIT workspaces, residency-set internals. No minted class exceeds ~1.2k
live against +29 000 regions/cycle, so the region count almost certainly lives **below the object
layer** — which is the independent reason to stop guessing classes.

## ✅ SOLVED: what an `IOAccelerator (graphics)` region IS (2026-08-08, `ioclass-cycle.sh`)

`ioclasscount` answered it in one run, at seconds per sample against `vmmap`'s minutes. Fresh
clone, cold boot, GL confirmed on vrend (`GALLIUM_DRIVER=virgl`). Regions at closed states
**4 397 → 30 537 → 59 206** (footprint 4.8 → 9.3 → 12.8 G), so **+26 140 / +28 669**.

System-wide kernel class deltas over the same closed-to-closed intervals:

| class | c0→c1 | c1→c2 |
|---|---|---|
| `IOMemoryMap` | +55 113 | +60 488 |
| `_IOMemoryDescriptorMixedData` / `OSSet` / `OSArray` | ~+26 240 | ~+28 800 |
| `IOGPUBufferMemoryDescriptor` | +23 603 | +25 874 |
| `AGXSecureMemoryMap` | +23 516 | +25 768 |
| `AGXAllocation` | +23 515 | +25 768 |
| `AGXResource` | +23 496 | +25 767 |
| `IOBufferMemoryDescriptor` | +2 646 | +2 897 |
| `IOGPUDeviceShmem` | +2 642 | +2 896 |
| everything else | ≤ +9 | ≤ +32 |

**`AGXResource` + `IOGPUDeviceShmem` = 26 138 / 28 663 against region growth 26 140 / 28 669 —
matching to within 2 and 6 regions across two intervals of different magnitude.** So one region ≈
one AGX kernel resource, in two populations that track each other 1:1 internally. `OSSet`/
`OSArray`/`_IOMemoryDescriptorMixedData` run at **1.005×** of that *sum* (one each per leaked
object of either population); `IOMemoryMap` at **2.11×** — ~2 maps per object but only one region
in our address space, so the second map is kernel/GPU-side.

**Attribution is settled twice over, despite `ioclasscount` being system-wide:** (a) the
system-wide class sum matches the *worker's own* per-process region growth to ±6, twice, at
magnitudes 2.5k apart — WindowServer or the supervisor contributing thousands would overshoot;
(b) after `systemctl poweroff`, every one of these classes returns to its pre-boot value. **The
leak is process-lifetime, owned by the worker, and released only when the worker exits.**

### Region structure (`vmmap -v` at c2-closed, 59 196 regions / 6.9 G)

Every single one is `rw-/rw- SM=SHM PURGE=N` — **non-purgeable**, so not a volatile cache the
kernel would reclaim under pressure.

| size | count | bytes | reading |
|---|---|---|---|
| **32K** | **41 189** | 1.29 G | **the count carrier** (70% of regions) |
| 1024K | 2 394 | 2.34 G | **the bytes carriers** — the two counts track 1:1 |
| 768K | 2 393 | 1.75 G | (only 87 pairs VA-adjacent: same event, different pools) |
| 16K | 6 042 | 94 M | ≈ the `IOGPUDeviceShmem` population |
| 48K / 160K / 128K | 3 049 / 2 999 / 757 | 153 / 469 / 95 M | |

**59 365 of 59 465 region boundaries are exactly contiguous** (longest run 6 904 regions, 53.9 GiB
of VA). The VA cursor marches monotonically with almost no holes — a **pure ratchet with no unmap
churn at all**, not a churn where some fraction escapes. The step-2 bimodal prediction is half
right: two populations yes, but the bytes carrier is thousands of ~1 MB regions, not a few
hundred huge ones.

### What this rules in and out

- **The ObjC census is not contradicted, and its own header said why**: *"objects we never pass
  through an `mtl_new_*` are invisible here"*. Census slot count `N = 17/19` of 128 (recovered
  from the dump lines), so the table never filled and **no class went silently uncounted** — the
  instrument hole is closed empirically, not by argument. The leaked kernel objects were simply
  **never wrapped by a bridge-minted ObjC object at all**: they are Metal-internal. The 41 189
  32K regions cannot be KK heaps — `kk_bo.c:147` mints exactly one `mtl_new_heap` per BO and the
  sentinel reads ~1.1k live, not 41k.
- **Do not call this per-frame.** 40 s × an *assumed* 60 fps ≈ 2 400 frames makes ~1 shmem and ~9
  AGX resources "per frame", but fps was never measured and several guest GL contexts (WebGL,
  WebRender, gnome-shell) flush independently, so commits ≠ frames. Per-**commit** arithmetic is
  consistent (~3 MTL4 command buffers × ~3 chunks) but **unobserved**. This investigation has
  already been burned twice by a striking match; leave it labelled a hypothesis.

## ✅ THE ALLOCATION SITE, NAMED (2026-08-09, `iokit-trace/`)

Full stack in `data/leaking-stack.txt`.

**The identification is arithmetic, not a resemblance.** Per-selector totals against the kernel
counter over the same window (fresh boot, ~110 s of the 5 000-fish aquarium):

| | |
|---|---|
| `AGXResource` | 7 541 → 35 070 = **+27 529** |
| selector 9 (`IOGPUResourceCreate`) total | **27 499** |
| **difference** | **30 — 99.9 %** |

So every leaked kernel resource is one `IOGPUResourceCreate` on this path, 1:1. Totalling **by
selector** is what made that comparison possible: the same selector is reached from dozens of
call sites, so any single stack's count is only a lower bound (the first reading, off one
truncated table, undercounted it as "~22k of 25 743" and would have been argued rather than
measured). Selector 29 (`IOGPUCommandQueueSubmitCommandBuffers`, 9 300–11 949) is the *busiest*
bucket and is **not** the leak — plain per-commit submission. Ranking by call count alone would
have fingered it.

```
zink_blit                          (host zink — the GL blit path)
 └ kk_CmdBlitImage2
   └ vk_meta_blit_image2 → vk_meta_blit_image → do_blit
     └ kk_CmdBeginRendering → cs_start_render
       └ mtl_new_render_command_encoder_with_descriptor
         └ -[AGXG13XFamilyCommandBuffer_mtlnext renderCommandEncoderWithDescriptor:options:]
           └ AGX::RenderContext::beginRenderPass → ContextCommon::newCommand
             └ IOGPUMetalCommandBufferStorageAllocResourceAtIndex
               └ IOGPUMetalResourcePoolCreatePooledResource
                 └ IOGPUResourceCreate     ← selector 9
```

**The leak is ~one `AGXResource` per Metal ENCODER-BEGIN (`ContextCommon::newCommand`), across
every pass type. The `vk_meta` blit path is the single biggest producer of encoder-begins, not
the whole fault.**

⚠ Do not narrow this to "every zink blit leaks" — an earlier draft of this section did, and it
is wrong in a way that would misdirect the fix. The blit stack is **12 048 of ~27 499** selector-9
calls, i.e. ~44 %. The remaining ~15k are buckets (2 181, 1 128, 1 000, 819, 753×2, …) sharing the
**same `ContextCommon::newCommand` tail** reached from draw/clear/compute passes. **Eliminating
every blit would roughly halve the rate and fix nothing.**

Corollary: the `> 100 copy boxes detected` zink warning is a **different subsystem**
(`zink_resource.c:3373`, copy-region tracking for readback validity) and is unrelated — same
workload, different mechanism. And 12 048 blits per 100 s ≈ 2–4 per frame is *normal*
compositing traffic (guest `glBlitFramebuffer` → vrend → zink native blit), not a pathological
storm. vrend is asking legitimately; there is no orders-of-magnitude reduction available there.

Also note every leaking class is `_mtlnext` — the **MTL4** encoding path, which KK moved to in
the 2026-08-05 rebase ([[limina-kk-mtl4-rebase]]). Combined with "regression was never measured",
that makes a pre-rebase KK build a genuinely informative one-boot A/B.

Note the shape: it is a **pool**, and a pool that never recycles is the whole fault. It grows to
the high-water mark of resources the pool cannot reclaim — consistent with
`AGXG13XFamilyCommandAllocator_mtlnext` and `ArgumentTable` sitting at `made == live` (released
only in `kk_destroy_cmd_buffer`, which does not run while `VkCommandBuffer`s are pooled and
recycled). That row was filed as "plausibly normal"; it is now a suspect again.

This is also why the standalone repro stayed clean through every variant: it did render passes,
but not *via `vk_meta` blits from a long-lived pooled command buffer*, which is the combination
that matters.

### The multiplier is NOT the allocator population

Two full cycles with the interposer and KK's per-class census on together:

| | cycle 1 closed | cycle 2 closed | delta |
|---|---|---|---|
| `AGXResource` | 32 866 | 54 996 | **+22 130** |
| selector 9 total | 25 398 | 47 816 | **+22 418** (1:1 again, ~1 %) |
| live `AGXG13XFamilyCommandAllocator_mtlnext` | 360 | 414 | **+54** |
| live `AGXG13XFamilyArgumentTable_mtlnext` | 120 | 138 | +18 |

414 allocators absorb +22 130 resources in one cycle — ~53 more per allocator per cycle, every
cycle. So the allocator population is flat while the leak is linear: **each allocator's pool
grows without bound**, which means reset is not reclaiming in the real stack even though KK does
register `.reset = kk_reset_cmd_buffer` (`kk_cmd_buffer.c:215`) and `kk_BeginCommandBuffer` calls
it (`:225`). Why the reset does not reclaim here is the remaining question.

⚠ One honest caveat about the isolated repro that bears on this: it never waits for completion,
so it resets allocators whose command buffers may still be executing. Metal does not permit that,
so the `RESET_EVERY` plateau may reflect an in-flight window rather than true pool trimming.
Re-run those variants with completion waits before leaning on the plateau numbers.

## ✅ FIX DEMONSTRATED — `LIMINA_KK_ALLOC_RECYCLE=1` (2026-08-09)

`kk_reset_encoder_state` (`kk_cmd_buffer.c`) normally calls `mtl_command_allocator_reset`. Under
the gate it instead **destroys and recreates** the allocator. Same boot path, same workload, and
a *longer* cycle than the baseline (60 s vs 40 s):

| closed state | unfixed | **recycle=1** |
|---|---|---|
| cycle 1 | 30 537 regions | **590** |
| cycle 2 | **59 206** regions | **1 010** |
| `AGXResource` growth over 2 cycles | +47 461 | **+972** |
| footprint | 12.8 G | 6.9 G |

**~98.5 % of the ratchet is gone**, against a harsher workload. This is simultaneously the
causation proof: the pooled resources are parked **on the allocator**, and they die with it.
`[MTL4CommandAllocator reset]` does *not* return them, even though KK calls it on every
`vkBeginCommandBuffer` (`kk_cmd_buffer.c:215`/`:225`) — which is exactly why the allocator
population stayed flat at ~414 while the leak grew linearly.

Residual: ~+420 regions and ~+0.5 GB per cycle remain. Consistent with the multi-fault structure
below — this fixes the dominant mechanism, not every accumulation.

**Still to do before this ships:** it is env-gated on purpose. Allocator churn now runs at
batch-recycle rate (hundreds/s) and **its cost has not been measured** — run the perf ladder
(`perf/README.md`) against the gate before making it unconditional, and check whether a cheaper
variant (recycle every N resets, or only when the pool has grown) buys the same reduction.

## Next steps, in order (audit-ranked, 2026-08-08)

1. **`ioclasscount | grep -iE 'agx|accel|iogpu'`, diffed closed-to-closed.** Seconds per sample
   against `vmmap`'s minutes, and a kernel class ratcheting ~10k/cycle **names itself**. Run this
   before anything else; it may end the region-identity question outright.
2. **`vmmap -v` (full listing) at fresh boot + ONE cycle** — ~26k regions, not the 160k state
   where each snapshot costs minutes. Histogram the `IOAccelerator` lines by virtual size, dirty
   size and share mode. Testable prediction: a **bimodal split** — a few hundred MB-scale regions
   carrying the ~3 GB/cycle (the +300 heaps) plus tens of thousands of small uniform regions
   (16–128 KB) carrying the count. If it holds, there are two separate faults to chase.
3. **Re-run `census-correlate.sh` ≥3 cycles reporting BO bytes** — resolve the +14-vs-+301
   contradiction above.
4. **If small regions dominate the count: DYLD-interpose `IOConnectMapMemory64` /
   `IOConnectUnmapMemory64`** in the worker, bucketing backtraces at map time. Every
   `IOAccelerator` region is minted by AGXMetal through those IOKit calls, so the backtrace names
   the Metal API and the KK/zink caller above it. This ends the guessing with a call stack.
   `leaks`/`heap`/`malloc_history` are **useless here** — kernel-established mappings never pass
   the malloc/VM interposition layer.
5. **Kill-switch A/Bs last** (`LIMINA_VREND_SHARED_IOSURFACE=0`; a pre-08-04 virgl-prefix build
   for the recency question) — cheap, but the magnitude argument already predicts the outcome.

Ranked causes: (1) **AGX/Metal device-lifetime internals under submission churn** — command
allocator chunks, per-commit kernel tracking, queue internals on the never-torn-down KK device;
region-count-shaped, invisible to any ObjC census, and consistent with the zink arm's
teardown-mediated cleanliness. (2) **Screen-lifetime BO/slab accumulation behind host zink** —
byte-shaped, matches +300 heaps/cycle. (3) The 08-05/06 scanout/import suspects, demoted above.

**Method note, twice-earned:** two counters in a row produced confident wrong readings because
they measured a proxy (a release call) rather than the thing itself (the death). Before believing
any row here, check how that class is actually released — and prefer the sentinel. The self-test
proves the **mechanism** fires; it says nothing about **coverage**, and an uninstrumented
allocation site reads identically to "nothing leaked".

## What is already ruled out

- **Not the 08-07 IOSurface scanout leak** — IOSurface counts return cleanly every cycle on both
  the worker and the supervisor.
- **Not bounded by `LIMINA_GPU_MEM_BUDGET_MIB`** — that ledger counts venus blob allocations;
  this reached 17.3 G against an 8 192 MiB default cap.

## ✅ The regions ARE MTL4 command-allocator heaps — measured, not inferred (2026-08-09)

Reading Apple's MTL4 docs turned the remaining inference into a direct measurement.
`-[MTL4CommandAllocator allocatedSize]` is documented as "the size of the internal memory heaps of
this command allocator"; `LIMINA_KK_ALLOC_STATS=<n>` (bridge `mtl_command_buffer.m`) now dumps the
distinct-allocator count and the sum/max/mean of their sizes every *n* resets.

Three open/close cycles, vrend arm, aquarium 5000 fish (`data/allocstats-cycles.txt`):

| state | regions | distinct | sum | max |
|---|---|---|---|---|
| cycle 1 closed | 61 947 | 450 | 6 475.5 MiB | 617.6 MiB |
| cycle 1 open | 73 544 | 450 | 7 762.1 MiB | 746.0 MiB |
| cycle 2 open | 87 756 | 450 | 9 355.3 MiB | 746.0 MiB |
| cycle 3 open | 102 092 | 450 | 10 980.4 MiB | 746.0 MiB |

**Bytes per region, closed→open, three independent cycles: 113.6 / 114.6 / 115.9 KiB.** A ratio
that tight, reproduced three times, settles the identity: the `IOAccelerator (graphics)` regions
**are** the command allocators' encoding heaps. That also closes the long-open "what are the 1024K
and 768K byte-carriers" question — they are allocator heap chunks.

**Hypothesis (B) is dead.** `distinct` is pinned at **450** across all three cycles (= 150
`VkCommandBuffer`s × 3 encoder states). The allocator population does not grow; the *pools* do. So
it is (A): a constant set of allocators whose heaps ratchet. The earlier "allocators flat at
360→414 while AGXResource +22 130" reading, and the tempting 54 × 410 ≈ 22 140 arithmetic that
went with it, was a coincidence — do not resurrect it.

**Scale against the documented design.** Apple's prescribed pattern is *one allocator per frame in
flight*, and Hello Triangle ships `kMaxFramesInFlight = 3`. KK runs **450**, one triple per
`VkCommandBuffer`, holding **10.98 GiB** between them — a single allocator peaked at **746 MiB**.

### ⚠ RETRACTED: "linear with no plateau"

Earlier text called this "a pure ratchet, linear, no plateau" off **two** cycles
(4 397 → 30 537 → 59 206, deltas +26 140 / +28 669). Two points cannot distinguish linear from
saturating, and a long **continuous** run now falsifies the strong form:
**+20 regions across 1 324 000 resets** while `sum` moved 6 467.6 → 6 471.6 MiB
(`data/allocstats-continuous-run.txt`). Within one workload run it **plateaus hard**, exactly as
`mtl4-repro` predicted.

The unbounded part is real but differently shaped: the high-water steps up **once per workload
launch** — +11.6k / +14.2k / +14.4k regions and ~+1.6 GiB per cycle — against the *same* 450
allocators. So the honest statement is **"bounded within a run, ratchets per workload launch"**,
not "linear forever". Anything reasoning from the old shape needs re-checking.

### ✅ CONFIRMED on better evidence — and one estimate REFUTED (adversarial review, 2026-08-09)

An adversarial review found that the mechanism-(2) verdict below rested on a weak leg: the
"anti-correlation" is really one confounded cycle (growth/violations ran 38.4, 40.0, **19.3** —
cycles 1 and 2 are *proportional*, consistent with violations driving growth at ~4.3 MiB each).
It proposed the decisive test, which the instrument's per-allocator table could already answer:
**split the bytes by whether an allocator ever took an in-flight reset** (`data/allocstats-join.txt`).

| cycle | violated_n | violated_sum | clean_n | clean_sum |
|---|---|---|---|---|
| 1 | 34 | 691.5 MiB | 344 | 2 104.2 MiB |
| 2 | 36 | 928.9 MiB | 342 | 4 892.1 MiB |
| 3 | 36 | 1 112.9 MiB | 342 | 6 279.1 MiB |

**Growth is +421.4 MiB on violated allocators against +4 174.9 MiB on clean ones — 90.8% of the
growth is on allocators that NEVER took an in-flight reset.** Violated allocators hold 15.1% of
the bytes and their count barely moves. Mechanism (1) cannot account for the growth, and the
conclusion no longer depends on the confounded cycle.

**Discharge-side self-test (review objection O3): `discharges=94 244` vs `commits=94 245`** — off
by the one commit still in flight at sample time — and `pending_now` returns to **0** at every
closed state. The completion half of the tracker demonstrably fires, so the earlier negative was
not "handlers never ran". A use-after-free in the old batch handling (freed on first fire, then
re-read on any re-invocation) is fixed: batches now live in a static ring keyed by
(index, generation). Truncation bound: commits carrying >32 command buffers go uncharged, 2 366 of
~151k = **1.57%**, far too small to move the 90.8% split.

### ⚠ REFUTED: "~9 allocators of real concurrency"

Thread sampling found only three threads in the recording path (`data/recording-concurrency.txt`)
and that was extrapolated to ~9 allocators of genuine demand against ~450 live — a claimed ~50×
over-provision. **`peak_pending` measures 184 in-flight command buffers.** Recording *threads* are
not the constraint: an allocator is held from `begin` until its work completes on the GPU, so the
floor is set by in-flight depth, not by how many threads record. The over-provision is therefore
small — single-digit multiples at most, not 50×. Do not cite the ~9 figure.

This substantially weakens the case for a pooling refactor, on top of the review's primary
objection that pooling manages *count* while the measured lever is *size*.

### ✅ SETTLED: mechanism (2). It is not a contract violation (2026-08-09)

The pending-at-reset counter ran (`data/allocstats-pending.txt`). Verdict: **KK's resets are
overwhelmingly legal, and the violations that do occur are anti-correlated with the growth.**

| cycle | region growth | violations added |
|---|---|---|
| 1 | +22 410 | 583 |
| 2 | +26 992 | +674 |
| 3 | **+14 314** | **+742** |

Cycle 3 has the **most** violations and the **least** growth — region growth varies ~2× while
violations barely move. Totals: **2 007 violations across 1 478 000 resets = 0.14%**.

The instrument self-tests: `charged=154 135, missed=0, cb_overflow=0`. Every committed command
buffer was charged to an allocator, so "few violations" cannot be "the map was empty" in disguise.
That check exists because a silent under-charge would have produced a confident, wrong negative —
the same failure mode that made `IOConnectMapMemory64` read zero earlier in this investigation.

The size histogram is the positive evidence for (2): allocators migrate **out of** the small
bucket and **into** the large ones, a different subset each launch —
`<1M` 177→153→229 while `32-128M` 6→16→20 and `>=128M` 5→12→14. Exactly "each launch drives a
different subset to a new high-water".

**So: plain documented high-water retention × burst reassignment. `reset` is behaving as
specified.** Consequences:

- `rerecord_cmd_buffer` (`kk_queue.c:35`) **is** a genuine contract violation, and it is a
  bystander for this bug. Fix it on correctness grounds if at all, not for memory.
- **"Obey the contract" fixes nothing here.** Any fix must manage pool *size*.
- The right shape is therefore **recycle past a size threshold**: bounded total, and near-zero
  churn, since only ~34 of ~405 allocators exceed 32 MiB while they carry most of the bytes.

### ✅ The size-threshold fix, measured

`LIMINA_KK_ALLOC_MAX_MIB=<n>` (kk `7e1dfe68363`) recycles an allocator only once its
`allocatedSize` passes the threshold; everything below stays on the plain `reset`. At **32 MiB**,
same three-cycle aquarium workload, cold boot both arms:

| | unfixed | `MAX_MIB=32` |
|---|---|---|
| regions (c3 open) | 67 760 | **17 642** (−74%) |
| allocator `sum` | 7 180.5 MiB | **1 422.1 MiB** (−80%) |
| largest allocator | 731.4 MiB | **26.7 MiB** (−96%) |
| cycle-3 growth | +14 314 | **+1 554** (−89%) |
| `IOAccelerator (graphics)` | — | **1.8 G** |
| hist `32-128M` / `>=128M` | 20 / 14 | **0 / 0** |

Per-cycle growth **decelerates** under the cap (+8 674 → +4 346 → +1 554), which is the plateau
the unfixed arm never reaches.

⚠ **Not yet a default, and the threshold is not tuned.** Recycle-on-every-reset
(`LIMINA_KK_ALLOC_RECYCLE`) reaches ~1 010 regions, an order of magnitude below the 32 MiB cap, so
this trades memory against churn and neither cost is measured. A lower threshold would recycle the
70 allocators now sitting in the 8–32M bucket. Perf ladder before either becomes default — task
\#13.

**Amendment to "(B) is dead".** Run A had a saturated population (`distinct` pinned at 450 while
`sum` went 6 475→10 980 MiB), which does prove pools grow on their own. But run B, from a cold
boot, shows `distinct` climbing 261→279→405. So population growth is real in the *early* phase and
**saturates** near ~450; pool growth continues indefinitely after that. (A) is the driver; (B) is
a bounded early contributor, not dead — the earlier flat statement was too strong.

### The mechanism: two survivors, one discriminator (SUPERSEDED — kept for the reasoning)

If `reset` genuinely recycled, cycle 2 would reuse cycle 1's heaps and the sum would stay put. It
does within a run and does not across launches. Two mechanisms explain **all** of the above, and
nothing measured yet separates them. Do not headline either.

1. **Reset-while-in-flight.** Apple: *"You are responsible to ensure that all command buffers with
   memory originating from this allocator instance are complete before calling resetting it."* A
   reset landing on in-flight work leaves heaps unrecyclable, so fresh ones get allocated. Steady
   state has stable timing and recycles fine; workload start/teardown bursts race.
   `rerecord_cmd_buffer` (`kk_queue.c:35`) calls `kk_reset_cmd_buffer_internal` inside
   `kk_queue_submit` with no completion wait, and is the one identified violation site.
2. **Plain high-water × burst reassignment — no contract violation at all.** `reset` retains
   high-water by design (`mtl4-repro`: `RESET_EVERY=50` holds +911 permanently while flat). Which
   of the 450 allocators catches a launch burst is arbitrary, so each launch drives a *different
   subset* to its own new high-water. This predicts everything observed: plateau within a run
   (stable assignment), ~constant +1.6 GiB per launch against the same 450 allocators, `max`
   stepping once then flat (per-allocator ceiling = biggest single burst), `mean` climbing.
   Practical ceiling ≈ 450 × 746 MiB.

**(2) is arguably the better-supported of the two right now**, because the doc reading established
that zink is fence-legal and `signalEvent` is completion-scoped — so it is unclear where ~14k
in-flight resets per launch would come from. Under (2), `rerecord_cmd_buffer` is a bystander and
"obey the contract" fixes nothing.

**Discriminator, one boot, one number:** in the bridge, record at commit which allocators the
committed command buffers were begun on, mark them complete in the feedback handler, and count
`pending > 0` at reset. Violations at launch magnitude → (1). Zero violations while `sum` still
steps per launch → (2), proven. Supporting read: dump a per-allocator size histogram each report —
under (2) individual allocators step up monotonically at launches and never move otherwise.

### ⚠ Instrument caveats

- **The hash never unregisters destroyed allocators.** So `distinct=450` cannot by itself
  distinguish a stable population from churn onto recycled pointers, and `sum` would include stale
  corpses. Other data rules churn out here (regions do not dip at close, and destroy demonstrably
  frees), but the caveat travels with the instrument.
- **450 = 150 `VkCommandBuffer`s × 3 encoder states is arithmetic, not a count.** The 3-per-cmdbuf
  factor is read from `kk_create_cmd_buffer`; the 150 is inferred by division and has not been
  verified against a live command-buffer census.
- **The 113 KiB/region ratio is delta/delta**, from region *counts* against `allocatedSize` bytes.
  Sealed in absolute terms too: at one instant, `IOAccelerator (graphics)` = **11.5 G** virtual
  across 102 099 regions against `sum` = **10 980.4 MiB (10.72 GiB)**, i.e. the command allocators
  account for ~93% of the tag. The remaining ~7% is the co-travelling population tracked elsewhere
  in this file.

## ⚠ The zink A/B does NOT exonerate KK/Metal or virglrenderer core (audit, 2026-08-08)

`memcycle-zink.sh` returns every byte (159 973 → 176 345 → 159 911, footprint back to 24.0 G
exactly), and that was read as "the fault is vrend-specific, not KK/Metal/virgl core". **That
inference has a confound: the two arms differ in TEARDOWN, not only in layer.**

- In the **zink arm**, guest zink owns a VkInstance/VkDevice, so Firefox exit destroys the whole
  host Vulkan device — `vkr_context_destroy` → `vkr_instance_destroy` → `vk->DestroyDevice`
  (`src/venus/vkr_context.c:985`, `src/venus/vkr_device.c:358`). Every KK pool, queue, command
  allocator, cached BO and AGX kernel object behind it dies **at every close**.
- In the **vrend arm**, nothing of the sort happens. vrend's host GL stack is created once per
  worker (`vrend_renderer_init`, `src/vrend/vrend_renderer.c:7628`, minting `ctx0` at `:7764`)
  and `ctx0` is destroyed only at `vrend_renderer_fini` (`:7831`, destroy at `:7847`) or
  `vrend_renderer_reset` (`:13874`, which recreates it two lines later) — **neither runs on
  Firefox exit**. Firefox exit destroys GL contexts on a screen and a VkDevice that live forever.

**The open-state sample proves the venus arm accumulates too, and it was in the data all along.**
`memcycle-zink.sh` snaps at *open* as well as closed, and that arm reads
**159 973 closed → 176 345 open → 159 911 closed**: **+16 372 regions while Firefox runs**, all of
them returned at exit. So the venus tier was never clean — it ratchets at the same order of
magnitude and is then *rescued* by teardown. (Not a controlled rate comparison: the two arms ran
in different sessions off very different baselines — 159 973 vs 4 397 — so "same order of
magnitude" is the strongest honest claim.)

That reframes the whole A/B. venus does **not** bypass the command allocators: there is one host
Vulkan driver, and guest-Vulkan → vkr → KosmicKrisp lands in the same `kk_cmd_buffer.c`, the same
`mtl_new_command_allocator`, three per `VkCommandBuffer`. The arms differ only in whether anything
ever *destroys* those allocators. `DestroyDevice` is the same event as `LIMINA_KK_ALLOC_RECYCLE`,
at once-per-workload instead of once-per-reset — so the zink arm has been running an unintentional
positive control for the fix since before the fix existed, and its cleanliness is now **predicted
by** the allocator hypothesis rather than merely compatible with it.

The correct statement is therefore **not** "vrend leaks, venus does not", but **"both tiers park
resources on the KK command allocators; only the venus arm has something that destroys them."**

So **any device-lifetime accumulation** — in KK, in host zink's BO cache, or in AGX kernel state
— produces exactly this signature: ratchets under vrend, "returns everything" under zink. The
zink arm's cleanliness is *teardown-mediated* and exonerates no layer. The honest scoping is:
**the leak lives in state owned by the persistent host-GL stack (vrend + host zink + the one KK
device they share), and only workload-shaped GL churn drives it.**

The 08-05/06 suspects (EGLImage scanout, classic-gbm venus import) are **demoted**: they mint
tens of IOSurfaces/EGLImages per cycle and their destroy paths verifiably pair
(`vrend_renderer.c:9229`), three orders of magnitude short of ~10–29k regions per cycle.
`LIMINA_VREND_SHARED_IOSURFACE=0` (`vrend_renderer.c:8765`) is still a one-boot kill-switch A/B,
but the magnitude argument predicts it changes nothing.

## ⚠ "Regression" is an unmeasured premise

The fault was **found** on 2026-08-08; nothing establishes it was **introduced** then. There are
no region/footprint measurements anywhere before that date — the only earlier `vmmap` use
(`perf/2026-07-27-replay-regression-ab.md:38`) is dylib-identity checking. The ratchet could
equally date from the July zero-copy scanout work, or from vrend-GL-by-default itself.

## ✅ Releasing an allocator returns 100% of its heaps (2026-08-09, `mtl4-repro/destroy-probe.m`)

The shipped pool **bounds** memory but never **returns** it — nothing is destroyed before
`vkDestroyDevice`. That gate only exists on one tier:

| tier | who creates the host KK VkDevice | who destroys it |
|---|---|---|
| **venus** | vkr, per *guest* `vkCreateDevice` | the guest app's `vkDestroyDevice` → `kk_alloc_pool_finish` (`kk_device.c:576`) |
| **vrend** | host zink's *screen*, at `virgl_egl_init` | `eglTerminate` (`vrend_winsys_egl.c:457`) ← `virgl_renderer_cleanup` ← rutabaga drop — i.e. **worker process exit** |

`vrend_winsys_destroy_context` only destroys per-context EGL contexts, never the display. So on
vrend nothing returns the ~1 GiB steady-state pool between workloads. `vkTrimCommandPool` is the
API-blessed gate and is dead twice over: zink never calls it, *and* it is per-command-pool while
the allocator pool is device-global.

The proposed fix — destroy a `draining && pending == 0` allocator instead of resetting it — rested
on one unverified assumption. **It holds:**

| mode | tests | result |
|---|---|---|
| `a` staged teardown | cbs → allocators → queue | +232.2 MiB / +1907 regions grown, **100% back** (1925 → 5 regions) |
| `b` real KK ordering | allocators released, completed cbs still alive | **100% back**, within 1 s |
| `c` **control** | `reset()` only | **0% back** — regions stay at 1925 |
| `d` 4 regrow cycles | cache masquerade | returns to baseline *every* cycle, regrows the same |
| `big` | one 125 MiB allocator | **99% back** |

The control is what makes the rest readable: same instrument, same workload, 0% vs 100%.

**Load-bearing result — mode `b`.** The allocator returns everything *while its completed command
buffers are still alive*. Command-buffer lifetime is irrelevant, so the destroy point needs no
hook in `kk_cmd_release_resources`. Releasing all 96 cbs moves `ioaccel` by **0.0 MiB**: the
allocators own every heap region, the cbs own none.

### ⚠ RETRACTED: "committed command buffers are never deallocated"

The first run read 0/96 and 0/384 sentinel deallocs and concluded the queue retains committed cbs
for its lifetime (~5 KiB each). **That was the probe's own reference leak**: `encode_pass` did
`[[g_dev newCommandBuffer] retain]`, and `new` already returns +1. The cbs could not die. Fixed,
they dealloc promptly (98/96 — the extras are `flush_roundtrip`'s). The queue retains nothing;
after queue release AGXResource returns to +0.

The disproof was **already in the record, unused**: `flush_roundtrip` cbs go through the full
release path and are excluded from the sentinel denominators, so a balanced probe must read
`dead_cbs >= 2` in *every* mode. It read 0 everywhere — including MODE=c, where nothing else was
ever released. A denominator that is impossible on its face is a bug in the instrument, not a
finding about the system.

Two lessons worth carrying: the sentinels earned their place by catching this at all (a memory
number alone would have been read as Metal behaviour), and the first analysis attributed "the
queue holds them" from a **footprint delta alone**, never sentinel-testing it — the corrected
probe now reports sentinels after queue release too.

### What this does NOT model

- Release while the queue is actively executing *other* allocators' work (the probe drains first;
  KK will destroy mid-frame). Correctness should hold by refcounting; promptness of the kernel
  unmap under load is unverified.
- Multithreaded encode/submit concurrent with the release.

Neither changes the verdict. Thresholds (`>=80%` build / `<=20%` dead) were fixed in the probe
source **before** the first run.
