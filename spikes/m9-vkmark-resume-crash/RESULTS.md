# vkmark crash on resume (dogfood, 2026-07-20) — FIXED (virglrenderer 0040)

**FIXED same day: virglrenderer 0040 (journal create-arg closure).** RED/GREEN via the new
`vkpipeline.py` leg in the `venus_session_preserved` gate: a live compute pipeline whose
shader module + layout were destroyed at create, heartbeating dispatches — SIGABRT at the
first post-restore beat on pre-fix virgl (the exact vkmark death, coredump and all), survives
both restore generations post-fix. Design write-up: docs/design/venus-snapshot-replay.md
§"The vkmark-on-resume crash FIXED".

**ROOT CAUSE (2026-07-20, from this log alone — see §Root cause below): the venus
re-creation journal is not transitively closed over create-argument references.**
vkmark destroyed its shader modules after pipeline creation (standard, legal Vulkan);
the modules' destroys pruned their create entries; at restore, the two retained
`vkCreateGraphicsPipelines` entries (pipelines 454/457) failed their shader-module
lookups (452/453/455/456) and were dropped — the histogram's exactly-2 "create" drops.
After `replay complete` (FATAL sticky again), a parked live ring command referenced
pipeline 454 → ring FATAL → ring thread exit → guest-visible
`VK_RING_STATUS_FATAL_BIT_MESA` → vkmark's next submit aborts (`vn_ring_submit_internal`
aborts on the status bit — the coredump's `vn_ring_submit_locked` frame).
Fix direction: pin create-arg references (the journal's existing pin/deferred-prune
machinery, today used only for blob←VkDeviceMemory, generalized to every CREATE
entry's decoded handle refs — single hook site: `vkr_cs_decoder_lookup_object` via
`vn_protocol_renderer_cs.h:83`).

User report: vkmark was running (mid-benchmark) during a suspend/resume on dogfood-mac/dogfood-guest;
on resume it crashed. Firefox was also open. **Worker survived; GNOME session survived;
Firefox survived (its venus context died but Firefox tolerates context loss); vkmark
aborted** — the per-context poison containment (virgl 0026-0028/0031) worked as designed.

Build under test: the 14:41 dogfood deploy — **debug profile**, but current master
(includes libkrun 0086 gen-2 fence rebase and all M9.3/M9.4 patches).

## Timeline (guest clock, -03)

- 14:44 guest boots (fresh session post-deploy). User opens Firefox (venus ctx 6),
  vkmark (venus ctx 14, mid-benchmark), then suspends.
- 15:07:48 `PM: suspend entry (s2idle)` — the M9 bracket. Snapshot taken, worker exits 126.
- 15:08:28 user relaunches; auto-resume arms (`snapshot.bin` → consumed at 15:08).
- 15:08:45 **restore replay** runs in the fresh worker. The storm begins (host log):
  - `vkr: failed to look up object <N> of type <T>` — hundreds of distinct objects,
    for ctx 6 (firefox, objects ~307k range) and ctx 14 (vkmark, objects 438-457,
    types 6/9/15/19).
  - each failed lookup → `vkr: cs decoder: ring FATAL set at vkr_cs_decoder_lookup_object:342`
    — the 0026 poison (per-context, no worker crash).
- 15:08:46 replay summary (the load-bearing line):
  `gpu restore: replay complete with 3607 dropped wire entries (stale references);
  drops by class [transient,create,recording,noted,free,pool-reset,ring-create,
  ring-destroy,ring-stream] = [0, 2, 3539, 0, 66, 0, 0, 0, 0]`
- 15:08:46 guest resumes (`PM: resume devices took 2.011s`). Immediately:
  - kernel: `virtio_gpu ... response 0x1203 (command 0x102)` (RESOURCE_UNREF →
    ERR_INVALID_RESOURCE_ID) — guest unref of a resource the host no longer knows.
  - vkmark's first real submit hits its FATAL ring → guest venus
    `vn_ring_submit_locked` `abort()` (by design when the ring is lost) → SIGABRT
    mid `vkCreateBuffer` in `CubeScene::setup`. Coredump 9088 preserved in the guest.
- 15:09:40 ctx 4 also trips `ring FATAL at vkr_dispatch_vkWaitRingSeqnoMESA:399`.

## Root cause (verified against the log + owned sources, 2026-07-20)

Neither of the first-pass candidate shapes was right — not a drop *cascade* inside the
replay (drops during replay clear the shared `ctx->cs_fatal_error` each time,
`vkr_renderer.c` `vkr_replay_recover_fatal`, and never orphan later entries by
themselves), and not a capture *race* (the journal had everything it ever retained).
The journal's pruning is the gap:

- Recording rule (`vkr_journal.c`): an object's destroy prunes every entry **keyed by**
  that object's id. But retained entries that merely *reference* the id in their wire
  args are not keyed by it — and creates referencing it can't replay once its create
  entry is gone. **The journal is not transitively closed over create-argument
  references.** Pipeline←shader-module is the canonical *legal* case (apps destroy
  modules right after pipeline creation; the pipeline stays live).
- The kill chain, exact and fully accounted in the log:
  1. Replay of ctx 14 (vkmark) reaches the tail: lookups of shader modules 452, 453,
     455, 456 (type 15) fail — their creates were pruned at vkmark's module destroys.
  2. The two `vkCreateGraphicsPipelines` entries (454, 457, type 19) fail on those
     lookups → dropped → **exactly the histogram's 2 create-class drops**.
  3. Recording entries binding 454/457 drop (part of the 3539). All replay-time
     FATALs are cleared (recoverable mode).
  4. `replay complete` logs; `replay_end` starts the rings and clears `ctx->replaying`
     → FATAL is sticky again.
  5. The re-created ring consumes its **parked** pre-suspend commands; one references
     pipeline 454 → `vkr_cs_decoder_lookup_object:342` FATAL, now sticky → ring thread
     exits → `vkr_ring_set_status_bits(VK_RING_STATUS_FATAL_BIT_MESA)` (guest-visible,
     `vkr_ring.c:476`).
  6. Guest venus `vn_ring_submit_internal` (`vn_ring.c:455`) reads the status bit on
     vkmark's next submit → `abort()` — the coredump's `vn_ring_submit_locked` frame,
     same second (15:08:46).
- The arithmetic closes exactly: FATAL-set lines by context = 66 (vkmark) + 449
  (gnome-shell) + 3093 (firefox) = 3608 = 3607 replay drops + **1 post-replay live
  failure** — the single one that killed vkmark's ring.
- The mass of the storm is the *benign* flavor of the same non-closure: 6331 of 6891
  failed lookups are type 9 (VkBuffer), ~6100 of them just THREE firefox buffers
  (307455/307489/307491) referenced by thousands of retained recordings — command
  buffers recorded against since-destroyed buffers (invalid to resubmit anyway;
  dropping is semantically fine). gnome-shell's 449 ≈ the 451 type-10 (VkImage)
  lookups — same shape.
- Secondary casualties, same family: guest kernel `RESOURCE_UNREF → 0x1203` right at
  resume (guest unref of a resource the host lost), and ctx 4 (gnome-shell) tripping
  `vkr_dispatch_vkWaitRingSeqnoMESA:399` (wait for a ring seqno the restored ring never
  reached) 55s later — the desktop visibly survived both.

## Fix direction

Generalize the journal's existing pin machinery (`vkr_journal_pin_key` /
`prune_deferred` / retained-destroy replay — today used only for blob←VkDeviceMemory)
to **every CREATE entry's decoded handle references**:

- Note each successful decode-time handle lookup into the TLS dispatch frame — single
  funnel: `vkr_cs_decoder_lookup_object` (all generated protocol lookups go through
  `vn_protocol_renderer_cs.h:83`).
- At `post_dispatch`, when the frame created objects (CREATE entry), pin every noted
  ref id that isn't one of the created ids; store the pinned refs on the entry.
- A pinned object's destroy defers its prune and retains the destroy command (existing
  machinery) — replay then re-runs create(module) → create(pipeline) → destroy(module)
  in original seq order, ending in the exact live world.
- Unpin when the create entry is killed (all its created ids dead) — needs an iterative
  worklist under the journal mutex (unpin may fire the deferred prune of the ref;
  no recursion / no mid-dispatch mis-retention: retention only happens when pinned>0).
- Guest-side hardening (separate, upstreamable): venus aborts on ring loss by design;
  failing submits with VK_ERROR_DEVICE_LOST instead is a mesa change worth pursuing
  independently.

## Files

- `supervisor-resume-session.log.gz` — dogfood-mac supervisor log, complete resume session
  (15:08:28→15:23; includes the full lookup-error storm + replay summary + DEBUG lines).
- `vkmark-coredump-info.txt` — guest `coredumpctl info 9088` (backtrace: abort ←
  vn_ring_submit_locked ← vn_buffer_create ← CubeScene::setup).
- `guest-kernel-resume-window.txt` — guest kernel journal 15:07-15:11 (s2idle cycle +
  the 0x1203 RESOURCE_UNREF error).
- The coredump itself persists on dogfood-guest (`coredumpctl dump 9088`) — survives poweroff.
