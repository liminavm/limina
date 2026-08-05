# venus command-stream overhead decomposition — 2026-07-28

Question (from the tier battery: virgl beat venus-GL at every measured point, and the
user is building a Vulkan-only compositor): where does a **native Vulkan** command
stream actually spend its time crossing the venus boundary, with no zink in the loop —
and which parts are load-bearing serdes vs our own overhead?

## Vehicle

`drawstorm.c` — portable Vulkan C probe: N tiny triangles/frame into an offscreen
1024x1024 target, one `vkCmdPushConstants` + `vkCmdDraw` per triangle, command buffer
re-recorded every frame, one submit + fence wait per frame. Times the three app-visible
sections per frame: record (`vkCmd*` loop), submit (`vkQueueSubmit`), fence
(`vkWaitForFences`). Builds on the Linux guest (venus ICD) and on macOS against KK
directly (`VK_ICD_FILENAMES` → kosmickrisp devenv json) — the same binary semantics on
both sides makes the virtualization tax a direct A/B.

Guest: F44 enhanced probe clone, 4 vCPU, EFI+venus boot, run over ssh with
`VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json`. Host: M1 Max direct.

## Baseline numbers (before fixes)

per-frame ms at n draws (record / submit / fence / total):

| n | host-native KK | guest venus |
|---|---|---|
| 1 | 0.001 / 0.022 / 0.258 / **0.281** | 0.005 / 0.001 / 0.356 / **0.361** |
| 100 | 0.004 / 0.035 / 0.291 / **0.330** | 0.009 / 0.001 / 0.445 / **0.455** |
| 1000 | 0.024 / 0.152 / 0.339 / **0.515** | 0.060 / 0.001 / 1.004 / **1.065** |
| 10000 | 0.243 / 2.105 / 1.329 / **3.676** | 0.407 / 0.002 / 8.561 / **8.970** |

Reading:
- **Guest-side venus encode is cheap**: 0.407ms vs 0.243ms native record at 10k draws
  (~40ns/draw-pair marginal). Guest `vkQueueSubmit` returns immediately (async ring).
- **The whole venus tax is host-side**, visible in the fence wait: +5.29ms/frame at
  10k draws (8.97 vs 3.68 total, 2.4x). Single-frame fixed overhead (n=1): only
  +0.08ms — matches the known ~0.08ms host round trip.
- Host-native's own per-draw cost is the **Metal replay at submit** (~0.21µs/draw):
  KK records vkCmd* into Mesa's `vk_cmd_enqueue` deferred list and replays at
  `kk_queue_submit` → `vk_cmd_queue_execute` (kk_queue.c).

## Host-side split (sample of limina-vmm during a 10k-draw storm)

vkr-ring thread, 2137 snapshots, 63% busy (1337): `vn_dispatch_vkQueueSubmit` →
KK replay+Metal = 527 (matches native submit cost); `vn_dispatch_vkExecuteCommandStreamsMESA`
(the decode loop) = 810, of which:

- **~55% journal machinery** (`vkr_journal_post_dispatch` 369 + pre 35 + note_lookup
  ~40): per-command calloc(msg) + malloc(payload) + malloc(keys) + memcpy + mutex push
  to the retention thread + TLS traffic. The two-lane fast path only covered
  TRANSIENT commands — RECORDING (vkCmd*) took the full capture path per command.
- **~19% a per-draw `getenv("GKVM_KK_RTLOG")`** (151 samples in `__findenv_locked`) —
  KK bring-up debug leftover in `vkr_dispatch_vkCmdDraw`.
- **~10% handle-lookup mutex traffic** (`vn_cs_decoder_lookup_object`: object-table
  pthread mutex per handle, plus journal TLS note per hit).
- **~6% the actual KK enqueue-record** (`vk_cmd_enqueue_*`), i.e. the legitimate work.

KK-side finds (for phase 2): `kk_upload_descriptor_root` memmoves the full descriptor
root per draw (123 samples); descriptor-pool BO allocation goes through
`mtl_new_heap` + heap dealloc **per frame** with IOGPU kernel round trips (66+12).

## Fixes applied (virgl fork)

1. `vkr_kk_rtlog()` cached helper (vkr_common.h) replacing raw getenv at all
   GKVM_KK_RTLOG sites.
2. Journal decode-lane cost: inline small payloads (≤96B, covers every hot vkCmd*)
   and single keys into the msg (one calloc per retained command instead of three
   mallocs; consumer materializes the heap copy), plus per-batch TLS message
   batching — one queue lock+signal per ring/context submit batch instead of per
   command (`vkr_journal_batch_begin/flush` bracketing both submit_cmd loops).
   Correctness: **only RECORDING-class inserts are batchable** — anything else
   drains the batch and pushes immediately. Two cross-thread causal edges proved
   this the hard way: (a) a CREATE is pinned from the virtqueue worker the moment
   the guest learns it was consumed (naive batching → 2 pin MISSes at session
   bring-up → alloc entries pruned); (b) a RING_CREATE decoded on the context
   thread starts a ring thread whose first journaled command (reply-stream set)
   must queue after it (create-only draining → the suite's snapshot-restore test
   serialized the RING_STREAM entry before its ring's create → load-bearing drop
   at restore → KK descriptor assert). RECORDING entries have no cross-thread
   dependents in valid Vulkan usage, so per-thread program order suffices.

## After

| n | baseline | 0054 (msg-inline+batch) | 0055 (block lane) | +kk 0015 (BO cache) | +0056 (lookup gen-cache) |
|---|---|---|---|---|---|
| 1000 | 1.065 ms | 0.91-0.95 ms | 0.924 ms | — | 0.818 ms |
| 10000 | 8.970 ms (111.5 fps) | 6.98 ms (143 fps) | 6.67 ms (150 fps) | 5.92 ms (169 fps) | **5.35-5.7 ms (175-187 fps)** |

(0054/0055 numbers from fresh-boot sessions; the settled-session 0054 run read 6.60.)
Venus host tax at 10k draws: **+5.29ms → ~+3.0ms (−43%)**. virgl 0055 replaced the
per-command message entirely: pure vkCmd* captures append into a 256KB thread-local
block (one memcpy, no alloc, no lock), one message per block, consumer batch-pops.
kk 0015 (cmd-pool BO cache default 512) then took host-native 3.68 → 2.56 ms and
guest 6.67 → 5.92. virgl 0056 (object-lookup generation cache: 4-entry per-decoder
cache gated on a ctx-wide table generation, bumped under the object mutex on every
insert/remove) removed the last per-handle mutex traffic: guest 5.92 → 5.35-5.7 ms
(175-187 fps), 1k draws 0.924 → 0.818 ms. Post-0056 profile: zero mutex/hash-search
under `vn_cs_decoder_lookup_object`; the decode lane is now vk_cmd_enqueue_* (real
KK record) + the journal note_lookup TLS append. **Cumulative: 8.97 → ~5.5 ms, venus
tax over native +5.29 → ~+2.9 ms with native itself 44% faster** — see
docs/perf/venus-cmdstream-overhead.md for what (little) remains.

## 2026-07-29: frames-in-flight axis (`-i`) + KK threaded submit (kk 0017)

Two findings, one fix.

**1. The instrumentation-tax trap (again).** The first fresh profile of the
storm showed `kk_draw → getenv` at 10.6% of wall — yesterday's *uncommitted*
PBO-hunt instrumentation (`LIMINA_KK_DRAWPROBE`) still applied in the mesa-cs
tree, taxing every draw in the deployed KK, host references included. Reverted
(snapshot preserved as `spikes/crossmark/kk-pbo-trace-instrumentation.patch`),
both builds redone pristine. Clean re-baseline: host 10k i=1 3.06→2.70 ms,
i=2 1.89→1.55; guest i=1 6.59→6.12, i=2 2.91→2.30. Lesson repeated: profile
with a *pristine* tree, and grep the profile for `getenv`/`findenv` first.

**2. The pipelined gap was a thread-topology artifact.** New `-i N`
(frames in flight; 1 = the original fully-serialized probe). Both ends
saturate at 2-deep pre-fix: the ring thread runs decode+enqueue (~0.9 ms) and
the vkQueueSubmit Metal replay (~1.3-1.4 ms) *serialized*, so pipelined venus
threw ~2.30 ms/frame against host-native 1.55 (1.49x). Fix = **kk 0017**:
`VK_QUEUE_SUBMIT_MODE_THREADED` (LIMINA_KK_SUBMIT_THREAD, default on) + the
move-capable native binary sync type it requires (dzn-pattern shared-event
swap; sync_file shims so zink's SYNC_FD-export semaphores still select it).
The replay moves to the vk_queue submit thread; the ring thread goes back to
decoding the next frame.

10k draws + push constants, 300 frames, per-frame ms (fps):

| in-flight | guest venus (immediate) | guest venus (threaded) | host KK (threaded) |
|---|---|---|---|
| 1 | 6.12 (163) | 6.06 (165) | 2.74 (365) |
| 2 | 2.30 (434) | 2.60 (385) | 1.52 (656) |
| 3 | 2.30 (431) | **1.39 (717)** | 1.52 (—) |

- **The pipelined venus tax is GONE**: at 3-deep the guest throughput
  (1.39 ms) sits at the host-native replay floor (submit-thread replay
  ~1.3 ms is the whole pipeline's bottleneck stage, wherever it runs).
  Saturation moved 2-deep → 3-deep (three overlapped stages now: guest
  encode | ring decode | Metal replay).
- Host-native is unchanged at saturation (1.52-1.55) and its vkQueueSubmit
  call drops to ~4 µs (fence absorbs the replay).
- The serialized (i=1) path is unchanged — its ~6 ms is latency, not
  throughput; the remaining levers there are the wake chain (sync path) and
  Phase-3 fused decode (docs/perf/venus-cmdstream-overhead.md).
- i=2 threaded reads ~0.3 ms worse than immediate (2.60 vs 2.30): with three
  stages, two frames in flight can't fill the pipe and the extra handoff
  shows. Real draw-heavy apps run 2-3+; compositors are 1-in-flight and
  unaffected.
- Correctness gates: guest crossmark pixel hashes bit-match all cross-tier
  references; vkmark 2778 (post-arc median was 2674); full HVF suite run
  with the threaded KK — see the ledger.

## 2026-08-05: the threaded-submit pair on the MTL4 tip — premise re-measured, pair RETIRED

The 2026-08-05 KK rebase removed kk 0017+0018 from `limina-kk`: upstream's live-record rework
(!42621, `0cd84d45`) moved encoding to vkCmd* record time, deleting the "vkQueueSubmit replays
the whole command stream on the ring thread" premise the pair existed for. Before deciding
retire-vs-re-derive, the remaining submit tax was measured on the tip
(`a3df3aae`, Vulkan 1.4 / Mesa 26.3.0-devel; artifacts in `mtl4-resubmit/`).

Rig: fresh clone of `Fedora-Workstation-44.enhanced.raw`, seated EFI+venus, 6 vcpu,
`check-gpu-context-health.sh` clean, `ab-vkmark.sh` (3× vkmark 1280x720 in the seated
session), 10 s `sample` of the worker mid-run-1 and mid-run-3.

**vkmark: 3259 / 3383 / 3320** (median 3320, spread ±2%). No comparable pre-rebase baseline
exists (the June 2778 was a different guest image + 4 vcpu — cross-image scores are
incomparable, see the perf-display-pinning trap), so the scores serve as the tip reference for
any future A/B, not as a regression verdict.

**vkr-ring-4 thread profile during vkmark (5587 snapshots / 10 s, run 1; run 3 agrees):**

| where | samples | share |
|---|---|---|
| relax nanosleep (idle) | 3826 | 68.5% |
| dispatch → vkQueueSubmit | 611 | 10.9% |
| dispatch → other venus cmds (ImportSemaphore 139, CreatePipelines 64, …) | 292 | 5.2% |
| ring poll/read (+572 et al.) | ~860 | 15.4% |

The old premise is dead: the ring thread idles two-thirds of the time under vkmark and submit
is ~11% of its wall time (it was the dominant cost pre-live-record). BUT a residue is real and
worth knowing: inside `kk_queue_submit`, ~180 of the 611 submit samples sit in
`vk_cmd_queue_execute` replaying `vk_common_CmdBeginRenderPass` → `kk_CmdBeginRendering` →
`mtl_new_render_command_encoder_with_descriptor` — **KK MTL4 still creates Metal render
encoders at submit time for classic-render-pass apps** (the vk_common render-pass translation
records into the common command queue; dynamic-rendering apps skip this). vkmark uses classic
render passes, so this is that path's worst case, and it is ~3% of ring-thread wall time.

Revival cost, measured by merge-tree dry-run: both commits CONFLICT on the tip
(`kk_device.c`, `kk_queue.c`, `kk_sync.c` — upstream's Metal4/hang-detection/drawable-wait
churn), so leg B would be a re-derivation of the sync machinery, not a cherry-pick.

**Verdict: RETIRED.** Offloading ~11% of a 68%-idle thread cannot move a GPU-bound workload,
and the conflict surface prices a re-derivation far above the bounded win. The residual
submit-time encoder creation is latency-class (wake-chain/fusion territory, still parked), not
thread-offload territory. If it ever resurfaces, the pair stays revivable from tag
`limina-kk-2026-08-05-pre-mtl4-rebase` (70ead9445fe + d2aeced7eb6) — but any upstream-shaped
fix should be a new design against the MTL4 queue, or better: teach the vk_common render-pass
path to record dynamic-rendering directly (kills the replay at the source, upstreamable).
