# kosmickrisp — patch-audit ledger

21 commits; base `418f2963a15` (mesa main, Metal-4/live-record era — rebased 2026-08-05,
see *Rebase executed* below). Schema + protocol: `README.md`.
Rows are keyed by SUBJECT; ordinals are informational and drift on re-export.

> **Fork model since 2026-08-04.** There is no `patches/kosmickrisp/` any more — our delta is the
> commits on the `limina-kk` branch of `github.com/liminavm/mesa`, pinned by
> `third_party/manifest.toml`. Read a "patch" here as "the commit with this subject". Unlike the
> other forks this tree is **not** vendored by `cargo xtask vendor`: it lives at
> `/Volumes/mesa-cs/mesa` on a case-sensitive sparse image (Mesa will not build on a
> case-insensitive filesystem), so the manifest pin records which rev that checkout *should* be on
> rather than driving a clone. The fork is also the first off-machine copy this work has ever had.
> Tag before every branch rewrite — every rev ever pinned must stay reachable.
> One commit joined at migration and is not yet audited: *"kosmickrisp: implement the MTLTEXTURE
> handle type of VK_EXT_external_memory_metal"* (paired with the virgl MTLTexture scanout commit;
> gated off by default).

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | limina: KosmicKrisp + zink host-stack patches (MTL4 re-port of the monolith) | `src/gallium/drivers/zink/zink_screen.c`, `src/kosmickrisp/bridge/mtl_buffer.m`, `src/kosmickrisp/bridge/mtl_command_buffer.m` +18 |  | mixed — SPLIT REQUIRED; per-component verdicts in Findings (vn_wsi superseded@311a437c; dm-assert superseded@bbf67327; zink guard + XFB + dm nil-check still needed; bind-cache obsolete-by-architecture on MTL4) | main `84acd848` 2026-08-03 | none-yet | none-yet | yes:guest→host-memory-safety in the XFB component (the 0007 clamp is the fix and must travel with it; no embargo — upstream has no XFB) | split-then-fold: decompose per Findings; fold 0007 + 0015 into their parent components; strip-diag-first (~340 DIAG lines: RTLOG/STATS/CAPTURE) | host | **split-then-triage** — components range from drop to upstream-now (dm nil-check ~15 LOC; 2DArray→2D demotion ~6 LOC pending tip re-verify) to upstream-after-split (XFB, zink guard) to carry-re-derive (perf pack) | next KK base bump = PARTIAL REWRITE of the delta (~40% dead: kk_encoder.c deleted, MTL4 argument tables kill the bind-cache premise; ~35% survives but needs re-measurement; ~25% XFB re-port) |
| 0002 | kk: lay out DRM-format-modifier attachment images as tiled, not linear | `src/kosmickrisp/vulkan/kk_image_layout.c` |  | needed — `kk_image_layout.c` unchanged since base, no modifier carve-out on tip | file-touch `9405760a` (pre-base); tip `84acd848` 2026-08-03 | n/a — see #14209 (KK/MoltenVK parity) for the modifier conversation | n/a | yes:guest-DoS-abort (nil encoder → worker SIGABRT) | standalone | host | **carry** — unreachable upstream: KK never advertises EXT_image_drm_format_modifier; only our venus (mesa 0010) force-advertise creates the trigger. Upstreamable only inside a real KK modifier feature |  |
| 0003 | kk: clamp attachment-less render pass target size + sample count to >= 1 | `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/kosmickrisp/vulkan/kk_encoder.c` |  | conflicts-on-tip — bug PERSISTS relocated (kk_encoder.c deleted upstream; unclamped renderArea at kk_cmd_draw.c:306 + deferred start overwrites sample count at kk_cmd_buffer.c:293; nil encoder now crashes in mtl_set_vertex_amplification_count) | kk_cmd_draw.c `603a410a` 2026-07-25; kk_cmd_buffer.c `7193e63e` 2026-07-23; tip `84acd848` | none-yet | none-yet | yes:guest-DoS-abort | standalone | host | upstream-after-cleanup (rebase to the relocated code first) | natively reproducible upstream: original trigger was mesa's own zink, and upstream now supports zink-on-KK (MRs 38605/41359) |
| 0004 | kk: give heap-less (host-imported) tiled image planes a private Metal heap | `src/kosmickrisp/vulkan/kk_image.c`, `src/kosmickrisp/vulkan/kk_image.h` |  | needed — assert intact at kk_image.c:858; heap-from-host-pointer limitation at kk_device_memory.c:133 | kk_image.c `282d791a` 2026-07-24; kk_device_memory.c `4a1ee1bd` **2026-08-03 (touched today — re-fetch before rebasing)** | none-yet | none-yet | yes:guest-DoS-abort | standalone | host | upstream-after-cleanup (strip the limina: comment prefixes) | reachable by NATIVE macOS apps via EXT_external_memory_host + OPTIMAL-tiling bind — not a wgpu-only quirk; KK under very active LunarG refactoring, supersession risk is time-sensitive |
| 0005 | kk: advertise VK_EXT_custom_border_color (lift zink-on-KK above GL 3.1) | `src/kosmickrisp/vulkan/kk_physical_device.c` |  | superseded-upstream@4c8e720c (2026-07-31!) — LunarG impl is FULLER (NIR lowering + border_color_swizzle) but gated `MESA_KK_EXPERIMENTAL=custom_border`, default OFF | main `84acd848` 2026-08-03 | n/a | theirs: !43078 (merged) | no | standalone | host | **DROPPED 2026-08-05 (rebase)** — upstream impl adopted; worker defaults `MESA_KK_EXPERIMENTAL=custom_border` | verified before deletion: zink lights up under the gate, zero custom-border warnings in the seated smoke |
| 0006 | kk: advertise VK_EXT_depth_clip_enable (GL_ARB_depth_clamp -> GL 3.2) | `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/kosmickrisp/vulkan/kk_physical_device.c` |  | needed; pending-supersession — Draft !43421 (squidbus, opened **2026-08-03, today**) is a strict SUPERSET (adds shader-emulated clamp for clamp+clip-both-on, which ours silently drops, + dynamic-state3) | main `84acd848` 2026-08-03; not on main yet | n/a | theirs: !43421 (Draft) — file NOTHING, watch it | no | standalone | host | carry → drop on !43421 merge | kk_cmd_draw.c churned heavily upstream (Metal4 + live-record) — conflict risk high at rebase |
| 0007 | kk: clamp guest-controlled transform-feedback buffer indices | `src/kosmickrisp/vulkan/kk_cmd_draw.c` |  | needed — but the vulnerable indexing exists ONLY in our 0001-added XFB handlers; upstream KK has NO transform-feedback code on main | main `84acd848` 2026-08-03 (blob search: 0 XFB cmd hits) | n/a until XFB itself is proposed | n/a | yes:guest→host-memory-corruption — in limina-authored code only, never shipped upstream ⇒ NO disclosure embargo | fold-into:0001 (XFB component) — INVARIANT: if XFB is ever proposed upstream, the clamp goes in the same MR, never trails it | host | carry | OOB write of guest-controlled addr+size past fixed xfb.buf[4]; SIGBUS repro spikes/venus-draw-probe/xfb-oob-probe.c |
| 0008 | kk: implement timestamp queries (vkCmdWriteTimestamp2) -> lift zink-on-KK to GL 3.3 | `src/kosmickrisp/bridge/mtl_device.h`, `src/kosmickrisp/bridge/mtl_device.m`, `src/kosmickrisp/bridge/mtl_encoder.h` +8 |  | superseded-upstream@ed807097 — LunarG implemented VK_QUERY_TYPE_TIMESTAMP on Metal 4 (MR !42864, merged 2026-07-14; + stage-LUT fix 7f540b05/!43081) | kk_query_pool.c file-touch `282d791a`, 2026-08-03 | work_items/14623 (theirs, closed by the MR) | !42864 (merged, theirs) — no MR path for ours | no | root of the 0010–0013 fixup chain (carry-fold only) | host | **DROPPED 2026-08-05 (rebase)** — upstream's MTL4-shaped impl inherited | M4 Pro probe ran same day: 100/100 clean, action item CLOSED, no upstream issue to file |
| 0009 | vk/meta: skip empty rects in vk_meta_clear_attachments | `src/vulkan/runtime/vk_meta_clear.c` |  | needed — applies cleanly; setup_viewport_scissor still asserts x0<x1 and computes x1-1 (u32 wrap corrupts sibling rects) | vk_meta_clear.c `44362461` (pre-base); vk_meta_draw_rects.c `18db05d3` 2024; tip `84acd848` | none-yet; no rejected lookalike found | none-yet — **ACTION: file the MR** | yes:guest-DoS-abort (NDEBUG: rendering corruption, not memory-safety) → public MR OK per disclosure logic | standalone | host | **upstream-now** — shared runtime, every vk_meta consumer (nvk/panvk/asahi/kk) inherits the fix | defense-in-depth twin virgl 0045 stays regardless; our multiview rect_count→max_rect_count hunk applies clean |
| 0010 | kk: resolve counter samples from a later command buffer where the GPU needs it | `src/kosmickrisp/bridge/mtl_device.h`, `src/kosmickrisp/bridge/mtl_device.m`, `src/kosmickrisp/vulkan/kk_encoder.c` +1 |  | superseded twice: net-deleted by our own 0013, and by upstream ed807097 | same | n/a | none (moot) | no | fold-into:0008 (dead code after 0013) | host | **DROPPED 2026-08-05 (rebase)** | split-cmd-buffer workaround 0013 removed entirely |
| 0011 | kk: make the split-counter-resolve probe fail safe and multi-sample | `src/kosmickrisp/bridge/mtl_device.m` |  | superseded (probe deleted by 0013) | same | n/a | none | no | fold-into:0008 (dead code after 0013) | host | **DROPPED 2026-08-05 (rebase)** | probe hardening for machinery 0013 removed |
| 0012 | kk: the timestamp sampling encoder must carry work — the sample was never taken | `src/kosmickrisp/bridge/mtl_device.h`, `src/kosmickrisp/bridge/mtl_device.m`, `src/kosmickrisp/bridge/mtl_encoder.h` +2 |  | superseded-upstream@ed807097 (upstream writes carry real work by construction) | same | n/a | none | no | fold-into:0008; strip-diag-first (`LIMINA_KK_TS_TRACE`) | host | **DROPPED 2026-08-05 (rebase)** | keep the Metal-3 fact somewhere durable: an empty blit encoder is elided and its sample is never taken |
| 0013 | kk: resolve timestamp queries on the CPU, and order the report write explicitly | `src/kosmickrisp/bridge/mtl_device.h`, `src/kosmickrisp/bridge/mtl_device.m`, `src/kosmickrisp/bridge/mtl_encoder.h` +7 |  | needed-in-carry; superseded-upstream@ed807097 for upstream purposes | same | none-yet — candidate BUG REPORT, see Findings | none | no | fold-into:0008 | host | **DROPPED 2026-08-05 (rebase)** with the arc | our shipped, M4-Pro-verified mechanism (CPU resolve + 0xff sentinel + sequenced completion); upstream resolves in the same MTL4 cmdbuf — may carry OUR bug on M4-class GPUs |
| 0014 | mesa: expose GL_AMD_pinned_memory on ES2+ contexts | `src/mesa/main/extensions_table.h` |  | needed — main still `GLL, GLC, x, x` (desktop-only) | file-touch `7f7c4ebb` 2025-11-18 (quiet file) | none-yet | none-yet | no | standalone | host | carry; optional low-priority upstream RFC | no deliberate restriction ever debated (only zink enablement MRs !23199/!28244 mention the ext); ES-flip precedent exists (AMD_performance_monitor, 10d21d41); Khronos spec lists no ES interactions — invites a conformance debate |
| 0015 | kk: cache command-pool BOs — default cap 512, LIMINA_KK_BOCACHE override *(2026-08-05: 0001's hunk + this row merged into one standalone commit)* | `src/kosmickrisp/vulkan/kk_cmd_pool.c` |  | needed — upstream `KK_CMD_POOL_BO_MAX` still 32; file quiet since May | main `84acd848` 2026-08-03; file-touch `b8f0fe6b` 2026-05-25 | none-yet | none-yet | no | amends 0001's `LIMINA_KK_BOCACHE` hunk — fold there or re-express standalone | host | upstream-after-cleanup: plain default bump (or adaptive cap) WITHOUT the LIMINA_ env, with the drawstorm numbers | RE-MEASURE on tip first — upstream's live-record rework (0cd84d45) may have shifted the BO-churn profile (10k draws: 3.68→2.56 ms/frame on our base) |
| 0016 | mesa/st: re-dirty FS sampler views after PBO upload/download meta-ops | `src/mesa/state_tracker/st_cb_texture.c` |  | **superseded-upstream@479773c7e42** — identical 2-line fix, same `Fixes: 62efee18607`, authored 2026-07-22, merged 07-24 (5 days BEFORE our independent root-cause); stable pick !43295 | file-touch `f4e0792f` 2026-07-29 | n/a (Reported-by trailer, no public issue) | !43151 (merged, theirs) — nothing to file | no | strip-diag-first (`LIMINA_PBO_TRACE`) | host — NO guest twin exists or is needed (guest F43 mesa predates the regression; mesa 0016-pre is the unrelated freelist backport) | **DROPPED 2026-08-05 (rebase)**, DIAG hunk included — upstream fix inherited | reproducer upstream = dosbox black screen, same zero-texel class; CLOSE the limina-zink-pbo-kk "OPEN: upstream it" item |
| 0017 | kosmickrisp: threaded queue submission (move-capable binary syncs) | `src/kosmickrisp/vulkan/kk_device.c`, `src/kosmickrisp/vulkan/kk_physical_device.c`, `src/kosmickrisp/vulkan/kk_physical_device.h` +3 |  | conflicts-on-tip + PREMISE DELETED upstream — !42621 live-record rework (merged 2026-07-03, 0cd84d45): encoding now happens at vkCmd* record time, vkQueueSubmit no longer pays the replay | main `84acd848` 2026-08-03; kk_queue.c churned (Metal4, hang detection, drawable waits); kk_sync.c untouched upstream | n/a | none-yet — no upstream threaded-submit MR exists | no | **PAIR with 0018 — must never ship/rebase alone** (0017-without-0018 = the dogfood tearing; tripwire exists) | host | **RETIRED 2026-08-05 (benchmarked same day)** — tip profile: ring thread 68% idle under vkmark, submit ~11% of wall (was dominant pre-live-record); both commits CONFLICT on tip (kk_device/kk_queue/kk_sync). Residue: KK still creates render encoders at submit for classic-render-pass apps (~3% of ring wall) — latency-class, and the right fix is dynamic-rendering-at-record, not thread offload. Numbers: `spikes/venus-cmdstream-probe/RESULTS.md` §2026-08-05; revivable from tag `limina-kk-2026-08-05-pre-mtl4-rebase` | any upstream form would be a re-derived proposal against the live-record architecture, not a rebase |
| 0018 | kosmickrisp: fresh event on binary reset (recycled-fence early signal) + sync trace | `src/kosmickrisp/vulkan/kk_queue.c`, `src/kosmickrisp/vulkan/kk_sync.c`, `src/kosmickrisp/vulkan/kk_sync.h` |  | needed-iff-0017-carried — fixes 0017's own kk_sync_type_binary; upstream binary syncs use the vk_sync_binary wrapper and have no such race | main `84acd848` 2026-08-03 | n/a | n/a | no | fold-into:0017 (makes the tripwire structural — one patch cannot be half-deployed); strip `LIMINA_KK_SYNCTRACE` if ever upstreamed | host | **REMOVED from the branch 2026-08-05** with 0017 (pair rule holds — revive together or not at all) | meaningless standalone |
| 0019 | vulkan/runtime: log-only render-pass begin VU mismatches instead of asserting | `src/vulkan/runtime/vk_render_pass.c` |  | needed — all three asserts live at vk_render_pass.c:2708/2732/2746 | file-touch `3abdee9e` 2026-03 (pre-base); tip `84acd848` | n/a | n/a | yes:guest-DoS-abort; OPEN RESIDUE: the attachment-COUNT asserts in the same function are untouched — NDEBUG makes them a potential guest-influenced OOB read (sec:memory-safety territory; on hardening-backlog) | standalone | host | **carry** — upstream deliberately asserts on VU violations ("valid usage in, or all bets off"); plausible future pitch = opt-in untrusted-caller mode, no upstream precedent | finish the COUNT clamps |

## Findings

### Rebase executed 2026-08-05 (the "own milestone" the 08-03 verdict called for)

Base `178a3d73968` → `418f2963a15`; head `f7145c12` → `a3df3aae` (21 commits); old head
tagged `limina-kk-2026-08-05-pre-mtl4-rebase` (pushed). Claude-Session trailers stripped
(message-only rewrite, trees verified identical). Validated: both builds compile, full HVF
suite 79/79, seated venus desktop smoke (human-eyeballed). KK is now Vulkan 1.4 on
Mesa 26.3.0-devel.

- **Dropped, upstream implementation adopted:** 0005 custom-border (`4c8e720c`; the worker
  now defaults `MESA_KK_EXPERIMENTAL=custom_border` — mechanism upstream, policy in limina),
  the whole timestamp arc 0008+0010–0013 (`ed807097` + `7f540b05`; the M4 Pro probe ran
  2026-08-05: upstream's same-cmdbuf resolve is CLEAN, 100/100, no issue to file — see
  `spikes/kk-timestamp-probe/RESULTS.md` §2026-08-05), 0016's core hunks (`479773c7e42`), the
  monolith's vn_wsi deep-copy and dm-assert components (`311a437c`, `bbf67327`).
- **Removed, then RETIRED same day on measurement:** the 0017+0018 threaded-submit pair —
  live-record deleted its premise, and the tip profile confirmed it (ring thread 68% idle
  under vkmark, submit ~11% of wall; details + the render-encoder-at-submit residue in
  `spikes/venus-cmdstream-probe/RESULTS.md` §2026-08-05).
- **Re-derived on MTL4:** the 0001 monolith (now *"limina: KosmicKrisp + zink host-stack
  patches (MTL4 re-port of the monolith)"* + two fixup commits — bind-cache/fastbind/
  slimroot excised as obsolete-by-architecture, XFB survives in full with the 0007 clamp,
  XFB root rebind now rides the argument table), 0003 attachment-less clamp (re-derived at
  the relocated code), 0015 BO-cache (folded with 0001's hunk into one standalone commit).
- **Cherry-picked clean:** 0002, 0004, 0006 (watch !43421 — still supersedes on merge),
  0009 + the negative-offset/i32-overflow follow-up, 0014, 0019, the zink lost-wakeup fix,
  the modifier-ext series, the EGLImage pair, MTLTEXTURE, queue-family-foreign.
- **New trap for the harness/scripts:** upstream zink now dlopens
  `@rpath/libvulkan.1.dylib` and meson strips build rpaths at install — every worker env
  needs the one-symlink `vulkan-rpath` shim dir (boot-enhanced-efi-kk.sh + limina-test
  `with_virgl_host_gl` both carry it; a miss silently degrades the worker to software-2D
  and fails the entire seated suite, seen live on the first post-rebase run).

### Series verdict (all 19 rows researched 2026-08-03, vs main `84acd848`)

The dominant fact: **upstream KK is under very active LunarG development and has
independently reinvented four of our patches in the last five weeks** (timestamps
ed807097, custom border 4c8e720c, PBO sampler-views 479773c7e42, vn_wsi deep-copy
311a437c — plus a depth_clip Draft opened 2026-08-03). Meanwhile it moved to
Metal 4 command encoding and deleted `kk_encoder.c`, so **the next KK base bump is
a series-wide partial rewrite, not a rebase**: the timestamp arc and 0005 drop
outright, 0016's core hunks vanish, the monolith loses its bind-cache premise, and
0003/0006/0017 need re-anchoring. Plan it as its own milestone.

Upstream-NOW queue (public MRs, no embargo): **0009** (vk_meta empty rects — shared
runtime, broad audience), the monolith's **dm nil-check** (~15 LOC), and its
**2DArray→2D demotion** (~6 LOC, re-verify on tip first). Upstream-after-cleanup:
0003 (bug persists relocated), 0004 (natively reachable), 0015 (de-limina the env,
re-measure on tip), the zink driCheckOption guard, and the split-out XFB feature
(with 0007's clamp folded in — never separately). Carries: 0002 (unreachable
without modifier advertisement), 0014, 0019 (+ finish the COUNT clamps).

### 0001 — the monolith decomposes into six components

(1) vn_wsi deep-copy: superseded@311a437c, drop. (2) external-memory hardening:
assert half superseded@bbf67327; the `newBufferWithBytesNoCopy` NIL-CHECK is still
missing upstream (residency-commit segfault) — upstream-now. (3) zink
driCheckOption guard: still needed (unconditional driQueryOptionb on tip);
interim MR defensible, real fix = wiring options_info into the drisw/kopper
loader. (4) XFB + primitives-generated-query (~620 LOC incl. kk_nir_lower_xfb.c):
upstream still has NO xfb (the properties block at kk_physical_device.c:850 is
dead import code — the `.EXT_transform_feedback` feature grep is the real oracle);
flagship upstream candidate after re-port onto MTL4 + 0007 folded in. (5) perf
pack (~330 LOC): bind-cache/fastbind obsolete-by-architecture (MTL4 argument
tables); slimroot/slimpush/elsize/NOLISTRESTART/BO-cap portable but re-measure;
earlyz stays a policy knob. (6) bridge DIAG (~280 LOC) + the 2DArray→2D demotion
functional fix: keep DIAG as a separate debug patch; demotion is a micro-MR
candidate.


### 0008 + 0010–0013 — the timestamp arc is superseded by upstream's Metal-4 implementation

LunarG (Jeremy Gebben, r-b Aitor Camacho) independently implemented
`VK_QUERY_TYPE_TIMESTAMP` upstream — MR !42864 / commit `ed807097`, merged
**2026-07-14, five days after our 0008 was written** — plus stage-LUT fix
`7f540b05`. Both stubs 0008 filled are gone on main; the zink GL 3.3 gate
(`timestampValidBits > 0`) is satisfied upstream (`= 64`). Supersession is
**architectural**: upstream KK moved to Metal 4 command encoding (`c08dba83`,
2026-06-19, after our base) and builds timestamps on MTL4 primitives with no
Metal-3 equivalent (`MTL4CounterHeap`, mid-encoder per-stage
`writeTimestampWithGranularity:`, `resolveCounterHeap:`); ours targets Metal-3
`MTLCounterSampleBuffer`. Neither ports to the other. Exit: on the next KK base
bump, drop all five and inherit upstream's. Until then the arc stays (it is
load-bearing for the shipped GL 3.3 tier and the M4 Pro dogfood fix).

Fold graph for the carry: 0010→0011→0012→0013 are a strict fixup chain on 0008,
and 0013 *deletes* most of 0010–0012 (the split-resolve probe and GPU resolve),
replacing them with CPU resolve at completion + explicit ordering. A carry-fold
yields one clean patch; it has no MR purpose.

**The one upstreamable asset is a bug report, not a patch.** Our measured Metal-3
facts — a counter sample only materialises when the command buffer that took it
*completes*; a GPU resolve encoded earlier silently writes 0 (18% of
later-cmdbuf resolves on M4 Pro; M1 Max unaffected); empty encoders are elided
and never sample — may apply to `MTL4CounterHeap` too, and upstream's new code
resolves **in the same MTL4 command buffer that wrote the timestamps**. If the
hazard carries over, upstream has our bug on M4-class GPUs and doesn't know it.
Action item: when we have a Metal4-capable build, re-run the
`spikes/kk-timestamp-probe` battery against upstream KK on the M4 Pro; if zeros
reproduce, file a mesa issue citing the numbers.

Tracker craft (this series): mesa's work-items search is weak — work_items/14623
was findable only via the commit's `Closes:` trailer; mine trailers, not search.
The mesa GitHub mirror (mesa3d/mesa) is dead — all upstream mesa reading goes
through GitLab in Chrome.

