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

## 2026-07-30: §22 wedge hunt — NOT reproducible with mid-animation exits (160 rounds)

`wedge-hunt.sh` on the gsrs rig (autologin loop, 4 windows, sustained overview
toggles, exit at a random mid-animation point, NIRI_VK_ASYNC_SCANOUT=1,
LIMINA_GPU_TRACE=1 host-side): 30×SIGKILL + 30×quit + 100×quit-with-windows =
**no wedge; the outstanding-fence oracle stayed empty throughout** — both the
hard-kill and orderly-quit retirement paths are solid on the current stack.
The dogfood §22 hit ran the instrumented-KK morning deploy (per-draw getenv →
much longer GPU tails → far wider unsignaled windows), their monitor config,
and one logout out of a day's use — a low-probability window we can't brute-force
locally. #16 pivots to mechanism-level fixes: retire-as-lost on the
vkr_queue_sync_submit failure branch (RED via an injectable failure knob) + a
generous deadline backstop for venus fence retirement (dma-fence "must
eventually signal"; PresentFenceState's wedge-proof ceiling as precedent).

## 2026-07-30 late: kk-0017 early-fence ROOT-CAUSED and FIXED (KK 0018)

Stage deletion on `explicit_sync_bridge` (env knobs added to their sync_spike.rs:
SYNC_SPIKE_SKIP_SEM / SYNC_SPIKE_SKIP_SYNCOBJ / SYNC_SPIKE_FENCE_REPEAT / SYNC_SPIKE_ITERS):
the fence stage ALONE is Pipelined even knob-ON; add the semaphore stage first and every
subsequent fence stage signals early (waits 0.2/0.9/11.8 ms vs D=173 ms — progressive
"healing"). The killer instrument was host-side: `LIMINA_KK_SYNCTRACE=1` (new, KK 0018)
traces every sync transition; with the seat stopped, the trace showed a fence WAIT
satisfied (cur=1) BEFORE its own cycle's empty-submit ENC-SIG was even encoded, then
RESET consuming the stale 1, then the late GPU signal landing after the reset and
pre-signaling the NEXT cycle — a self-sustaining off-by-one.

ROOT CAUSE: kk_sync_type_binary (kk 0017) = raw 0/1 MTLSharedEvent with CPU reset. The
commit's safety argument ("every binary wait moves the payload to a fresh event") covers
SEMAPHORES only; VkFences are never moved, and vkr pools + vkResetFences-recycles them
per submit. Under threaded submit the reset races the previous use's in-flight GPU
signal; one late signal locks in the off-by-one. Explains everything: fresh-fence repros
(emptysub.c, fdtruth.c) GREEN; the sem stage triggers it (its ~166 ms host CPU-block
export widens the in-flight window while the pool recycles); dogfood tearing = guest
IN_FENCE retiring one submit early; §23-era fake punctuality.

FIX (patches/kosmickrisp/0018, mesa commit d2aeced7eb6): binary reset swaps in a FRESH
event (the kk_timeline_move pattern) — the stale signal lands on the orphaned event
harmlessly; binary fences are CPU-waited only, so no encoded GPU wait dangles. Cost: one
MTLSharedEvent alloc per fence reset (same churn class as move; monotonic scheme later
if it profiles). RED→GREEN on the knob-ON rig host: fence→sync_file wait 0.2 ms/FAIL →
176.5 ms/PASS, all three bridge stages Pipelined.

0017+0018 together are now candidates for re-validation toward deploy (crossmark,
vkmark, suite, tearing eyeball) — 0017 alone stays deploy-blocked. NOTE: the guest tree
/home/claude/gnome-shell-rs on the rig image carries the (uncommitted) sync_spike.rs
stage-deletion knobs.

PERF RECHECK (same session): drawstorm 10k -i 3, seat stopped, 3 runs/arm on the rig —
fixed KK 605/612/615 fps vs pre-fix 675/640/599 fps: the arms OVERLAP (pre-fix run 3
below all fixed runs), so the alloc-per-reset costs nothing measurable; the 07-29
"717 fps" reference was a different boot/build state, not a bar this boot clears in
either arm. Monotonic-value scheme stays a someday-optimization only.
