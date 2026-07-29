# venus-fence-truth — is an exported sync_file truthful about GPU completion?

2026-07-29. Trigger: tearing on the dogfood (background bleeding through windows
mid-overview-animation under `NIRI_VK_ASYNC_SCANOUT=1` — mixed animation frames in
horizontal bands, screenshot in the session log). Under async scanout the atomic
commit's `IN_FENCE_FD` is the only thing between a mid-render buffer and the glass,
so the first premise to verify was `venus-explicit-sync-gap.md` §6.2 — flagged there
in 2026-07-04 as *"the actual thing that can be broken"* and never measured.

## Probe

`fdtruth.c`: queue ~26 ms of serialized GPU copies (64×64 MiB, barrier-separated),
then measure — (1) baseline wall time via plain `vkWaitForFences`; (2) export the
submission's fence and signal semaphore as `SYNC_FD` right after `vkQueueSubmit`,
`poll(2)` each fd at export and in a loop until POLLIN. An fd that signals while the
GPU is mid-workload is the lie. Build/run recipe in the file header.

## Verdict (guest F44 enhanced, venus → virglrenderer → KK, M1 Max)

**The venus fence sync_file is TRUTHFUL.**

| observable | time after submit |
|---|---|
| fence fd exported (fd=5) | 0.02 ms, **state: pending** ✅ |
| fence fd signals | 25.8 ms = GPU completion ✅ |
| semaphore fd exported (fd=6) | **25.95 ms — the export call itself blocks until GPU completion** |
| GPU baseline | ~26 ms (32.7 incl. cold-run decode) |

Corroborating statics (all read, all honest): vn export = empty execbuf on the
queue's own ring (`vn_create_sync_file`, ring ≥ 1 via `vn_instance_acquire_ring_idx`)
with `vkWaitRingSeqnoMESA` ordering it behind the render's decode; host
`vkr_context_submit_fence` routes ring ≥ 1 to the per-queue sync thread =
`QueueSubmit(0, NULL, fence)` + `WaitForFences` (vkr_queue.c) = KK fence = Metal
completion. Ring 0 fences DO retire at decode (`ctx->retire_fence` immediately) —
correct for venus's CPU ring, and vn never puts render fences there.

Host-side control: same probe against KK directly (bundle 0016 dylib and build-kk
0017 knob-on) — truthful; export blocks then returns the `-1` already-signaled
sentinel (no native sync_file on macOS).

## Finding 2 — semaphore sync_fd export is a hidden full-GPU CPU wait

`vn_GetSemaphoreFdKHR` → `vn_create_sync_file` + **`vn_wsi_sync_wait(dev, fd)`**,
which is skipped only `if (dev->renderer->info.has_implicit_fencing)` — our renderer
doesn't advertise it, so **every semaphore sync_fd export CPU-blocks until the GPU
finishes**. Fence export does NOT wait (returns a live pending fd).

Consequence for the compositor (gsrs): the wlroots-style explicit-sync recipe
(§5 of venus-explicit-sync-gap.md) exports the render **semaphore** per frame — on
this stack that serializes the compositor thread on GPU completion every frame
(honest-but-late flips; a §21 miss/latency mechanism, NOT tearing). Exporting from
the **fence** is the async-correct path today. Longer term we can advertise/implement
implicit fencing or relax vn's wait (mesa patch, upstreamable).

## What this rules OUT for the tearing

- exported-fence lying early (measured truthful),
- the flip/buffer-release side (engineered: patched guest kernel fences blob-scanout
  RESOURCE_FLUSH; host holds it until the supervisor's "shown" CA-latch ack —
  virtio_gpu.rs `PresentFenceState`).

Remaining suspects, in order: compositor-side buffer/fence pairing or damage in the
new overview-animation code (incl. building IN_FENCE from a syncobj point that
wasn't materialized — wait-for-submit semantics hand back a signaled/null fence);
the scale-1 identity-mapping present branch (§21.2 anomaly — the artifacting was
seen in exactly that re-run). Both belong to the local gsrs rig
(docs/perf/gsrs-local-rig.md).

## Probe traps (cost hours; keep)

- **`vkWaitForFences` on a fence after SYNC_FD export blocks forever**: export has
  fence-reset side effects (vk_fence.c `vk_common_GetFenceFdKHR`, spec 1.2.194).
  The probe's first version did exactly that — the "hang" was misread as a driver
  regression (suspected kk 0017 knob-off) and burned a bisect cycle including a
  pointless `.move` removal experiment. Wait `vkQueueWaitIdle` instead.
  - Guest flavor: venus turns the forever-wait into a `vn_relax` **abort** (SIGABRT
    after ~minutes) — a vn_relax abort in WaitForFences smells like this, not like
    a lost fence.
  - Unexplained curiosity (not chased, invalid-usage territory): the clean-bundle
    0016 dylib RETURNS from that invalid wait; build-kk (0016 or 0017) blocks.
- **Pipe your probe through `head`/`tail` and a hang eats all output** — stdout is
  block-buffered; `setvbuf(_IOLBF)` + write to a file, and print poll snapshots
  IMMEDIATELY at each export (the semaphore export blocking ~26 ms silently pushed
  the first fence-fd poll past GPU completion in probe v2, masking the one number
  that mattered).

## 2026-07-29 late night: RED inventory for the kk 0017 early-fence bug

The deploy-accident bug (kk 0017 threaded submit → gsrs bridge test FAIL + dogfood
tearing) turned out NOT to be the obvious mechanism. Distilled repros are GREEN:

- `emptysub.c` (host, vkr's retirement pattern distilled: work submit + empty
  `QueueSubmit(0, fence)`): fence orders behind prior work on BOTH knob settings.
- `fdtruth.c` through the full guest stack on a knob-ON host: truthful.

The one reliable RED is the compositor side's test, run in a guest on a knob-ON
host — deterministic, ~2s:

    cargo test -p niri-vk explicit_sync_bridge   # in their repo; FAILS knob-ON

Its failing signature: the fence-stage workload/wait completes in ~0.1 ms where
~200 ms of busy-work was queued (semaphore stage before it behaved normally).
Suspects for the real trigger, in order: vn fence FEEDBACK interplay, vkr's
pooled/recycled VkFences meeting `kk_timeline_reset` with a late in-flight Metal
signal, multi-stage object reuse, the timeline-syncobj stage. Root-cause by
deleting stages from their test until the minimal trigger remains.
