# Venus command-stream overhead: decomposition and plan of attack

2026-07-28. Context: the tier battery showed virgl/vrend beating zink-on-venus GL at
every measured point (glmark2 2.4x, aquarium 57 vs 53 fps @10k fish), and the user is
building a **Vulkan-only compositor** — so venus, not vrend, must become the fast
path. The question this memo answers: where does a native Vulkan command stream
actually spend its time crossing the venus boundary, and what do we attack, in what
order?

Method + raw numbers: `spikes/venus-cmdstream-probe/` (drawstorm, guest-vs-host A/B
of the same binary, worker `sample` decomposition). Key headline:

> At 10k draws/frame the venus tax was **+5.29ms/frame over host-native KK**
> (8.97 vs 3.68ms), ~all host-side. Less than a third of it was load-bearing
> decode work; the rest was our own journal machinery, a leftover per-draw
> getenv, and lock traffic.

## What the boundary actually costs (per command, host side)

The chain per `vkCmd*`: ring read → `vn_dispatch_command` (journal pre) → decode args
+ object lookups (mutex per handle) → KK entrypoint = `vk_cmd_enqueue_*` (generic
deferred record, ~24ns — cheap) → journal post (capture+push) … then at
`vkQueueSubmit`: `vk_cmd_queue_execute` replays the recorded list into Metal encoding
(~0.21µs/draw — the same cost host-native apps pay).

**Not the story:** venus serialization/deserialization proper. Guest encode is
~40ns/draw-pair; wire decode is a small slice of the ring thread. A virgl-style
"coarser protocol" would buy little — the protocol isn't where the time goes.

## Phase 1 — decode-lane parasites (DONE 2026-07-28, virgl fork)

1. **getenv per draw** (`GKVM_KK_RTLOG` bring-up logging in `vkr_dispatch_vkCmdDraw`
   et al): ~19% of decode. → cached `vkr_kk_rtlog()` (vkr_common.h).
2. **Journal recording-lane capture** (~55% of decode at 20k cmds/frame): per-command
   3× malloc + memcpy + mutex push to the retention thread. → msg-inline payloads
   (≤96B) and keys (one calloc per retained command, heap copy materialized on the
   consumer), TLS per-batch push batching (one lock+signal per ring submit batch).
   **Only RECORDING-class inserts batch**; everything else drains the batch and
   pushes immediately. Two cross-thread causal edges forced that rule, both caught
   empirically: a CREATE gets pinned from the virtqueue worker the moment the guest
   sees it consumed (naive batching → pin MISSes at session bring-up), and a
   RING_CREATE decoded on the context thread must be queue-visible before the new
   ring thread's first journaled command (create-only draining → the suite's
   snapshot-restore test hit a load-bearing RING_STREAM entry serialized before its
   ring's create). vkCmd* entries have no cross-thread dependents in valid usage,
   so per-thread program order — which the batch preserves — is enough for them.

3. **Journal recording block lane** (virgl 0055, same day): the per-command message
   itself went away. Pure `vkCmd*` captures append into a fixed 256KB thread-local
   linear block — one memcpy, zero allocation, zero locks — shipped to the consumer
   as ONE message on fill / order-boundary / batch end. ~20k calloc+mutex round
   trips per storm frame became ~5. The consumer applies a block under one
   `j->mutex` hold and batch-pops the whole queue under one lock. This is the
   ring-buffer idea executed with the ordering constraint kept structural: a
   per-thread ring would re-break the single global apply order (the same
   cross-thread causal edges that killed naive batching), so the block only ever
   holds RECORDING captures and everything else still serializes through the queue
   in real time.

Result: 10k-draw frame 8.97 → 6.67ms (+34% fps fresh-boot); venus tax over native
−43%. The worker profile now shows the decode lane clear of journal and allocator
cost — the residue is the object-table lookup mutex (Phase 2 item 3) and the KK
submit path. Validation: HVF suite (snapshot/replay correctness rides on the
journal).

## Phase 2 — the double-handling and the locks

1. **DONE (kk 0015, 2026-07-28): KK cmd-pool BO churn.** The prior LIMINA_KK_BOCACHE
   analysis was right but left opt-in — with the 32-BO default cap, a 10k-draw frame
   created+destroyed ~300 kk_bos (each a full MTLHeap = IOGPU kernel round trip at
   draw time + residency churn). Defaulted the cache to 512. **Host-native 3.68 →
   2.56ms (+44% fps); guest 6.67 → 5.92ms; `mtl_new_heap` vanishes from the storm
   profile.** Biggest single win of the whole arc, and it helps host-native apps too.
2. **Mostly moot after 1: descriptor-root memmove** — with slimroot already ON and
   the churn gone, `kk_upload_descriptor_root` is ~2% of the storm profile; the
   per-draw root snapshot is semantically required when push constants change per
   draw. No further action unless a real workload shows it.
3. **DONE (virgl 0056, 2026-07-28): object-lookup mutex per handle** (~10% of
   decode, was the top decode residue): every command in a recording stream looks
   up the same VkCommandBuffer (and layout) under the ctx object-table mutex.
   Fixed with a **4-entry per-decoder cache** gated by a table generation counter
   (`ctx->object_gen`, bumped under object_mutex on every insert/remove, read
   relaxed on the fast path — staleness only diverts to the locked path, and a
   use of a just-created/reused id is ordered after its create's gen bump by the
   ring transport itself; entries are tagged with the generation captured *under*
   the mutex so a tag can never be newer than its lookup). 4 entries, not 1,
   because vkCmdPushConstants alternates cmd-buffer and layout lookups — a
   1-entry cache would thrash every command. Result: guest 10k-draw 5.92 →
   5.35-5.7 ms; the storm profile shows zero mutex/hash-search traffic under
   `vn_cs_decoder_lookup_object`; the residue inside lookup is now the journal's
   note_lookup TLS append.
4. **Journal residue**: gone (virgl 0055 block lane); nothing further indicated.

## Phase 3 — structural (only if 1+2 leave a real gap)

**Fused decode**: teach the vkr→KK boundary to skip the `vk_cmd_enqueue` middle copy —
decode venus wire straight into KK's internal command representation (we own both
sides). Eliminates one full record pass (~6% of decode) plus allocator traffic, but
couples vkr to KK internals and complicates the two-tier story (vkr must keep the
generic path for any non-KK host driver). The measured ceiling: host-native itself
pays 2.1ms/10k-draw submit in the Metal replay — fusion cannot beat that floor, so
its upside is bounded by the enqueue+decode slice, not the whole gap. Decide after
Phase 2 re-measurement.

**Not planned**: protocol coarsening (gallium-style stateful protocol for Vulkan) —
wrong shape, loses Vulkan semantics, and the measurements say serdes isn't the cost.

## Phase 2.5 — the pipelined gap was thread topology, not decode cost (DONE 2026-07-29, kk 0017)

The frames-in-flight axis (drawstorm `-i N`, added 2026-07-29) reframed the residual
2x: at 2+ frames in flight the guest encode fully overlaps, and the bottleneck is the
vkr ring thread running decode (~0.9 ms) and the vkQueueSubmit Metal replay
(~1.3-1.4 ms) back-to-back on one thread. **kk 0017** moves the replay to mesa's
vk_queue submit thread (`VK_QUEUE_SUBMIT_MODE_THREADED`, `LIMINA_KK_SUBMIT_THREAD`
default on), which required a move-capable native binary sync type (dzn-pattern
shared-event swap + sync_file shims — see the patch). Result at 10k draws:

| in-flight | guest venus | host KK native |
|---|---|---|
| 1 | 6.06 ms (165 fps) | 2.74 (365) |
| 2 | 2.60 (385) | 1.52 (656) |
| 3 | **1.39 (717)** | 1.52 (—) |

**The pipelined venus tax is gone** — the guest saturates at the host's own
submit-thread replay floor; the boundary costs nothing at throughput. Saturation
moved to 3-deep (three overlapped stages). Correctness: crossmark hashes bit-match,
vkmark 2778, HVF suite 70/70. Caveats: the serialized (1-in-flight) path is
unchanged — that ~6 ms is latency (wake chain + serial decode+replay), where Phase 3
fusion remains the only cmdstream-side lever; and exactly-2-in-flight reads ~0.3 ms
worse than immediate (can't fill three stages). En route the fresh profile also
caught `LIMINA_KK_DRAWPROBE`'s per-draw getenv still applied uncommitted in the
mesa-cs tree, taxing every KK user 10% — reverted; numbers above are pristine-tree.

## For the compositor specifically

A compositor frame is few-draws/one-submit/one-present: its costs are the **sync
path** (fence-present, wake-chain — separate backlogs, largely done or documented),
not per-command throughput. This memo's work matters for the *apps under* the
compositor (games, draw-heavy Vulkan): with Phase 1, venus now sustains ~1.5M
draw+push pairs/sec through the boundary; Phase 2 targets ~2x native-gap closure.

## Ledger anchors

- drawstorm before/after: spikes/venus-cmdstream-probe/RESULTS.md
- tier battery that motivated this: perf/2026-07-28-tier-battery.md
- related: docs/perf/present-misses.md §19-20, limina-venus-wake-chain memory
