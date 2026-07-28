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

| n | baseline | 0054 (msg-inline+batch) | 0055 (block lane) |
|---|---|---|---|
| 1000 | 1.065 ms | 0.91-0.95 ms | 0.924 ms |
| 10000 | 8.970 ms (111.5 fps) | 6.98 ms (143 fps) | **6.67 ms (150 fps)** |

(0054/0055 numbers from fresh-boot sessions; the settled-session 0054 run read 6.60.)
Venus host tax at 10k draws: **+5.29ms → ~+3.0ms (−43%)**. virgl 0055 replaced the
per-command message entirely: pure vkCmd* captures append into a 256KB thread-local
block (one memcpy, no alloc, no lock), one message per block, consumer batch-pops.
The post-0055 profile shows the decode lane clear of journal/allocator cost; the
remaining gap = KK descriptor-root memmove + descriptor-pool Metal-heap churn (now
the top host cost) + the object-table lookup mutex — see
docs/perf/venus-cmdstream-overhead.md for the ranked backlog.
