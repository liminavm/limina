# crossmark matrix — 2026-07-28 (post virgl 0049-0056 + kk 0014/0015)

The re-baselined GL-vs-Vulkan scorecard the 2026-07-28 tier battery called for,
now same-scene across every tier. All cells: M1 Max host, 4 vCPU guests
(enhanced F44 for venus/zink cells, stock accessible F44 for vrend), EFI+venus
coexist boot, offscreen 1024x1024, 300 measured frames, run over ssh.

**Scene identity: every pixel hash matched across all five cells** for every
shape at matching parameters (e.g. draws-1k `cdc34cc1349de67e`, state
`52418dbe3768344e`, desktop `b66fac4e9752154b`, upload f=300
`3394eba3524c31b3`) — GL and Vulkan, guest and host, vrend's TGSI path
included, render bit-identical frames. The workloads are exactly comparable.

## Totals (per-frame ms; fps in parens)

| shape | venus VK | zink-on-venus GL | vrend GL | host KK VK | host zink-KK GL |
|---|---|---|---|---|---|
| draws 10k | **5.89** (170) | 6.48 (154) | 8.83 (113) | 2.84 (352) | 6.91 (145) |
| draws 1k | **0.79** (1273) | 1.48 (675) | 1.11 (900) | 0.59 (1703) | 1.13 (883) |
| state 1k | **1.68** (597) | 3.30 (303) | 2.16 (462) | 1.14 (876) | 2.19 (457) |
| upload | 0.75 (1326) | 1.03 (969) | **0.49** (2031) | 0.50 (2015) | 0.60 (1672) |
| desktop | 0.58 (1711) | 0.65 (1532) | **0.23** (4392) | 0.38 (2605) | 0.46 (2174) |

## Reading

- **The 2.4x virgl-beats-venus-GL verdict is DEAD at command scale.** At 10k
  draws zink-on-venus GL (154 fps) now beats vrend (113) by 36%, and native
  venus Vulkan (170) beats both. The cmdstream arc (virgl 0054-0056, kk 0015)
  plus the earlier relax-ladder work flipped the ranking that motivated it.
- **Vulkan is the right compositor bet.** venus VK wins every guest cell at
  command/state scale and its 10k-draw cell runs within 8% of the *host* GL
  reference. The remaining venus-vs-host-VK gap is ~2x at 10k draws (5.89 vs
  2.84) — the decode+replay double pass — and a fixed ~0.2-0.3 ms/frame sync
  envelope at small n.
- **Guest zink beats HOST zink at 10k draws** (6.48 vs 6.91): the venus ring
  batches the encode (guest draw section 2.66 ms vs host 5.26), so the
  virtualized GL stack pipelines better than direct host dispatch. The zink
  draw-loop cost itself is the dominant GL tax at scale, not the boundary.
- **vrend still owns the small-frame shapes** (desktop 0.23 ms, upload 0.49,
  draws-1k 1.11): lower fixed per-frame cost. CAVEAT: vrend's desktop cell
  beating even host-native KK VK (0.23 vs 0.38) smells like loose fence
  semantics — vrend's glFinish may return before the work fully retires (the
  known fence-accurate-present gap, docs/perf backlog). Treat vrend's
  small-frame sync numbers as optimistic until that lands; the 10k cell (GPU
  pipeline saturated) is the trustworthy one.
- **zink's state-churn cost crosses the boundary poorly** (3.30 vs vrend's
  2.16): GL program switches → vkPipeline binds amplify through venus. If
  state-heavy GL apps matter, this is the zink cell to attack next.

## The zink PBO upload bug (found by the upload shape)

Host zink-on-KK (mesa 26.2-devel, zink-kk branch): PBO texture upload (orphan
`glBufferData` + `glTexSubImage2D` from `GL_PIXEL_UNPACK_BUFFER`) applies only
the FIRST frame's upload; the texture is frozen afterward (hash constant
across f=2/5/60; `CM_GL_NO_PBO=1` client-pointer path tracks VK bit-for-bit).
**Guest zink-on-venus and vrend both do this correctly** — the bug is only in
the host zink-kk build. Repro is `-S upload` on the host GL cell, or the
dedicated oracle `pbotest.c` (below).

### Root-cause hunt (2026-07-28 night session, unresolved — narrowed to KK/Metal)

Oracles in this directory: `pbotest.c` (GL, per-frame verify + variant knobs),
`kkload.c` (pure-VK load-op probes), `kktexel.c` (pure-VK replica of the
st_pbo texel-fetch loop, with fresh-view / fresh-mem / fresh-pool /
one-submit axes). KK-side tracing: `kk-pbo-trace-instrumentation.patch`
(env-gated `LIMINA_KK_TRACE` prints, applies to /Volumes/mesa-cs/mesa; left
applied in the tree, harmless without the env var).

What is PROVEN (each by direct observation, frame 0 works / frames 1+ zeros):

- zink's command stream is correct and was exonerated end-to-end: barriers,
  load/store ops (KK forces LOAD+STORE), fences all verified in traces;
  `ZINK_DEBUG=sync` (maximal barriers) does not help; `noreorder`, `norp`,
  descriptor modes, `GALLIUM_THREAD=0` all no-ops on the symptom.
- The readback pipeline is innocent: a completion-handler probe on KK's
  image→buffer copy shows the GPU itself writes zeros into the staging
  buffer on frames 1+ (frame 0 writes the real pattern).
- The main-draw pipeline is innocent: `PT_NEW_TEX` passes, and the failing
  frames read back **zeros**, not the previous pattern — so the upload RP
  did execute and wrote zeros over the old content (a skipped upload would
  leave the previous frame's pattern).
- Therefore the poison is the **texel fetch through the fresh VkBufferView**
  (KK: linear MTLTexture created from the placement MTLBuffer, accessed
  bindlessly by gpuResourceID): it returns zeros on frames 1+ even though
  the underlying buffer memory demonstrably holds the data (`PT_CHECK_BUF`
  GPU-copies it back correctly, and frame 0 fetches the same buffer fine).
- Same-frame vertex fetches from an even *fresher* BO (zink's per-frame
  131k stream buffer) work — so post-first-commit heap allocations DO
  become GPU-visible in general; it is not a blanket residency failure.
- Exonerated by direct experiment: residency-set commit no-op-ing
  (force-dirty probe), adding the texel-buffer texture itself to the
  residency set, the kk 0015 cmd-pool BO cache (`LIMINA_KK_BOCACHE=0`),
  per-frame descriptor-pool churn, per-frame VkDeviceMemory churn,
  BO frees mid-stream (zink frees nothing during the run), premature
  fence signal (a 200 ms sleep after glFinish changes nothing), and
  cross-submit fence chaining (queue->wait_fence steal chain verified
  fence-by-fence in the trace — the copy provably runs after the RP that
  feeds it).
- `kktexel` with **fresh view + fresh memory + fresh descriptor pool +
  one-submit-no-wait** still PASSES — the minimal trigger is some
  yet-unreplicated zink-specific ingredient.
- TRAP for future sessions: `MTL_SHADER_VALIDATION=1` zeroes **all** KK
  bindless texture accesses ("MTLResourceUsage flags mismatch or missing")
  including known-good kktexel runs — the validation layer does not
  understand queue-attached MTLResidencySets. Do not treat it as evidence.

Remaining suspects for next session, in order: (1) the placement-heap
aliasing shape — zink's PBO VkBuffer is bound into a 2 MB slab whose heap
also carries KK's whole-heap `map` buffer, and the texel texture is created
from that placement buffer (`kk_bind_buffer_memory` →
`mtl_new_texture_with_descriptor_linear`); kktexel uses dedicated little
allocations. (2) GENERAL/unified image layouts (kktexel uses legacy
transitions). (3) The systematic Metal cmd-buffer address aliasing visible
in traces (each submit's main cb pointer == the previous submit's pre_gfx
pointer) — verify it is dealloc/realloc recycling, not a live double-commit.
(4) Xcode GPU capture of frame 1 would answer "what did the fetch actually
read" definitively.

Impact check: limina's shipping tiers are unaffected (guest zink→venus→KK
does PBO uploads correctly — crossmark guest upload cell verified; vrend
correct). Host zink-on-KK is a dev reference config only.

## Present axis (2026-07-28, same day)

`-p`/`-F` legs inside the seated GNOME session, display pinned 1280x800@1.0
(config-file + reboot), uncapped (venus VK negotiated **mailbox**, GL legs
swap-interval 0), fullscreen configure = 1280x800 confirming the pin. Guest
cells only — present *is* the guest→host scanout path.

Fullscreen totals, per-frame ms (fps):

| leg | venus VK | zink-on-venus GL | vrend GL |
|---|---|---|---|
| desktop | 0.84 (1186) | 0.67 (1483) | **0.26** (3884) |
| draws 1k | 1.21 (826) | 1.14 (876) | 1.07 (932) |
| draws 10k | **5.33** (188) | 5.40 (185) | 8.71 (115) |

(windowed 1024x1024 desktop legs: venus 1.08, zink 0.70, vrend 0.37 —
fullscreen is uniformly cheaper.)

- **The offscreen ranking survives the present path at scale**: venus ≈
  zink-on-venus ≫ vrend at 10k draws, with present adding only ~0.1-0.3 ms
  to the venus legs. Nothing about scanout rescues vrend's command-rate gap.
- **vrend is still the small-frame champion through present** (0.26 ms
  desktop, 4.6x faster than zink-on-venus) — consistent with its offscreen
  fixed-cost edge and, plausibly, its looser fencing.
- **Methodology asymmetry to keep in mind**: the VK leg fence-waits every
  frame before presenting (a fully serialized single-in-flight app — worst
  case), while the GL legs pipeline through eglSwapBuffers (sync=0). Small-n
  GL-vs-VK present totals therefore flatter GL; at 10k draws they converge
  (185 vs 188 fps), where the pipeline is saturated either way.
- Uncapped present measures the present-path *overhead*, not the display
  rate — mailbox discards; the compositor still samples at its own cadence.
- Not yet verified which mutter path (composite vs direct-scanout) each leg
  hit — needs a RUST_LOG=trace run reading `[FLUSH2]`/`[FENCEPRESENT]`
  DIAGs; the comparison is still fair (all three cells share the same guest
  compositor), but absolute present costs may differ by path.

## Caveats

- Both host references are **debugoptimized** mesa builds (assertions on);
  guest mesa is a release RPM build. Host cells are comparable to each other;
  host-vs-guest deltas are slightly pessimistic on the host side. The worker's
  KK (venus cells' host half) is the same debugoptimized build-kk, so the
  guest cells share that caveat.
- Offscreen only: no scanout/present cost in any cell. The -present axis
  (Wayland, uncapped, windowed+fullscreen) is the planned follow-up and is
  where vrend's timer-present vs venus fence-present will actually show.
- Single boot per guest cell, single run per number (draws-10k venus rechecked
  ~5.35-5.9 across boots earlier the same day — treat ±5-8% as boot noise, per
  the vkmark-not-glmark2 rule).
