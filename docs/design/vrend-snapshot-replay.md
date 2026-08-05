# vrend snapshot replay — retain-and-replay for the classic GL world (task #19)

Sibling of `venus-snapshot-replay.md`; extends the same two-layer re-creation journal to
classic vrend contexts. Motivation: since the 2026-08-04 GL-ladder flip the compositor and
all GL clients run on classic vrend on BOTH tiers, and the venus-only journal made snapshot
resume come back permanently black (`spikes/restore-s2idle-wake/RESULTS.md`, commit
68eb54f): restore rebuilds an empty vrend world, classic virgl is fire-and-forget, so the
guest renders into the void — 4.5 M rejected `CmdSubmit3d` + `SetScanout
ErrInvalidResourceId` in minutes, while every process-survival oracle stays green.

## What a seated classic world consists of

1. **virtio-gpu control-queue state** (libkrun layer): context creates (already journaled),
   classic resource creates (`RESOURCE_CREATE_2D/3D` — NOT journaled today; only
   `CREATE_BLOB` is), backing attach (`RESOURCE_ATTACH_BACKING` iovecs — not journaled),
   context-resource attach (journaled), non-blob `SET_SCANOUT` (not journaled; only
   `SET_SCANOUT_BLOB`).
2. **vrend wire state** (virgl layer, created inside `SUBMIT_3D` streams, decoded by
   `vrend_decode.c` `decode_table`): sub-contexts (`CREATE/SET/DESTROY_SUB_CTX` — the
   object namespace is **per sub-ctx**: `sub_ctx->object_hash`), objects
   (`CREATE_OBJECT`/`DESTROY_OBJECT`: blend/dsa/rasterizer/shader/vertex-elements/
   sampler-view/sampler-state/surface/query/streamout-target; shaders arrive in
   **continuation chunks** via `VIRGL_OBJ_SHADER_OFFSET`), `LINK_SHADER`, and the **current
   bound state** — gallium only re-emits `BIND_*`/`SET_*` on *change*, so a client that
   bound shader A once and draws forever never re-emits the bind. Structural replay without
   the current-state snapshot would draw with missing state indefinitely.
3. **Resource content**: classic resources keep a guest backing store (the iovecs) that the
   guest writes and syncs via `TRANSFER_TO_HOST_*` — it is *in the restored RAM*. Host-only
   content (FBO/renderbuffer results) has no guest backing but is re-rendered every frame
   by the live clients.

## Design

**Cross-layer fence: needed after all, but the venus machinery generalizes.** The first
draft claimed classic resources are control-queue-only; WRONG — `vrend_decode.c:1578`
decodes `VIRGL_CCMD_PIPE_RESOURCE_CREATE` (wire defines `blob_id` → args), and with
`+resource_blob` negotiated (our guests have it) mesa creates buffers via wire
`PIPE_RESOURCE_CREATE` followed by control-queue `CREATE_BLOB(blob_mem_host3d, blob_id)`.
Same wire-before-blob inversion as venus, so the **existing per-CreateBlob
`vkr_seq`/`replay_wire_upto!` fence machinery applies verbatim** once `journal_vkr_seq` /
`journal_export` route to the vrend journal for classic contexts (today they return
0/None — venus-gated). Non-blob classic resources (`RESOURCE_CREATE_2D/3D`) have no wire
dependency; for everything else the order is still two-phase per context:

    phase A (libkrun): create contexts (existing) → classic resource creates →
                       attach backings (restored-RAM iovecs) → ctx attaches
    phase B (virgl):   feed the retained wire journal through the normal
                       vrend_decode_ctx_submit_cmd path, per sub-ctx
    phase C (libkrun): full-box TRANSFER_TO_HOST per live resource (content, from
                       guest backing) → SET_SCANOUT (latest-wins) → flush

### libkrun layer (`gpu/journal.rs` + `worker.rs` recording sites)

New `GpuJournalOp` variants, tombstoned by `resource_unref` like blobs:

- `ResourceCreate2d/3d { resource_id, format, w, h, …full args }` (worker.rs arms at 698 /
  821)
- `AttachBacking { resource_id, iovecs: Vec<(u64, usize)> }` (arm at 741; detach prunes)
- `SetScanout { scanout_id, resource_id, rect }` — latest-wins per scanout (arm at 721),
  parallel to `SetScanoutBlob`

Replay: extend the existing `restore_gpu` loop (virtio_gpu.rs ~1118) — classic creates and
attaches replay like blobs but with **no `replay_wire_upto!` fencing**; phase-B wire feed
happens after all of a context's resources exist; phase-C transfers replay from the
restored guest memory (same `mem` the blob path uses), then scanout + flush.

### virgl layer: `vrend_journal` (new, `src/vrend/vrend_journal.{c,h}`)

Mirror of `vkr_journal` in spirit, but **command-classified, not effect-classified**: the
tee lives in `vrend_decode_ctx_submit_cmd` (single entry point for classic submits, like
`vn_dispatch_command` for venus) and copies raw dwords of durable commands into a
per-context log. Classes:

- **Creates** (retained until tombstone): `CREATE_OBJECT` keyed `(sub_ctx, type, handle)` —
  shader creates keyed `(sub_ctx, handle, chunk)` retaining the whole continuation set;
  `CREATE_SUB_CTX` keyed `(sub_ctx_id)`. Tombstones: `DESTROY_OBJECT` (prunes the create
  and every state entry referencing the handle is NOT required — see re-bind note),
  `DESTROY_SUB_CTX` (prunes everything under that sub-ctx).
- **Latest-wins state per sub-ctx** (the "current binds" snapshot), keyed by
  `(sub_ctx, cmd, slot-discriminator)` where the discriminator is the stage/start-slot/
  index words that make entries non-overlapping: `BIND_OBJECT` (per type),
  `BIND_SAMPLER_STATES` (stage,start), `SET_FRAMEBUFFER_STATE`,
  `SET_FRAMEBUFFER_STATE_NO_ATTACH`, `SET_VERTEX_BUFFERS`, `SET_SAMPLER_VIEWS`
  (stage,start), `SET_INDEX_BUFFER`, `SET_CONSTANT_BUFFER` (stage,index),
  `SET_UNIFORM_BUFFER` (stage,index), `SET_SHADER_BUFFERS`/`SET_SHADER_IMAGES`
  (stage,start), `SET_VIEWPORT_STATE` (start), `SET_SCISSOR_STATE` (start),
  `SET_STENCIL_REF`, `SET_BLEND_COLOR`, `SET_CLIP_STATE`, `SET_SAMPLE_MASK`,
  `SET_MIN_SAMPLES`, `SET_POLYGON_STIPPLE`, `SET_STREAMOUT_TARGETS`, `SET_TESS_STATE`,
  `SET_TWEAKS` (tweak id), `LINK_SHADER`, `SET_SUB_CTX` (context-level latest-wins, and
  replay brackets each sub-ctx's entries with a synthesized `SET_SUB_CTX`).
- **Queries**: retain `CREATE_OBJECT(query)`; `BEGIN_QUERY` latest-wins per handle, removed
  by `END_QUERY`. A query spanning the snapshot replays its BEGIN into the fresh context;
  results are bogus-but-answerable (no hang). Revisit if a client is seen to care.
- **Blob-backed pipe resources**: `PIPE_RESOURCE_CREATE` retained keyed by `blob_id`
  (word 11), pruned at the blob resource's GLOBAL unref (the venus pin/unpin FFI path
  generalizes); `PIPE_RESOURCE_SET_TYPE` latest-wins per res handle (word 1). These are the
  gbm/compositor buffers since the GL flip — load-bearing.
- **Video codec commands**: not journaled for now; the census counts them so a guest that
  uses them shows up loudly rather than silently losing state.
- **Dropped** (transient / re-emitted or content-path): `CLEAR*`, `DRAW_VBO`, `BLIT`,
  `RESOURCE_COPY_REGION`, `TRANSFER3D`, `END_QUERY`, `GET_QUERY_RESULT*`, barriers, string
  markers, `RESOURCE_INLINE_WRITE` (see gap below).

Log structure: per-context ordered vector with in-place latest-wins (record = drop older
same-key entry, append at tail — creation-before-bind order is preserved because dedup
never moves a create). Census counters + dump next to the vkr ones; `VREND_JOURNAL=0` kill
switch symmetric with `VKR_JOURNAL=0`.

Replay entry: reuse `limina_replay_submit` (the venus wire-replay FFI already routes
through `virgl_context->submit_cmd`, which for classic contexts IS
`vrend_decode_ctx_submit_cmd`) — verify and, if venus-gated, add the classic branch. Feed
order: sub-ctx creates first, then per sub-ctx `SET_SUB_CTX` + its creates + its state,
finish with the latest-wins current `SET_SUB_CTX`.

### Known content gap (phase 2 if it bites): RESOURCE_INLINE_WRITE

Small buffer uploads can go straight from the command stream without touching the guest
backing store; phase-C transfers would restore stale bytes for those ranges. Streaming
buffers self-heal (rewritten every frame); one-time inline-written init data may not. If
the pixel oracle shows residue: journal inline writes latest-wins per (resource,
offset-range), bounded by live content.

### Serialization

The GPU snapshot section gains a vrend part (per-context wire logs + the new journal ops);
bump the payload version; old snapshots fail-closed to the fresh-renderer fallback exactly
like a KK-fingerprint mismatch (existing machinery).

## Phases (RED-first, mirroring M9.3)

- **P0 — record + census.** vrend_journal tee + counters + dump; boot a seated GNOME,
  measure the bill of materials (entries/bytes per context; expected: dominated by shader
  create chunks, hundreds of objects, ~30 live state keys per sub-ctx). No behavior change.
- **P1 — serialize + replay.** Journal ops + wire logs into the snapshot; two-phase replay
  + transfers + scanout. RED gate first: new L2 (`vrend_session_preserved` or an extension
  of `venus_session_preserved`) asserting **post-restore classic health**: zero (or
  epsilon) `CmdSubmit3d`/`SetScanout`/`ResourceFlush` rejections in the first N seconds
  AND a successful post-restore scanout flip. Today's stack fails this in seconds — cheap,
  deterministic RED. (The counters already exist: unknown_ctx/unknown_res + the worker
  WARN storm.)
- **P2 — pixels.** Golden-frame oracle on the windowed present path (IOSurface capture per
  `limina-render-verify-golden`): post-restore capture must not be all-black and should
  converge to a redrawn desktop. Close the inline-write gap here if observed.

## Traps / notes carried from the diagnosis

- SIGUSR1 is the RAW seam (unquiesced dump of a running guest, L1 vehicle only);
  production suspend = SIGTSTP bracket. Never validate resume through SIGUSR1.
- Classic virgl reports no errors to the guest — a dead world is INVISIBLE guest-side;
  host counters/logs are the only tell.
- vrend has no venus-style dead-ring abort: clients render into the void forever, so
  process-survival oracles prove nothing about pixels.
- The venus replay machinery (staged payload at thaw re-activation, DRIVER_OK gate,
  restore-before-`replay_end` ordering) is load-bearing and shared; phase B rides the same
  staging point.
