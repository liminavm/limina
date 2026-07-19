# Venus snapshot & replay — seamless GPU suspend/resume (M9.3+)

Status: DESIGN (2026-07-19). Owner: M9.3 seamless resume.
Prereq reading: `spikes/m9-freeze-trigger/RESULTS.md` rounds 5–10 (the transport fix and the
session-restart discovery this design answers).

## 1. Problem

The M9 snapshot/restore transport layer is done: libkrun 0072 (sticky queue re-arm) revives the
virtio-gpu queues, the VM resumes, rendering works. But the **GNOME session restarts** instead of
being preserved: ~17 s after restore the pre-suspend gnome-shell aborts in mesa venus
(`abort ← vn_relax ← vn_ring_submit_locked ← vn_CreateImage` — the dead-ring abort threshold),
every Wayland client dies, and GNOME starts a fresh session.

Root cause: **the host GPU world does not survive restore.** The worker process restarts with an
empty virglrenderer (`rutabaga: 0 contexts, 0 resources`, vkr uninitialized), while the guest's
mesa — whose state lives in snapshotted guest RAM — still believes in its venus context, rings,
and ~1100 Vulkan objects. Its first submission after thaw spins on a ring nobody services.

Goal: **the guest never notices.** Same gnome-shell PID across restore, apps alive, no coredump.

## 2. Constraints

- **Must survive a host reboot.** The snapshot artifact on disk must contain everything needed to
  rebuild the host GPU world in a fresh worker on a freshly booted host. "Keep the host objects
  alive in memory" is not an architecture (though the journal lives in worker memory *while
  recording*, and phase 1 leans on that — see §8).
- **No guest modification required** (two-tier guarantee): stock mesa venus must be preserved as-is.
  Guest-side improvements (e.g. virtio-gpu PM ops, device-lost robustness) are enhanced-tier
  extras, never the entry fee.
- **Mechanism in virglrenderer/libkrun, policy in limina**, and upstreamable: venus VM migration is
  a known upstream want; the recording/replay mechanism should be shaped as a virglrenderer API,
  not a limina hack.
- **Fail closed to today's behavior.** If replay cannot be performed faithfully (host driver
  changed across reboot, unsupported feature in the journal), fall back to the current
  fresh-renderer restore: the session restarts, which is degraded but correct.

## 3. Core idea: a totally-ordered two-layer re-creation journal + content capture

Venus hands us the key: **object IDs are guest-assigned**, and every state-building operation
arrives as a self-contained wire command. So we don't serialize Vulkan objects — we **retain the
original wire bytes that built them** and replay those bytes through the existing decoder at
restore. The guest's IDs come back automatically; host handles are new but the guest never sees
host handles.

Two layers build the world, and their operations interleave in dependency order:

1. **rutabaga/virtio-gpu layer** (owned by libkrun): context create, `CREATE_BLOB`
   (which references vkr memory objects), blob map into guest PA, set_scanout.
2. **vkr wire layer** (owned by virglrenderer): every venus command — instance/device/ring
   creation, `vkCreate*`/`vkAllocate*`, binds, descriptor updates, command-buffer recordings.

A `CREATE_BLOB(blob_id=X)` needs vkr object X to already exist; a ring create references a blob
resource. So the journal is **one totally-ordered log with entries of both kinds**, recorded at
execution time under a shared sequence counter. libkrun owns the merged journal and the snapshot
file section; virglrenderer provides the record tee and the replay entry point.

Replay is then: walk the journal in order, dispatching rutabaga entries to the device layer and
wire entries to `vkr_context_submit_cmd()` (`third_party/virglrenderer/src/venus/vkr_context.c:241`)
— the same funnel live traffic uses, bypassing the ring.

## 4. Inventory: what must be reconstructed

Measured bill of materials for a healthy seated GNOME (probe 4, RESULTS.md): 1 vkr context,
3 rings, 1 sync queue, 18 resources, **1137 objects** (dset=770, buffer=93, image=62,
image_view=43, memory=29, cmd_buf=26, pipeline=19, cmd_pool=17, shader=16, dpool=14, …).

| State | Where it lives | How it comes back |
|---|---|---|
| Guest mesa's world-view | guest RAM | already snapshotted (M9) |
| rutabaga context/resource tables | worker memory | journal (rutabaga entries) |
| vkr objects (the 1137) | worker + KK/Metal | journal (wire entries) replayed |
| Ring cursors (head/tail/status) + buffer | **guest-visible blob resource** (`vkr_ring.h:76` — control/buffer point into `ring->resource`) | blob **content capture**; host `vkr_ring` recreated by replaying its create command, then synced to the in-memory cursors |
| VkDeviceMemory contents | host allocations (vkMapMemory ptrs), some mapped into guest PA windows | content capture (§6) — **not** covered by the guest-RAM dump |
| Blob→GPA mappings | HVF mapping state | recorded map ops replayed (`hv_vm_map` fresh host ptr at the same GPA) |
| Pipeline cache | KK | `vkGetPipelineCacheData` at snapshot, seeded at replay (also speeds up pipeline recreation) |
| Scanout binding + IOSurface | worker/WindowServer | re-issued at restore; IOSurface IDs are host-private, free to change |
| In-flight work (submits, unretired fences) | GPU | **none by construction** — quiesce drains to zero (§6) |

## 5. Recording (always-on, cheap)

Tee point: the command dispatch path, where the command type and its exact wire span are known
(decoder cursor before/after). Every command is classified by a generated table over
`VK_COMMAND_TYPE` (the venus protocol is generated code; the table is maintainable alongside it):

- **CREATE** (`vkCreate*`, `vkAllocate*`, ring/sync-queue create): retain the wire bytes, keyed by
  the new object ID(s).
- **DESTROY**: drop every entry keyed to that object (tombstone; the log is pruned, not grown).
- **MUTATE** (`vkBind*Memory`, `vkUpdateDescriptorSets`, `vkBeginCommandBuffer` + `vkCmd*` +
  `vkEndCommandBuffer`, `vkResetCommandBuffer/Pool`): retained keyed to the target object.
  `Begin`/`Reset` clears that object's mutation list first — so a per-frame re-recorded command
  buffer costs a bounded window, not unbounded growth. Descriptor updates append (v1); per-binding
  replace-compaction is a later optimization if dset-heavy apps make journals fat.
- **TRANSIENT** (`vkQueueSubmit*`, waits, fence/semaphore ops, query readbacks, feedback ops):
  not retained. After quiesce there is no in-flight work to replay.
- **Special**: `vkAllocateMemory` also flags the object for content capture; `vkCreatePipelineCache`
  flags for `GetPipelineCacheData` at snapshot.

rutabaga-layer recording (libkrun): context create/destroy, `CREATE_BLOB`/unref (params + blob id),
`resource_map_blob`/unmap (GPA), `set_scanout` — same tombstone pruning, same sequence counter.

Cost when not snapshotting: a classification lookup per command + memcpy of retained spans.
Steady-state GNOME is GPU-quiet (probes: idle ≈ 0 submits); the retained set is bounded by live
objects, and destroy prunes. The existing GPUTRACE reporter grows journal-size / entry-count
counters so we can watch it on real workloads. Estimated journal for seated GNOME: a few MB
(dominated by 16 shader modules' SPIR-V and 770 dset updates).

## 6. Snapshot-time: quiesce, then capture

Order matters; all of this happens after vCPUs pause (existing M9 machinery) and before the RAM
dump is finalized:

1. **Drain the rings**: let ring threads consume until `cur == tail` for every ring.
2. **Retire all fences**: wait until the fence ledger reads `outstanding=0` (probe 2 gives us this
   for free) and `vkDeviceWaitIdle` per device returns.
3. **Capture memory contents**: for each live `VkDeviceMemory` — map (or use the existing mapping)
   and memcpy. On KK/Apple UMA the observed allocations are host-visible (`host-visible blob via
   vkMapMemory` throughout the worker logs); a probe asserts this at snapshot time. Non-host-visible
   memory (if KK ever reports it) is a phase-3 GPU-copy path; v1 fails closed to fresh-renderer
   restore and logs.
4. **Pipeline cache**: `vkGetPipelineCacheData` per live cache.
5. **Serialize**: journal + contents + pipeline caches into the snapshot file (§7).

The guest may be parked mid-`vn_relax` on a submission — that's fine: its command bytes are already
in the ring buffer blob (captured as content), and the recreated ring resumes at the same cursors,
consumes it, and advances seqno as if nothing happened.

## 7. File format

Extend the versioned snapshot (`third_party/libkrun/src/vmm/src/snapshot.rs` — magic + version +
CRC, v4 added the per-device virtio-transport section) with a **v5 GPU section**:

```
[gpu section]
  presence byte (0 = no venus state; stock/2D guests pay nothing)
  host fingerprint: KK deviceUUID, driverVersion, virglrenderer patch level
  journal: entry count, then (seq, layer, object-key, len, bytes)*
  memory contents: count, then (memory object id, size, bytes)*      — zstd later
  pipeline caches: count, then (cache object id, len, bytes)*
```

The **host fingerprint fails the seamless path closed**: on restore, if KK's deviceUUID or driver
version differs from snapshot time (host OS update across the reboot), skip replay, restore
fresh-renderer, log why. The session restarts — today's behavior as the graceful floor.

## 8. Restore-time replay (before vCPUs resume)

1. Verify fingerprint; on mismatch → fresh-renderer fallback.
2. Initialize virglrenderer as today.
3. Walk the journal in sequence order:
   - rutabaga entries → recreate contexts, blobs (fresh host allocations), remap at the recorded
     GPAs, re-issue scanout binding;
   - wire entries → `vkr_context_submit_cmd()` on the recreated context.
   Memory contents are restored immediately after each `vkAllocateMemory`+map replays (so ring
   creates later in the journal read live control words from the blob).
4. Recreated rings sync `buffer.cur` to the restored tail and start their threads.
5. Resume vCPUs (existing 0072 transport re-arm runs as-is). The guest's parked submission is
   consumed; seqno advances; `vn_relax` wakes; **nobody aborts**.

**When replay runs (learned in P1, 2026-07-19): at the guest thaw's re-activation, not before
`gate.open()`.** The payload is *staged* on the device during `build_microvm` and the GPU worker
replays it when the guest's `DRIVER_OK` re-activates the device. Replaying before the vCPUs run
is doubly wrong: (a) the thaw resets the GPU (0072's bus fallback `reset → features → DRIVER_OK`),
and the host-side `reset_session`/`reset_session_state` would wipe the just-replayed world; (b) the
`GpuAddMapping` messages replay emits are serviced by a VMM thread that only starts *after*
`build_microvm` returns — a deadlock. At activation both hazards are gone, no queue command has
run yet, and the guest kernel is still mid-thaw so userspace can't touch mapped blobs before the
mappings are re-established. `replay_begin/end` run only for contexts that own a vkr wire journal
(the kernel's ctx 1 is virgl/none — no host venus state). On success the device journal adopts the
payload's op list (warm baseline for the next suspend); `reset_session` clears the journal.

Three more P1 lessons, each worth its scar:

- **Stale references are normal; context-FATAL must not be sticky during replay.** A retained
  `vkUpdateDescriptorSets` can reference an object destroyed pre-snapshot (its create was pruned);
  the lookup miss trips the context-wide FATAL and, left sticky, early-bails every later entry
  (one stale descriptor write killed 4k+ entries). The write is semantically droppable (dangling
  reference before, unwritten slot after — garbage-if-accessed either way): virgl clears the FATAL
  after a failed replay entry while `ctx->replaying`, libkrun counts it as a *recoverable* wire
  failure. Only structural rutabaga failures (context/blob/map) fail the replay. A seated GNOME
  snapshot replays with ~150–190 dropped stale entries out of ~4k.
- **`replay_begin` must poll: context creation is asynchronous.** The proxied create goes to the
  same-process render-server *thread*; the direct `limina_*` calls overtake it. libkrun retries
  `replay_begin` (1 ms × 2000) — each FFI call re-takes the renderer lock, letting the server in.
- **Pin the guest MAC across the restore.** The resumed guest keeps the NIC identity it read at
  boot, and gvproxy's config statically binds IP↔MAC — a fresh random MAC on the restore worker
  orphans the guest's cached one and the network never comes back (production restore keeps
  gvproxy alive across the worker swap; the L2 test pins `--net-mac` on both legs instead).

**P1 gate GREEN (2026-07-19):** `venus_session_preserved` passes — same boot_id, same gnome-shell
pid across suspend → snapshot → fresh-worker restore, zero new coredumps through the 35 s abort
window, and the gvproxy packet log showed a pre-suspend HTTPS connection *continuing* post-restore.

**First eyeball on a lived-in session (2026-07-19, F44 enhanced, user-driven apps): NOT ship-ready —
black window, and three concrete P2 work items.** The session core held (same boot_id, gnome-shell
same pid, zero coredumps, most of the world replayed), but:

1. **In-flight sync fd wedge (the black window).** gnome-shell resumed alive but wedged:
   `eu-stack` showed the main thread in `zink_flush → util_queue_fence` waiting on mesa's submit
   thread, which sat in `vn_GetSemaphoreFdKHR → vn_wsi_sync_wait → poll()` — a `sync_file` backed
   by a pre-suspend virtio-gpu fence that the new epoch never signals. The guest froze mid-frame;
   the fence's completion either raced the RAM dump or was never re-emitted. Fix shape: in
   `save_snapshot`, after the vCPUs park, wait (bounded) for the GPU **fence ledger** (0071 probe)
   to drain before `dump_ram` — the guest is frozen so no new submissions arrive, completions land
   in guest RAM/GIC before capture, and restored waiters see signaled fences. (The v4-era
   "worker-quiesce during the dump" open item, now with a body attached.)
2. **Descriptor-write journal bloat + stale storm.** A lived-in session retained **11,279** stale
   `vkUpdateDescriptorSets` entries (all referencing destroyed `VkImageView`s) vs ~150 on the idle
   test desktop. Recovery dropped them all correctly, but dset compaction (latest-wins per binding,
   deferred to P3) must move up to P2 — the journal is mostly garbage without it.
3. **Multi-context structural failure.** ctx 15 (a user-launched app) failed CREATE_BLOB res 144:
   `blob_id 51 is not a live VkDeviceMemory` — its backing alloc wasn't there despite the fence.
   The single-context L2 test never exercises this; needs a repro with `RUST_LOG=info` (the replay
   diagnostics are INFO-level and the default `warn` run captured nothing). Repro pair kept:
   `eyeball-m93.{raw,snap}` at the repo root (gitignored; ~49 GB — delete after P2).

**P2 progress (2026-07-19):** item 1 is FIXED — the worker drains the fence ledger to zero before
capture (libkrun 0075; `save_snapshot` reordered so GPU capture precedes the GIC save, letting
drain-time completion IRQs latch). The ~50% SSH-after-restore flake alongside it was a *double
suspend trigger* (the test's in-guest `systemctl suspend` + the bracket's button pulse; whichever
lands post-freeze replays on resume and re-suspends the restored guest unwakeable) — fixed with
`LIMINA_BRACKET_NO_BUTTON`. One open intermittent remains: run 12 resumed, then live ring traffic
hit `vn_cs_decoder_set_fatal` (ctx 3) ~1 s later → vn_relax abort → shell core. Run 13 (identical
build) was fully green, and its drop census (libkrun 0076: per-class histogram + cmd_type) showed
all 167 replay drops were **NOTED-class stale dset/bind writes** — the benign kind — with **zero
RING-class drops**, so the "dropped reply-position entry skews the ring" theory is unconfirmed.

**ROOT-CAUSED + FIXED (2026-07-19, libkrun 0077): the resumed guest raced the staged replay.**
Run 14's red log was decisive: the first *live* ring command after replay-complete was a genuine
`vkExecuteCommandStreamsMESA` (valid resource id, in-bounds offset/size — not framing garbage)
whose **nested stream** decoded as `vkCreateInstance` nonsense, i.e. the host read stale bytes
where the guest had just written its post-resume command stream. The staged replay runs
asynchronously at the thaw's DRIVER_OK re-activation while the vCPUs are already running; when
guest userspace thawed before the replay had re-established the blob GPA mappings, mesa's writes
landed in not-yet-remapped shmem and were lost. Fix: `Gpu::activate` blocks the guest's DRIVER_OK
write — a vCPU MMIO exit during `dpm_resume`, while guest userspace is still frozen — on a
`restore_done` condvar the worker flips after the staged replay completes (~360 ms observed;
120 s timeout escape hatch). Deadlock-free: the replay's `GpuAddMapping` messages are serviced by
the VMM worker thread, which needs nothing the blocked vCPU holds. Validated 5/5 green
(`venus-sess-loop.sh` runs 15–19) vs ~50% red before, barrier engaged every run.

**Eyeball item 2 FIXED (2026-07-19, virglrenderer 0036): the dset stale storm dies at record
time.** A `vkUpdateDescriptorSets` journal entry was keyed only by the touched set, so a
referenced view/sampler/buffer dying never pruned it — the 11k stale writes were entries whose
references died pre-snapshot. Now every referenced object is also a key; a NOTED entry dies with
its first dead key, which is behavior-equivalent to the replay-time drop (one dead reference
drops the whole entry there too). Run 20: NOTED drops 112–170 → **0**, total drops 700+ → 31
(all RECORDING-class `vkCmd*` stale refs — same treatment is possible later; much smaller storm,
still recoverable). Full HVF suite green on 0075+0076+0077.

Image layouts: after replay, images are in `UNDEFINED` while the guest believes otherwise. v1
transitions every replayed image to `GENERAL` after content restore (correct if conservative on
UMA/KK); v3 tracks last-known layout by watching barriers/renderpass final layouts at record time.

## 9. Phasing

- **P0 — recording infrastructure.** Classification table, journal + tombstones in vkr, rutabaga
  recording in libkrun, GPUTRACE journal metrics. No behavior change. Validate: journal object
  census matches the probe-4 dump (`virgl_renderer_limina_dump_state`) on a seated desktop.
- **P1 — replay, no content capture.** Serialize the journal (v5 section), replay object graph +
  ring recreation on restore; memory contents garbage except ring blobs (P1 captures **blob
  resources only** — small, and they carry the ring cursors + parked commands). Success = the RED
  L2 test goes half-green: same gnome-shell PID, no vn_relax abort, session survives; visuals may
  glitch (stale textures redraw within frames on a compositor).
- **P2 — full content capture.** All VkDeviceMemory captured/restored; pipeline cache seeding.
  Success = visually clean seamless resume, eyeball-verified. This is the ship gate for M9.3.
- **P3 — hardening.** Device-local GPU-copy path, layout tracking, journal compaction (dset
  replace semantics), zstd for contents, multi-context/app-diversity soak (games, video players,
  Firefox), host-reboot L2 (restore from file with caches cleared / worker cold-started), and the
  upstream conversation (venus migration RFC with the classification table).

## 10. Testing

- **The RED test exists in spirit already** and gets formalized as L2 `venus_session_preserved`:
  boot seated GNOME (enhanced image), record gnome-shell PID, inject gnome-calculator, snapshot,
  restore in a **fresh worker process**, assert: same boot_id, same gnome-shell PID, calculator
  alive, `coredumpctl` empty, GPUTRACE steady-state error counters zero. This is RED today (we
  watched it fail with a coredump).
- The fresh-worker-process requirement **is** the host-reboot simulation: nothing survives but the
  file. A periodic manual test does a literal host reboot between snapshot and restore.
- P0 gets a unit-level journal test (record a synthetic command stream, prune, assert retained
  set) and the census cross-check against the vkr state dump.
- Fallback path test: corrupt the fingerprint, assert fresh-renderer restore + session-restart
  behavior (never a wedge).

## 11. Risks / open questions

- **Layout tracking** (P3) is the known-hairy part; `GENERAL`-everything is the v1 answer and may
  cost performance until then.
- **KK memory model**: design assumes UMA host-visible allocations (all observations agree). The
  snapshot-time probe turns a wrong assumption into a clean fallback, not corruption.
- **Command-buffer replay fidelity**: buffers recorded pre-suspend and resubmitted post-restore
  must replay their recordings exactly; the Begin/Cmd*/End retention covers it, but interactions
  with pools (`vkResetCommandPool`) need care.
- **Cross-context/shm imports** (the 0032 SCM_RIGHTS path) and multi-context apps: P3 soak
  territory; the journal's total order should handle it, but it's unproven.
- **vrend (GL tier) contexts**: out of scope — tier-1 guests fall back to fresh-renderer restore
  (session restart), which is the documented degraded floor. venus is the enhanced tier and the
  M9.3 target.
- **Journal growth on churny apps** (games creating/destroying per-frame): tombstones bound live
  size, but record-side overhead needs the GPUTRACE metrics before we trust it everywhere.
