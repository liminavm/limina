# kk-timestamp — implement KK timestamp queries → lift zink-on-KK to GL 3.3 / GLSL 330

## Why
`glprobe` grants only **GL 3.2 core** on zink-on-KK. Mesa's `ver_3_3` gate
(`src/mesa/main/version.c:309`) needs 8 extensions; 7 are already present — the **only**
missing one is `GL_ARB_timer_query` (proven by `spikes/virgl-zink-kk/glprobe.c`'s gate dump).
zink enables it iff `VkQueueFamilyProperties.timestampValidBits > 0`
(`zink_screen.c:987`). KK hardcodes `timestampValidBits = 0` with a literal
`TODO_KOSMICKRISP Timestamp queries` (`kk_physical_device.c:1247`, from the upstream
`7c268a1e918 "kk: Add KosmicKrisp"` import — still present on Mesa main), and
`kk_CmdWriteTimestamp2` is an empty stub (`kk_query_pool.c:265`). We own KK → fill the TODO.

## Probes (build: `clang -fobjc-arc -framework Metal -framework Foundation -o P P.m`)
- **01-caps-probe** — this M1 Max supports counter sampling ONLY at
  `MTLCounterSamplingPointAtStageBoundary` (draw/dispatch/blit/tile all unsupported). A
  `timestamp` counter set with a `GPUTimestamp` counter exists. `sampleTimestamps:gpuTimestamp:`
  returns cpu==gpu ⇒ Apple GPU timestamps are already in the CPU **nanosecond** domain.
- **02-stageboundary-domain-probe** — a `MTLBlitPassDescriptor` sample-buffer attachment
  (start/end of encoder) samples correctly; resolved `MTLCounterResultTimestamp.timestamp`
  lands in the same ns window as `sampleTimestamps`, and encoder-duration == GPUEndTime−GPUStartTime.
  ⇒ `timestampPeriod = 1` (ns) is correct; **no tick→ns conversion**.
- **03-gpu-order-resolve-probe** — the GATING test:
  - **[A] PASS** — `resolveCounters:inRange:destinationBuffer:` in a **separate fenced blit
    encoder** within the same command buffer yields the correct value (== CPU
    `resolveCounterRange`), materialized mid-command-buffer. This is the load-bearing result:
    it lets us write the query result in **GPU command order**, not a CPU completion handler.
  - **[B] PASS** — an empty sampling encoder still records its start-of-encoder sample.
  - **[C] FAIL (by design)** — resolving a slot in the **same** encoder that sampled it reads 0.
    ⇒ **resolve MUST be a distinct, later, fenced encoder from the sample.**

## Design (validated; reviewed by a Fable advisor which killed the first cut)
FIRST cut (CPU resolve in the Metal completion handler) was **fatally wrong**: KK signals
VkFences via a GPU-encoded event that fires *before* the Metal completed-handler, and a
same-submit `vkCmdCopyQueryPoolResults(WAIT_BIT)` reads `pool->bo` via an in-stream GPU
compute shader — so a CPU handler write is both racy (reset/destroy window) and too late
(copy reads stale). KK's own `kk_query_write_cpu_result` comment documents exactly this
"must be written in GPU command order" invariant.

CORRECTED design — write the result in GPU command order, mirroring existing KK query writes:
1. `kk_physical_device.c`: `timestampValidBits = 64`, `timestampComputeAndGraphics = true`,
   keep `timestampPeriod = 1`.
2. Per `vkCmdWriteTimestamp2`: **sample encoder** (one-off blit created WITH a
   `startOfEncoderSampleIndex` attachment into a 1-sample `MTLCounterSampleBuffer`, fenced) →
   **resolve encoder** (separate fenced blit; `resolveCounters` the slot **directly into
   `pool->bo`** at `kk_query_offset(pool,query)` — 8 bytes = the report value; ≠ UINT64_MAX ⇒
   available). All fence-chained through KK's existing `signal_fence_and_end`/`wait_fence`.
   Ordering vs `CmdResetQueryPool` (a GPU dispatch writing UINT64_MAX) and vs
   `CmdCopyQueryPoolResults` (in-stream GPU read) is then automatically correct.
3. **In a render pass** (`last_used == KK_ENC_RENDER`): do NOT end the pass mid-flight (loadOp
   re-clear + pre_gfx event/bind-cache corruption). **Defer** — stash `(pool,query)` and flush
   the sample+resolve when the render encoder ends (value = end-of-pass = a logically-later
   stage, spec-legal). Matters for venus/guest-Vulkan, which can time mid-pass; zink rarely does.
4. Overflow/lifetime: per-timestamp 1-sample buffers (no capacity mgmt), released in
   `kk_post_execution_release`. Never silently drop a sample (a missing write leaves the query
   unavailable → `kk_query_wait_for_available` 2s timeout → DEVICE_LOST).

Stage is over-synchronized to ~bottom-of-pipe for all timestamps: spec-legal (impl may latch at
any logically-later stage), monotonic preserved, ARB_timer_query has no accuracy conformance.

## Status — SHIPPED (patches/kosmickrisp/0008)
Implemented as `patches/kosmickrisp/0008-kk-implement-timestamp-queries-*`. After rebuilding KK
(`ninja -C /Volumes/mesa-cs/build-kk`; needs Homebrew `llvm-ar` on PATH):
- `glprobe` grants **3.3 core** / `GL_VERSION 3.3 (Core Profile)`; `GL_ARB_timer_query` present;
  textured-triangle render still PASS.
- Real timer-query exercise (`spikes/virgl-zink-kk/timerprobe.c`): `glGetInteger64v(GL_TIMESTAMP)`,
  monotonic `glQueryCounter`, and **`GL_TIME_ELAPSED = 54µs nonzero`** — genuine GPU time, the exact
  thing an encode-time CPU sample would have gotten wrong (~0).

OPEN: deploy the rebuilt KK dylib into the enhanced-tier boot env and confirm the venus desktop
still comes up (KK is the shared host driver; the change is additive but venus now advertises
timestampValidBits=64 to guests). v2 (optional): accurate in-render-pass timing via per-pending
sampling instead of a single pass-end latch; VK_EXT_calibrated_timestamps (probe already proved the
shared ns domain) to speed zink's one-shot GL_TIMESTAMP path.
