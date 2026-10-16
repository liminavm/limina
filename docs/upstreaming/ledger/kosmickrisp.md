<!-- ledger-check: dep=kosmickrisp -->
# kosmickrisp — patch-audit ledger

131 commits; base `d44453ce07b` (mesa main, rebased 2026-10-14 by limina `b9be1f0`), pin
`059715d2aa5` (`third_party/manifest.toml` [kosmickrisp]); see *Rebase executed* below.
Schema + protocol: `README.md`. The `checked` column of rows 0001–0019 records main `84acd848`
(2026-08-03), which is two bases old (`418f2963a15` on 2026-08-05, then `d44453ce07b` on
2026-10-14); those `need` verdicts have not been re-verified against the current base.
94 commits have only a skeleton row (*Unaudited commits* at the end).
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
> gated off by default). 2026-10-16 audit: still on the branch as `87feda87b94`; it now has a
> skeleton row under *Unaudited commits*.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | limina: KosmicKrisp + zink host-stack patches (MTL4 re-port of the monolith) | `src/gallium/drivers/zink/zink_screen.c`, `src/kosmickrisp/bridge/mtl_buffer.m`, `src/kosmickrisp/bridge/mtl_command_buffer.m` +18 |  | mixed — SPLIT REQUIRED; per-component verdicts in Findings (vn_wsi superseded@311a437c; dm-assert superseded@bbf67327; zink guard + XFB + dm nil-check still needed; bind-cache obsolete-by-architecture on MTL4) | main `84acd848` 2026-08-03 | none-yet | none-yet | yes:guest→host-memory-safety in the XFB component (the 0007 clamp is the fix and must travel with it; no embargo — upstream has no XFB) | split-then-fold: decompose per Findings; fold 0007 + 0015 into their parent components; strip-diag-first (~340 DIAG lines: RTLOG/STATS/CAPTURE) | host | **split-then-triage** — components range from drop to upstream-now (dm nil-check ~15 LOC; 2DArray→2D demotion ~6 LOC pending tip re-verify) to upstream-after-split (XFB, zink guard) to carry-re-derive (perf pack) | next KK base bump = PARTIAL REWRITE of the delta (~40% dead: kk_encoder.c deleted, MTL4 argument tables kill the bind-cache premise; ~35% survives but needs re-measurement; ~25% XFB re-port) |
| 0002 | kk: lay out DRM-format-modifier attachment images as tiled, not linear | `src/kosmickrisp/vulkan/kk_image_layout.c` |  | needed — `kk_image_layout.c` unchanged since base, no modifier carve-out on tip | file-touch `9405760a` (pre-base); tip `84acd848` 2026-08-03 | n/a — see #14209 (KK/MoltenVK parity) for the modifier conversation | n/a | yes:guest-DoS-abort (nil encoder → worker SIGABRT) | standalone | host | **carry** — unreachable upstream: KK never advertises EXT_image_drm_format_modifier; only our venus (mesa 0010) force-advertise creates the trigger. Upstreamable only inside a real KK modifier feature |  |
| 0003 | kk: clamp attachment-less render pass target size + sample count to >= 1 | `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/kosmickrisp/vulkan/kk_encoder.c` |  | conflicts-on-tip — bug PERSISTS relocated (kk_encoder.c deleted upstream; unclamped renderArea at kk_cmd_draw.c:306 + deferred start overwrites sample count at kk_cmd_buffer.c:293; nil encoder now crashes in mtl_set_vertex_amplification_count) | kk_cmd_draw.c `603a410a` 2026-07-25; kk_cmd_buffer.c `7193e63e` 2026-07-23; tip `84acd848` | none-yet | none-yet | yes:guest-DoS-abort | standalone | host | upstream-after-cleanup (rebase to the relocated code first) | natively reproducible upstream: original trigger was mesa's own zink, and upstream now supports zink-on-KK (MRs 38605/41359) |
| 0004 | kk: give heap-less (host-imported) tiled image planes a private Metal heap | `src/kosmickrisp/vulkan/kk_image.c`, `src/kosmickrisp/vulkan/kk_image.h` |  | needed — assert intact at kk_image.c:858; heap-from-host-pointer limitation at kk_device_memory.c:133 | kk_image.c `282d791a` 2026-07-24; kk_device_memory.c `4a1ee1bd` **2026-08-03 (touched today — re-fetch before rebasing)** | none-yet | none-yet | yes:guest-DoS-abort | standalone | host | upstream-after-cleanup (strip the limina: comment prefixes) | reachable by NATIVE macOS apps via EXT_external_memory_host + OPTIMAL-tiling bind — not a wgpu-only quirk; KK under very active LunarG refactoring, supersession risk is time-sensitive |
| 0005 | kk: advertise VK_EXT_custom_border_color (lift zink-on-KK above GL 3.1) | `src/kosmickrisp/vulkan/kk_physical_device.c` |  | superseded-upstream@4c8e720c (2026-07-31!) — LunarG impl is FULLER (NIR lowering + border_color_swizzle) but gated `MESA_KK_EXPERIMENTAL=custom_border`, default OFF | main `84acd848` 2026-08-03 | n/a | theirs: !43078 (merged) | no | standalone | host | **DROPPED 2026-08-05 (rebase)** — upstream impl adopted; worker defaults `MESA_KK_EXPERIMENTAL=custom_border` | verified before deletion: zink lights up under the gate, zero custom-border warnings in the seated smoke |
| 0006 | kk: advertise VK_EXT_depth_clip_enable (GL_ARB_depth_clamp -> GL 3.2) | `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/kosmickrisp/vulkan/kk_physical_device.c` |  | needed; pending-supersession — Draft !43421 (squidbus, opened **2026-08-03, today**) is a strict SUPERSET (adds shader-emulated clamp for clamp+clip-both-on, which ours silently drops, + dynamic-state3) | main `84acd848` 2026-08-03; not on main yet | n/a | theirs: !43421 (Draft) — file NOTHING, watch it | no | standalone | host | carry → drop on !43421 merge | kk_cmd_draw.c churned heavily upstream (Metal4 + live-record) — conflict risk high at rebase; **2026-10-16 audit: DROPPED from the branch at the 2026-10-14 rebase** — no commit with this subject in `d44453ce07b..059715d2aa5`; base `d44453ce07b` advertises `EXT_depth_clip_enable` and emulates clamp in the fragment shader when clamp and clip are both on (`kk_cmd_draw.c`, `emulate_depth_clamp = clamp && clip`), the !43421 superset. Verdict columns left as written |
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
| 0019 | vulkan/runtime: log-only render-pass begin VU mismatches instead of asserting | `src/vulkan/runtime/vk_render_pass.c` |  | needed — all three asserts live at vk_render_pass.c:2708/2732/2746 | file-touch `3abdee9e` 2026-03 (pre-base); tip `84acd848` | n/a | n/a | yes:guest-DoS-abort | standalone | host | **carry** — upstream deliberately asserts on VU violations ("valid usage in, or all bets off"); plausible future pitch = opt-in untrusted-caller mode, no upstream precedent | the attachment-COUNT asserts are handled by the next two rows |
| — | vulkan/runtime: refuse a render pass begin with too few attachment views | `src/vulkan/runtime/vk_render_pass.c` |  | needed — upstream asserts the counts match, and with NDEBUG a short view array is read out of bounds by every later attachment access in the pass | `limina-kk` | n/a | n/a | yes:guest-OOB-read | standalone (pairs with 0019's log macro) | host | **carry**; same upstream stance as 0019 | refuses the begin (logs, fails `vkEndCommandBuffer`, NextSubpass/EndRenderPass no-op without a pass) rather than clamping: ~10 sites index attachments by pass-declared index |
| — | vulkan/runtime: zero an imageless framebuffer's attachment count | `src/vulkan/runtime/vk_render_pass.c` |  | needed — `vk_common_CreateFramebuffer` sets `attachment_count` only for non-imageless framebuffers on an unzeroed allocation | `limina-kk` | none-yet | none-yet | yes:uninitialised-read | standalone | host | **upstream candidate** — a plain bug independent of the untrusted-caller question | found while fixing the row above |
| — | zink: keep the cube bit when the same sampler view is rebound | `src/gallium/drivers/zink/zink_context.c` |  | needed — upstream `zink_set_sampler_views` clears `di.cubes` for every slot, then skips an unchanged view before the bit is set again | `limina-kk` 0a78970b2bc | none-yet | none-yet | no | standalone | host | **upstream candidate** — a plain zink bug on any driver without `VK_EXT_non_seamless_cube_map` | a frontend that rebinds the same cube view per draw (vrend) loses the emulation from the second draw: unlowered shader over the 2D-array view; piglit `cubemap pbo` on a desktop GL context |
| — | kk: save the pushed set 0 by value across meta operations | `src/kosmickrisp/vulkan/kk_cmd_meta.c` |  | needed — upstream `kk_meta_begin`/`kk_meta_end` save and restore the `desc->push[0]` pointer, so a meta push overwrites the application's set 0 in place | `limina-kk` 714ec72031c | none-yet | none-yet | no | standalone | host | **upstream candidate** — a plain KK bug; NVK copies the set by value | a draw after `vkCmdBlitImage` in the same command buffer, with no re-push of set 0 in between, reads the blit's descriptor (zink: its UBO); piglit `sgis_generate_mipmap@gen-teximage`, `gen-texsubimage`, `gen-nonzero-unit` |
| — | nir: add nir_texop_texel_buffer_flags_kk | `src/compiler/nir/{nir.h,nir.c,nir_print.c,nir_validate.c}`, `src/compiler/spirv/spirv_to_nir.c` |  | needed — the KK RGB32 texel buffer emulation queries its descriptor flags before descriptor lowering, like `image_min_lod_agx` | `limina-kk` de5c8a05ac8 | none-yet | none-yet | no | goes with the row below | host | **upstream candidate**, together with the row below | — |
| — | kk: emulate R32G32B32 uniform texel buffers | `src/kosmickrisp/vulkan/{kk_buffer_view.c,kk_descriptor_types.h,kk_descriptor_set*.c,kk_nir_lower_descriptors.c,kk_nir_lower_texel_buffer_rgb32.c,kk_shader.c}` |  | needed — Metal has no three-channel texel formats, so upstream KK offers no RGB32 uniform texel buffer and Mesa withholds `ARB_texture_buffer_object_rgb32` and `OES_texture_buffer` | `limina-kk` 80be4bf1b3b | none-yet | none-yet | no | needs the row above | host | **upstream candidate** — mechanism only; texel buffer descriptors grow 8 → 16 bytes and every buffer fetch branches on a flag | zink-on-KK GLES stays below 3.2 without it; piglit `arb_texture_buffer_object@formats` RGB32 subtests, `texture-buffer-size-clamp` |
| — | kk: dispatch indirect tessellation grids in threads | `src/kosmickrisp/{bridge/mtl_encoder.*,vulkan/kk_cmd_buffer.*,vulkan/kk_cmd_draw.c,libkk/kk_tessellation.cl}` |  | needed — upstream KK dispatches the thread counts libkk_tess_setup_indirect writes with dispatchThreadgroupsWithIndirectBuffer, so every indirect tessellated draw runs its vertex shader 64× per vertex and writes past the lowered stages' outputs | `limina-kk` 7829e47fb23 | none-yet | none-yet | no | standalone | host | **superseded** by the threadgroups row below — `dispatchThreadsWithIndirectBuffer` loses the stores of a kernel whose private array spills to the stack, on every thread past the first couple of a threadgroup | `spikes/kk-gs/indirect-tess-count.c`: 7104 vertex invocations for 111 vertices drawn indirectly |
| — | kk: dispatch indirect tessellation grids in threadgroups | `src/kosmickrisp/{libkk/kk_tessellation.cl,vulkan/kk_cmd_draw.c,vulkan/kk_shader.c,bridge/mtl_encoder.*}` |  | needed — upstream dispatches the setup kernel's THREAD counts as threadgroups (64× over-dispatch); thread-indirect dispatch loses stack-spilled private stores | `limina-kk` 7ba5661b976 | none-yet | none-yet | no | standalone | host | **upstream candidate** — a plain KK bug: the setup kernel writes threadgroup counts and each lowered kernel bounds-checks its partial last threadgroup | `spikes/kk-gs/indirect-tess-count.c` 111/111 direct and indirect; `spikes/kk-gs-tes-io/` |
| — | kk: trim rasterized varyings to the components the FS reads | `src/kosmickrisp/vulkan/kk_shader.c` |  | needed — Metal links a vertex output to a fragment input only on an exact type match, while Vulkan lets the FS read fewer components; a transform-feedback store keeps all four components after nir_opt_varyings shrinks the FS input, so the pipeline is refused (zink: VK_ERROR_INVALID_SHADER_NV, draw dropped) | `limina-kk` 221adc9d008 | none-yet | none-yet | no | standalone | host | **upstream candidate** — a plain KK bug; shaders compiled without their FS (ESO) are not covered | vrend replay of piglit arb_shader_image_load_store host-mem-barrier (xfb/WaW): 6 → 0 failed MTLRenderPipelineStates |
| — | kk: do not end a Metal command buffer that was never begun | `src/kosmickrisp/vulkan/kk_cmd_buffer.c` |  | needed — a vkBeginCommandBuffer that fails (no allocator, or a lost device) leaves the command buffer half-open, and the client's retry ends a Metal command buffer that was never begun, which Metal asserts on and aborts the process | `limina-kk` 90dd9ac9ae8 | none-yet | none-yet | no | standalone | host | **upstream candidate** — reachable upstream through the device-lost path | host probe: zink-on-KK survived 1000+ failed Begins in 10 min (aborted on the first before) |
| — | kk: put a ceiling on the command-allocator pool (default from a measured guest desktop) | `src/kosmickrisp/vulkan/kk_device.{c,h}`, `kk_queue.c` |  | needed — the pool mints an allocator on every miss without limit, so a client that never ends its command buffers, or a GPU that falls behind, grows it until Metal refuses and the process aborts | `limina-kk` 61ceb2aac48, 98e8f166733 | none-yet | none-yet | no | standalone | host | **limina-only** — the pool itself is our delta; the ceiling is policy (`LIMINA_KK_ALLOC_CEILING`) | `spikes/kk-alloc-pool/ceiling-2026-10-07/`: poolprobe bounded at the ceiling; a seated desktop peaked at 78 on the shared vrend device; covers two commits: `61ceb2aac48` *kk: put a ceiling on the command-allocator pool* and `98e8f166733` *kk: size the allocator ceiling from a measured guest desktop* |
| — | zink: rebuild the attachment info after an unordered blit | `src/gallium/drivers/zink/zink_blit.c` |  | needed — zink_blit restores rp_changed after its own begin_rendering rebuilt dynamic_fb.info for the blit framebuffer, so the next pass begins with the blit's depth/stencil pointers and load/store ops (assert `!pStencilAttachment \|\| stencilAttachmentFormat`; a stale view reaches the driver with asserts off) | `limina-kk` 746a2777897 | none-yet | none-yet | no | standalone | host | **upstream candidate** — the code is identical on upstream main | `spikes/zink-stencil-attachment-assert/stencilfuzz.c`: assert on the 12th directed repetition before, 2000 clean after |
| — | kk: scalarize fragment shader loads of clip and cull distances | `src/kosmickrisp/compiler/{msl_iomap.c,msl_nir_lower_common.c}` |  | needed — MSL carries clip/cull distances as one scalar member per component and only the producer's stores were scalarized, so a fragment shader reading gl_ClipDistance with a vector load got the first distance broadcast into every component | `limina-kk` 6eec9a07bb7 | none-yet | none-yet | no | standalone | host | **upstream candidate** — a plain KK bug | MSL dump of glsl-1.50 geometry clip-distance-bulk-copy reads all eight members; glsl-1.30 clipping and arb_cull_distance still pass |
| — | kk: pass the patch ID to the fragment shader after tessellation | `src/kosmickrisp/vulkan/kk_shader.c` |  | needed — after tessellation gl_PrimitiveID in the FS is the patch ID, but Metal's [[primitive_id]] numbers rasterized triangles across every instance of the one poly draw | `limina-kk` 1e8c348241f | none-yet | none-yet | no | standalone | host | **upstream candidate** — a plain KK bug | piglit arb_tessellation_shader fs-primitiveid-instanced fixed; the list passes 1749 (baseline 1748), no new failures |
| — | zink: apply the MoltenVK workarounds only when MoltenVK is the driver | `src/gallium/drivers/zink/zink_screen.c` |  | needed — zink includes MoltenVK's headers on every `__APPLE__` build, so `MVK_VERSION` is defined whenever they are installed and its MoltenVK-only workarounds apply to whichever driver runs: on KK that withholds fbfetch (and with it `KHR_blend_equation_advanced`) and turns off dynamic vertex input stride | `limina-kk` f8567a3d4b2 | none-yet | none-yet | no | standalone | host | **upstream candidate** — the gates are identical on upstream main; `LIMINA_ZINK_MVK_WORKAROUNDS=1` (ours) restores the old behaviour | `spikes/zink-mvk-gate/`: 8/8 advanced blend modes read back the formula; `spikes/gles32-gate/` ES 3.2 requirements missing 3 → 2 |
| — | mesa: draw without primitive restart when no index range holds the restart index | `src/mesa/main/draw.c`, `src/mesa/vbo/{vbo.h,vbo_minmax_index.c}` |  | needed — GLES 3 / WebGL 2 keep fixed-index restart on for every indexed draw, and on a driver that cannot restart some topologies u_vbuf rewrites every such draw's indices on the CPU whether or not the restart index occurs; upstream's min/max cache already skips restart indices but does not record seeing one | `limina-kk` 37d76cbed33 | none-yet | none-yet | no | standalone | host | **upstream candidate** — generic GL frontend, driver-independent; restart and non-restart scans are kept apart in the cache key | `spikes/list-restart-probe/`: a mid-list restart draws the right triangle; aquarium unchanged (`perf/listrestart-2026-10-07/`) |
| — | zink: leave list restart out of the supported modes on KosmicKrisp | `src/gallium/drivers/zink/zink_screen.c` |  | needed — KK restarts a list only by unrolling its indices in a compute dispatch per draw (or skips it, misdrawing); with lists left out, the frontend check above sends only draws that really restart to primconvert | `limina-kk` 621ebea5f76 | n/a | n/a | no | depends on the frontend commit | host | **carry** — policy for our KK; venus guests still reach KK directly and keep its `LIMINA_KK_NOLISTRESTART` skip | conformant vrend-tier list restart at skip speed: aquarium 36-47 fps vs 35-44 |
| — | kk: run draw-time compute in a pre-graphics stream instead of splitting the pass | `src/kosmickrisp/vulkan/{kk_cmd_buffer.c,kk_cmd_buffer.h,kk_cmd_draw.c,kk_queue.c}` |  | needed — upstream records compute a draw needs (list-restart unroll, tessellation setup) into the same command buffer as the pass, so each such draw ends the render encoder and reloads every attachment; aquarium with the unroll on runs ~5000 encoders per frame and drops from ~40 fps to 1 | `limina-kk-pregfx` da9c3527240 (WIP) | n/a | n/a | no | standalone | host | **carry; REVERT when upstream lands the mesh-shader unroll** — it reintroduces the pre-graphics command buffer upstream removed on purpose in 0126f4388a5 ("kk: One MTL4CommandBuffer per VkCommandBuffer", MR 43697), whose stated plan is a mesh-shader unroll; not sent upstream for that reason; 2026-10-16 audit: not on `limina-kk` (`d44453ce07b..059715d2aa5`); branch `limina-kk-pregfx` and `da9c3527240` are not in the local clone, so its state was not verified |
| — | poly: emulate last-vertex provoking for a geometry shader's flat outputs | `src/poly/nir/{poly_nir.h,poly_nir_lower_gs.c}` |  | needed — poly's GS rasterization provokes from the first vertex; Honeykrisp has a provoking-vertex rasterizer bit, Metal has none | `limina-kk` 95c04aa5d65 | none-yet | none-yet | no | standalone | host | **upstream candidate** with the KK GS port — opt-in through poly_nir_lower_gs_opts, Honeykrisp unchanged | piglit pv list on venus 235/245 |
| — | poly: let an unroll drop the adjacent vertices of an adjacency topology | `src/poly/{cl/restart.h,prim.h}`, `src/kosmickrisp/libkk/kk_draws.cl` |  | needed — an adjacency draw without a GS must rasterize as its plain topology; Metal has no adjacency primitives | `limina-kk` 322a38e25a3 | none-yet | none-yet | no | standalone | host | **upstream candidate** with the KK GS port | |
| — | zink: keep the primitive emulations off geometry shaders on KosmicKrisp | `src/gallium/drivers/zink/{zink_screen.c,zink_types.h}` |  | needed — with geometryShader, zink routes quads, edge flags, line stipple and smoothing through generated GSs, which on KK cost a compute pass per draw | `limina-kk` a8e80893d3f | n/a | n/a | no | standalone | host | **carry** — policy for our KK; `LIMINA_ZINK_GS_EMULATION=1` restores zink's routing; guest zink-on-venus keeps its default routing | |
| — | kk: geometry shaders on poly's compute emulation | `src/kosmickrisp/{libkk/kk_geometry.cl,vulkan/kk_cmd_draw.c,vulkan/kk_shader.c,...}` |  | needed — upstream KK has no geometryShader | `limina-kk` f4c6153269f | none-yet | none-yet | no | depends on the two poly commits | host | **upstream candidate after a split** — a port of Honeykrisp's poly GS wiring; carries limina's XFB (0001), so the GS-capture parts wait on XFB upstream | host GLES 3.1 → 3.2; piglit GS list on venus 2500/2622 (13 fail: streams>0, clip-distance copy ~1 ulp, guest zink psiz, tes-gs-max-in-out-components) |
| — | kk: advertise geometry shaders by default | `src/kosmickrisp/vulkan/kk_physical_device.c` |  | needed while the GS port is limina-only | `limina-kk` b1c16c2e2d3 | n/a | n/a | no | fold into the GS commit upstream | host | **carry** — `LIMINA_KK_GEOMETRY_SHADER=0` withdraws it | vrend guests on the GLES host flavour then expose GLSL 430 and fake ARB_gpu_shader_fp64 (guest virgl's fake_fp64 drops double instructions) |
| — | kk: follow descriptor updates across consecutive bindings | `src/kosmickrisp/vulkan/kk_descriptor_set.c` |  | needed — upstream refuses a write, copy or template entry whose count runs past its binding, which the spec's consecutive binding updates allow | `limina-kk` edf5c2693cd | none-yet | none-yet | no | standalone | host | **upstream candidate** — a plain KK spec bug | ggml/llama.cpp's Vulkan backend writes all of a dispatch's buffers in one such write (issue #19); `spikes/kk-guest-bounds/desc-span-probe.c` |
| — | kk: mask shift counts the way NIR defines them | `src/kosmickrisp/compiler/nir_to_msl.c` |  | needed — NIR takes a shift count modulo the bit size, MSL promotes 8/16-bit operands and shifts by the whole count | `limina-kk` bad46c4e6ee | none-yet | none-yet | no | standalone | host | **upstream candidate** — a plain KK miscompile | ggml's IQ scale reads came out 0; `spikes/kk-shift-mask/` |
| — | kk: advertise the workgroup counts Metal takes in y and z | `src/kosmickrisp/vulkan/{kk_physical_device.c,kk_cmd_dispatch.c}` |  | needed — upstream advertises 65535 in y and z and its empty-dispatch check multiplies the counts in 32 bits | `limina-kk` 206cd4d2e40 | none-yet | none-yet | no | standalone | host | **upstream candidate** | ggml's RMS_NORM over 65536 rows; `spikes/kk-workgroup-count/` |
| — | nir/opt_large_constants: accept stores elsewhere of the values already held | `src/compiler/nir/nir_opt_large_constants.c`, `src/compiler/nir/tests/opt_large_constants_tests.cpp` |  | needed — upstream takes a variable only when all its stores share one block; glslang's initializer plus its store at the use site, moved into a loop by merge-return, splits them | `limina-kk` 897886cdb2d | none-yet | none-yet | no | standalone | host | **upstream candidate** — core NIR, helps every backend that lowers large temps to scratch | ggml's IQ grids rebuilt in 8 KiB of scratch per thread; `spikes/kk-iq-large-constants/` |
| — | kk: unroll the loops the source asks to, while they stay small | `src/kosmickrisp/compiler/nir_to_msl.c` |  | needed — upstream KK runs no loop unrolling on compute shaders, so a hinted loop bounded by a specialization constant leaves its arrays in scratch or if-else trees | `limina-kk` 059715d2aa5 | none-yet | none-yet | no | standalone | host | **upstream candidate** — the 512 budget is tuned on one M1 Max against ggml's kernels; re-measure on other GPUs before proposing it | ggml mul_mm 3.7x, matrix-vector 1.3x geomean; `spikes/kk-iq-large-constants/` |

## Findings

### Rebase executed 2026-10-14 (limina `b9be1f0`; recorded by the 2026-10-16 audit)

Base `418f2963a15` → `d44453ce07b` (mesa main, carrying MR 45147's zink fence/submit rework);
the pin is now `059715d2aa5`, 131 commits in `d44453ce07b..059715d2aa5`. limina `b9be1f0`
records the HVF suite at 186/187 on this stack (the one failure, `l1_restore_vtimer_offset`,
also fails on the previous pins). The ledger was not updated at the time; this entry and the
remapped SHAs below were written from git on 2026-10-16.

- **0006 depth_clip_enable left the branch.** Base `d44453ce07b` advertises
  `EXT_depth_clip_enable` with fragment-shader clamp emulation for clamp+clip, the !43421
  superset the 0006 row anticipated. The row's verdict columns are unchanged; its notes record
  the drop.
- **The zink lost-wakeup fix probably went into MR 45147.** The 2026-08-05 entry below lists it as
  cherry-picked clean; no commit on the branch carries that subject or mentions a lost wakeup.
  `6aa1f4a2105` ("zink: fix the data races on the cross-thread batch/fence state") now sits on
  top of 45147's mutex and makes the lockless readers atomic. Not verified commit by commit:
  the local clone is shallow.
- **Geometry shaders scalarize their inputs before poly's lowering**, because the base now
  advertises `storageInputOutput16` (per `b9be1f0`'s message).
- **SHA remap** of the `limina-kk <sha>` cites (pre-rebase objects are not in the clone):
  `74303f3f28d`→`0a78970b2bc`, `c1e2e732313`→`714ec72031c`, `63dbadbbbe1`→`de5c8a05ac8`, `c76a16a26f8`→`80be4bf1b3b`, `5b1aece01cb`→`7829e47fb23`, `cffe2ed9333`→`7ba5661b976`, `aa3be8eac95`→`221adc9d008`, `d445c2dd92c`→`90dd9ac9ae8`, `8599974d563`→`61ceb2aac48`, `c6c29e8b099`→`98e8f166733`, `69c65e835d6`→`746a2777897`, `152321a1876`→`6eec9a07bb7`, `88b1341efe8`→`1e8c348241f`, `de950d67e16`→`f8567a3d4b2`, `5f52a52d7f6`→`37d76cbed33`, `24fd7c3f49d`→`621ebea5f76`, `84d995ff2bf`→`95c04aa5d65`, `9bff4c6c2e2`→`322a38e25a3`, `c4555b3bb3b`→`a8e80893d3f`, `bf73a5a34b7`→`f4c6153269f`, `5bedcfa6690`→`b1c16c2e2d3`. The five newest cites (`edf5c2693cd`, `bad46c4e6ee`, `206cd4d2e40`, `897886cdb2d`,
  `059715d2aa5`) were made after the rebase and are unchanged.
- `7829e47fb23` ("dispatch indirect tessellation grids in threads") is still on the branch,
  under `7ba5661b976` (threadgroups), although its row marks it superseded.

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

## Unaudited commits (skeleton, 2026-10-16)

Phase-0 output of `scripts/patch-audit/check-ledger.sh --skeleton kosmickrisp` against
`d44453ce07b..059715d2aa5`: one row per commit that had no row, grouped by theme. The last
column holds unresearched first guesses (disposition + sec flag) from the 2026-10-16 audit, not
verdicts; a researched row replaces its skeleton row in the table above. `98e8f166733` is
covered by the combined allocator-ceiling row and has no skeleton row.

### A. Modifier / external-memory / IOSurface import series (arrived with the 2026-08-05 migration)

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `aab20ebc012` | vulkan/meta: also skip negative-offset and i32-overflow clear rects | `src/vulkan/runtime/vk_meta_clear.c` |  | upstream-now (fold with 0009); sec: guest-DoS |
| `3b1a42a17b5` | vulkan/runtime: never abort on a modifier-props query of a non-modifier image | `src/vulkan/runtime/vk_image.c` |  | upstream-now; sec: guest-DoS |
| `e184d80eaa3` | vulkan/runtime: compile VK_EXT_image_drm_format_modifier support on Darwin | `src/vulkan/runtime/vk_image.c`, `src/vulkan/runtime/vk_image.h` |  | upstream-after-cleanup (with a KK modifier feature); sec: no |
| `056018246e8` | kosmickrisp: implement VK_EXT_image_drm_format_modifier (LINEAR only) | `src/kosmickrisp/vulkan/kk_format.c`, `src/kosmickrisp/vulkan/kk_image.c`, `src/kosmickrisp/vulkan/kk_image.h` +3 |  | upstream-after-cleanup (needs upstream modifier demand); sec: no |
| `c7430ec4461` | kosmickrisp: advertise VK_EXT_queue_family_foreign; tolerate invalid explicit modifier pitches | `src/kosmickrisp/vulkan/kk_image.c`, `src/kosmickrisp/vulkan/kk_physical_device.c` |  | upstream-after-cleanup; sec: no |
| `87feda87b94` | kosmickrisp: implement the MTLTEXTURE handle type of VK_EXT_external_memory_metal | `src/kosmickrisp/bridge/mtl_texture.h`, `src/kosmickrisp/bridge/mtl_texture.m`, `src/kosmickrisp/vulkan/kk_bo.c` +6 |  | upstream-after-cleanup (upstream has MTLHEAP only); sec: no |
| `430acc07da0` | limina: EGLImage-from-IOSurface import path (EGL -> dri -> zink -> KK) | `src/egl/drivers/dri2/egl_dri2.c`, `src/gallium/drivers/zink/zink_resource.c`, `src/gallium/frontends/dri/dri2.c` +7 |  | carry (limina IOSurface path); sec: no |
| `602ce7157b7` | limina: fix IOSurface import compiled out by ZINK_USE_DMABUF; install from_handle on KK | `src/gallium/drivers/zink/zink_resource.c`, `src/gallium/frontends/dri/dri2.c` |  | carry (limina IOSurface path); sec: no |
| `f46445fda2e` | limina: carry a plane index through the IOSurface import chain | `src/egl/drivers/dri2/egl_dri2.c`, `src/gallium/drivers/zink/zink_resource.c`, `src/gallium/frontends/dri/dri2.c` +5 |  | carry (limina IOSurface path); sec: no |
| `ee5bca680a7` | zink: an IOSurface plane import is a complete image, not an aux plane | `src/gallium/drivers/zink/zink_resource.c` |  | carry (limina IOSurface path); sec: no |
| `4c467052631` | dri: import a one- or two-channel IOSurface as itself (limina) | `src/gallium/frontends/dri/dri2.c` |  | carry (limina IOSurface path); sec: no |

### B. Diagnostics and instrumentation knobs

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `b7a1404d2b4` | kk: live-object census for the IOAccelerator investigation | `src/kosmickrisp/bridge/mtl_argument_table.m`, `src/kosmickrisp/bridge/mtl_bridge.h`, `src/kosmickrisp/bridge/mtl_bridge.m` +14 |  | carry (debug; strip-diag-first); sec: no |
| `c9f33394c3c` | kk: LIMINA_KK_ALLOC_STATS — read the allocator pools with Apple's own API | `src/kosmickrisp/bridge/mtl_command_buffer.h`, `src/kosmickrisp/bridge/mtl_command_buffer.m`, `src/kosmickrisp/bridge/mtl_command_queue.m` +1 |  | carry (debug; strip-diag-first); sec: no |
| `b9ee98df1cd` | kk: LIMINA_KK_RPLOG — dump the resolved render pass before encoder creation | `src/kosmickrisp/bridge/mtl_encoder.m` |  | carry (debug; strip-diag-first); sec: no |
| `67a8ba01540` | kk: hush the per-import texture log behind LIMINA_KK_IMPORT_TRACE | `src/kosmickrisp/vulkan/kk_image.c` |  | carry (debug; strip-diag-first); sec: no |
| `8ed287649f2` | kk: say why the device was lost | `src/kosmickrisp/bridge/mtl_command_queue.m`, `src/kosmickrisp/bridge/mtl_types.h`, `src/kosmickrisp/vulkan/kk_queue.c` |  | carry (debug; strip-diag-first); sec: no |
| `6c46a0e49ae` | kk: a label setter, and a knob that leaks every BO | `src/kosmickrisp/bridge/mtl_texture.h`, `src/kosmickrisp/bridge/mtl_texture.m`, `src/kosmickrisp/vulkan/kk_bo.c` |  | carry (debug; strip-diag-first); sec: no |
| `e1de16265f9` | kk: a nil texture view is always worth reporting | `src/kosmickrisp/bridge/mtl_texture.m` |  | carry (debug; strip-diag-first); sec: no |
| `60a5562a515` | kk: two knobs that answer where a mistyped texture binding comes from | `src/kosmickrisp/vulkan/kk_descriptor_set.c`, `src/kosmickrisp/vulkan/kk_image_view.c` |  | carry (debug; strip-diag-first); sec: no |
| `405e7f038a1` | kk: make a shader-validation report name its shader and its slot | `src/kosmickrisp/bridge/mtl_compiler.h`, `src/kosmickrisp/bridge/mtl_compiler.m`, `src/kosmickrisp/bridge/stubs/mtl_compiler.c` +2 |  | carry (debug; strip-diag-first); sec: no |
| `b14dd759955` | kk: name a pipeline by its MSL, not by a pointer | `src/kosmickrisp/vulkan/kk_shader.c`, `src/kosmickrisp/vulkan/kk_shader.h` |  | carry (debug; strip-diag-first); sec: no |
| `2e99bf7f803` | kk: make a device loss name the work the GPU was doing | `src/kosmickrisp/vulkan/kk_cmd_buffer.c`, `src/kosmickrisp/vulkan/kk_cmd_buffer.h`, `src/kosmickrisp/vulkan/kk_limina_work.c` +3 |  | carry (debug; strip-diag-first); sec: no |
| `d39a28dbfe8` | kk: name the address a failing pass wrote, and log every allocation's range | `src/kosmickrisp/vulkan/kk_bo.c`, `src/kosmickrisp/vulkan/kk_cmd_buffer.c`, `src/kosmickrisp/vulkan/kk_cmd_buffer.h` +3 |  | carry (debug; strip-diag-first); sec: no |
| `6f58fd4b9df` | kk: a lever that clamps every vertex fetch to its bound range | `src/kosmickrisp/vulkan/kk_shader.c`, `src/kosmickrisp/vulkan/kk_shader.h` |  | carry (debug; strip-diag-first); sec: no |
| `20d7d70dc8b` | kk: say what a failing pass drew, and let image textures be leaked | `src/kosmickrisp/vulkan/kk_cmd_buffer.c`, `src/kosmickrisp/vulkan/kk_cmd_buffer.h`, `src/kosmickrisp/vulkan/kk_cmd_draw.c` +1 |  | carry (debug; strip-diag-first); sec: no |
| `280e2ecb02e` | kk: count residency-set members and sampler-table retirements | `src/kosmickrisp/vulkan/kk_device.c`, `src/kosmickrisp/vulkan/kk_device.h`, `src/kosmickrisp/vulkan/kk_queue.c` |  | carry (debug; strip-diag-first); sec: no |
| `485a6c7905f` | kk: pin the sampler table resident under a knob, and log its address | `src/kosmickrisp/vulkan/kk_device.c` |  | carry (debug; strip-diag-first); sec: no |
| `4ae133406d0` | kk: the address log never covered imported memory | `src/kosmickrisp/vulkan/kk_device_memory.c` |  | carry (debug; strip-diag-first); sec: no |
| `ee5b2c14222` | kk: name a stale compute encoder instead of faulting inside AGX on it | `src/kosmickrisp/bridge/mtl_encoder.h`, `src/kosmickrisp/bridge/mtl_encoder.m`, `src/kosmickrisp/vulkan/kk_cmd_buffer.c` +1 |  | carry (debug; strip-diag-first); sec: no |
| `c769e8588f0` | kk: make the encoder guard say it is alive, and name both parties on a refusal | `src/kosmickrisp/bridge/mtl_encoder.h`, `src/kosmickrisp/bridge/mtl_encoder.m`, `src/kosmickrisp/vulkan/kk_cmd_buffer.c` |  | carry (debug; strip-diag-first); sec: no |
| `e6a5ba3cef9` | kk: watch the AGX context behind each compute encoder, not just the encoder | `src/kosmickrisp/bridge/mtl_encoder.h`, `src/kosmickrisp/bridge/mtl_encoder.m`, `src/kosmickrisp/vulkan/kk_cmd_buffer.c` |  | carry (debug; strip-diag-first); sec: no |
| `d3b19f69bf1` | kk: leave each compute copy on disk before AGX sees it | `src/kosmickrisp/bridge/mtl_encoder.m` |  | carry (debug; strip-diag-first); sec: no |
| `03f6106bbbd` | zink: count render-pass begins, ends and submits by call site | `src/gallium/drivers/zink/zink_clear.c`, `src/gallium/drivers/zink/zink_context.c`, `src/gallium/drivers/zink/zink_context.h` +2 |  | carry (debug; strip-diag-first); sec: no |
| `1a4eaba7481` | zink: classify pass-splitting barriers and count resumed passes | `src/gallium/drivers/zink/zink_context.c`, `src/gallium/drivers/zink/zink_context.h`, `src/gallium/drivers/zink/zink_synchronization.cpp` |  | carry (debug; strip-diag-first); sec: no |
| `67d304a5ef3` | zink: name what changed when a pass restarts on the same attachments | `src/gallium/drivers/zink/zink_context.c` |  | carry (debug; strip-diag-first); sec: no |
| `fa90bcc9e6f` | build: make a ThreadSanitizer build of Mesa possible on this toolchain | `build-support/tsan-blacklist.txt`, `src/util/streaming-load-memcpy.c` |  | carry, possibly upstream-after-cleanup (TSan build); sec: no |

### C. Residency, allocation and device-loss robustness

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `84a6c131ecc` | kk: refuse a NULL allocation instead of handing it to the residency set | `src/kosmickrisp/vulkan/kk_device.c` |  | upstream-after-cleanup; sec: guest-DoS |
| `3b3e7e59e81` | kk: stop allocating once the device is lost | `src/kosmickrisp/vulkan/kk_bo.c`, `src/kosmickrisp/vulkan/kk_cmd_pool.c`, `src/kosmickrisp/vulkan/kk_queue.c` |  | upstream-after-cleanup; sec: guest-DoS |
| `c9da94099c1` | kk: count a sampler slot as retired only when it is | `src/kosmickrisp/vulkan/kk_device.c` |  | upstream-after-cleanup; sec: guest-DoS |
| `f65ee40e5d4` | kk: register image textures and their views for residency | `src/kosmickrisp/vulkan/kk_image.c`, `src/kosmickrisp/vulkan/kk_image_view.c` |  | carry (tied to our residency model), possibly upstream; sec: no |
| `861f715ff02` | kk: every image-view texture we mint belongs in the residency set | `src/kosmickrisp/vulkan/kk_image_view.c`, `src/kosmickrisp/vulkan/kk_image_view.h` |  | carry (tied to our residency model), possibly upstream; sec: no |
| `030d3a5ba4b` | kk: borrow command allocators from a budget-retired device pool | `src/kosmickrisp/vulkan/kk_cmd_buffer.c`, `src/kosmickrisp/vulkan/kk_cmd_buffer.h`, `src/kosmickrisp/vulkan/kk_cmd_pool.c` +4 |  | carry (the allocator pool is our delta; policy); sec: no |

### D. Guest-input hardening (bounds and validation)

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `f11fd7301eb` | vulkan/runtime: refuse a render pass begin with a null handle | `src/vulkan/runtime/vk_render_pass.c` |  | carry (VU-violation check, same upstream stance as 0019); sec: yes (guest-DoS/OOB) |
| `51c0bcef04f` | kk: bound descriptor set binds by the sets and dynamic buffers KK holds | `src/kosmickrisp/vulkan/kk_cmd_buffer.c` |  | carry (VU-violation check, same upstream stance as 0019); sec: yes (guest-DoS/OOB) |
| `5d1ed5dcbeb` | kk: bound a push descriptor set index to the pipeline layout | `src/kosmickrisp/vulkan/kk_cmd_buffer.c` |  | carry (VU-violation check, same upstream stance as 0019); sec: yes (guest-DoS/OOB) |
| `1333dede966` | kk: bound guest query indices and multiview spans to the pool | `src/kosmickrisp/vulkan/kk_query_pool.c` |  | carry (VU-violation check, same upstream stance as 0019); sec: yes (guest-DoS/OOB) |
| `6a7ff9a7e56` | kk: drop indirect draws above maxDrawIndirectCount | `src/kosmickrisp/vulkan/kk_cmd_draw.c` |  | carry (VU-violation check, same upstream stance as 0019); sec: yes (guest-DoS/OOB) |
| `ee2598c7505` | kk: drop a draw with no live render pass and clear the deferred-start flag on end | `src/kosmickrisp/vulkan/kk_cmd_draw.c` |  | carry (VU-violation check, same upstream stance as 0019); sec: yes (guest-DoS/OOB) |
| `ac6bc758440` | kk: refuse descriptor set layouts and allocations KK cannot lay out | `src/kosmickrisp/vulkan/kk_descriptor_set.c`, `src/kosmickrisp/vulkan/kk_descriptor_set_layout.c` |  | upstream-after-cleanup (plain KK bug); sec: yes (guest-DoS/OOB) |
| `4f772d6fbe7` | kk: refuse images outside the extent, layers and levels KK advertises | `src/kosmickrisp/vulkan/kk_image.c` |  | upstream-after-cleanup (plain KK bug); sec: yes (guest-DoS/OOB) |
| `e1e3b7be0db` | kk: survive a descriptor pool whose backing memory cannot be allocated | `src/kosmickrisp/vulkan/kk_descriptor_set.c` |  | upstream-after-cleanup (plain KK bug); sec: yes (guest-DoS/OOB) |
| `f00cffbe253` | kk: do not dereference an unknown format when swizzling fragment output | `src/kosmickrisp/vulkan/kk_shader.c` |  | upstream-after-cleanup (plain KK bug); sec: yes (guest-DoS/OOB) |
| `3af83b80891` | kk: drop tessellation draws whose patch size is not one we serve | `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/vulkan/runtime/vk_graphics_state.c` |  | upstream-after-cleanup (plain KK bug); sec: yes (guest-DoS/OOB) |
| `98a2b12dc6e` | kk: bound descriptor writes, copies and templates to the set | `src/kosmickrisp/vulkan/kk_descriptor_set.c` |  | upstream-after-cleanup (pair with `edf5c2693cd`); sec: yes (guest-OOB) |
| `2aa540a7ef1` | kk: validate transform-feedback buffer offsets and sizes against the buffer | `src/kosmickrisp/vulkan/kk_cmd_draw.c` |  | carry (fold into the XFB component with 0007); sec: yes (guest-OOB) |
| `31f5ccc452c` | kk: validate guest-supplied serialized shaders and render-area bounds | `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/kosmickrisp/vulkan/kk_shader.c` |  | carry (limina-specific); sec: yes (guest-DoS/OOB) |

### E. Rendering correctness and performance (KK and zink)

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `352da85cb39` | zink: don't recurse forever populating a shadow attachment | `src/gallium/drivers/zink/zink_render_pass.c` |  | upstream-now (queued in the guest mesa series); sec: guest-DoS |
| `6aa1f4a2105` | zink: fix the data races on the cross-thread batch/fence state | `src/gallium/drivers/zink/zink_batch.c`, `src/gallium/drivers/zink/zink_bo.h`, `src/gallium/drivers/zink/zink_clear.c` +9 |  | upstream-now (rebased on MR 45147); sec: data race |
| `25530d9a082` | zink: make attachment writes shader-readable when a pass ends | `src/gallium/drivers/zink/zink_context.c` |  | upstream-after-cleanup; sec: no |
| `f782fdb9814` | kk: default residual untyped values in msl type inference | `src/kosmickrisp/compiler/msl_type_inference.c` |  | upstream-after-cleanup; sec: no |
| `f9e4403ee6b` | kk: bit-reinterpret fragment outputs on a render-target type mismatch | `src/kosmickrisp/compiler/msl_nir_lower_common.c` |  | upstream-after-cleanup; sec: no |
| `195258e043d` | kk: draw direct non-indexed triangle fans from a static index buffer | `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/kosmickrisp/vulkan/kk_device.c`, `src/kosmickrisp/vulkan/kk_device.h` |  | upstream-after-cleanup; sec: no |
| `f855ff8550e` | kk: give the static fan path the last-vertex provoking order too | `src/kosmickrisp/vulkan/kk_cmd_draw.c` |  | upstream-after-cleanup; sec: no |
| `c2befbe6e0e` | kk: keep a 3D image's 2D-array alias within its 2D mip chain | `src/kosmickrisp/vulkan/kk_image.c` |  | upstream-after-cleanup; sec: possibly OOB, check |
| `bee16b46505` | kk: make the settled limina perf policies unconditional | `src/kosmickrisp/vulkan/kk_cmd_buffer.c`, `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/kosmickrisp/vulkan/kk_image.c` +2 |  | carry (policy for our KK); sec: no |
| `1af268551af` | zink: don't track render passes on KosmicKrisp | `src/gallium/drivers/zink/zink_screen.c` |  | carry (policy for our KK); sec: no |
| `6109b7fa91a` | kk: restore stock's triangle-merge disable and forced depth write | `src/kosmickrisp/vulkan/kk_shader.c` |  | carry (policy for our KK); sec: no |
| `a15ae758986` | kk: force LOAD on the mapped colour slot when storing a pass for restart | `src/kosmickrisp/vulkan/kk_cmd_draw.c` |  | carry (policy for our KK); sec: no |

### F. Transform feedback

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `fc0780c978a` | kk: capture transform feedback from every direct draw, by primitive vertex | `src/kosmickrisp/vulkan/kk_cmd_buffer.h`, `src/kosmickrisp/vulkan/kk_cmd_draw.c`, `src/kosmickrisp/vulkan/kk_nir_lower_xfb.c` +1 |  | carry (fold into the XFB component; upstream-after-split); sec: no |
| `866ac0fd308` | kk: drop transform-feedback-only outputs once capture is lowered | `src/kosmickrisp/vulkan/kk_nir_lower_xfb.c` |  | carry (fold into the XFB component; upstream-after-split); sec: no |
| `3c5cfb4a98c` | kk: serve primitives-generated and transform-feedback query pools | `src/kosmickrisp/vulkan/kk_query_pool.c` |  | carry (fold into the XFB component; upstream-after-split); sec: check (guest-indexed query memory) |

### G. Native-context bridge for Linux guests (on the host branch)

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `566d59b261c` | wsi/wayland: let a driver withhold formats it cannot share | `src/kosmickrisp/bridge/limina/lmn_memory.c`, `src/kosmickrisp/bridge/mtl_device.h`, `src/kosmickrisp/bridge/stubs/mtl_device.c` +3 |  | upstream-now (generic wsi/wayland); sec: no |
| `9ea211ac369` | kk: a bridge onto limina's Metal-level GPU protocol, for Linux guests | `meson.options`, `src/kosmickrisp/bridge/limina/limina_gpu_ring.h`, `src/kosmickrisp/bridge/limina/limina_gpu_wire.h` +11 |  | carry (limina protocol/UAPI); sec: yes until audited (guest-supplied wire and fds) |
| `30b35680021` | kk: dma-buf export and import on the limina bridge | `src/kosmickrisp/bridge/limina/lmn_bridge.h`, `src/kosmickrisp/bridge/limina/lmn_memory.c`, `src/kosmickrisp/bridge/limina/virtgpu_limina_drm.h` +9 |  | carry (limina protocol/UAPI); sec: yes until audited (guest-supplied wire and fds) |
| `8bfd4a9ffef` | kk: binary semaphores export real sync_files on the limina bridge | `src/kosmickrisp/bridge/limina/lmn_bridge.h`, `src/kosmickrisp/bridge/limina/lmn_memory.c`, `src/kosmickrisp/bridge/limina/lmn_queue.c` +6 |  | carry (limina protocol/UAPI); sec: yes until audited (guest-supplied wire and fds) |
| `b03215faf39` | kk/limina: import sync_files into binary semaphores, export real fds | `src/kosmickrisp/vulkan/kk_physical_device.c`, `src/kosmickrisp/vulkan/kk_sync.c`, `src/kosmickrisp/vulkan/kk_sync.h` |  | carry (limina protocol/UAPI); sec: yes until audited (guest-supplied wire and fds) |
| `be01483aff5` | kk/limina: import buffers other drivers made as surface textures | `src/kosmickrisp/bridge/limina/lmn_bridge.h`, `src/kosmickrisp/bridge/limina/lmn_memory.c`, `src/kosmickrisp/bridge/limina/virtgpu_limina_drm.h` +3 |  | carry (limina protocol/UAPI); sec: yes until audited (guest-supplied wire and fds) |
| `b71707a73a2` | kk: create an instance on Linux | `src/kosmickrisp/vulkan/kk_instance.c`, `src/kosmickrisp/vulkan/kk_wsi.c` |  | carry (limina protocol/UAPI); sec: no |
| `9389bf2c34a` | kk: present through the software WSI path off macOS | `src/kosmickrisp/vulkan/kk_physical_device.c`, `src/kosmickrisp/vulkan/kk_wsi.c` |  | carry (limina protocol/UAPI); sec: no |
| `6e61e2caf48` | kk/limina: small heaps share pooled arenas | `src/kosmickrisp/bridge/limina/lmn_bridge.h`, `src/kosmickrisp/bridge/limina/lmn_device.c`, `src/kosmickrisp/bridge/limina/lmn_encoder.c` +2 |  | carry (limina protocol/UAPI); sec: no |
| `b4d6039e414` | kk/limina: command buffers hold the objects their streams name | `src/kosmickrisp/bridge/limina/lmn_bridge.h`, `src/kosmickrisp/bridge/limina/lmn_encoder.c`, `src/kosmickrisp/bridge/limina/lmn_queue.c` |  | carry (limina protocol/UAPI); sec: no |
| `b824055d8f6` | kk: name the DRM render node when the bridge is on one | `src/kosmickrisp/bridge/limina/lmn_device.c`, `src/kosmickrisp/bridge/mtl_device.h`, `src/kosmickrisp/bridge/mtl_device.m` +2 |  | carry (limina protocol/UAPI); sec: no |
| `0075bc828b7` | kk/limina: publish queue events when they are pushed | `src/kosmickrisp/bridge/limina/lmn_queue.c` |  | carry (limina protocol/UAPI); sec: no |
| `e9690a613ac` | kk/limina: wire version 3 and the SURFACE_CREATE UAPI | `src/kosmickrisp/bridge/limina/limina_gpu_wire.h`, `src/kosmickrisp/bridge/limina/virtgpu_limina_drm.h` |  | carry (limina protocol/UAPI); sec: no |
| `e2247c8eb87` | kk/limina: present windows as surfaces the compositor imports | `src/kosmickrisp/bridge/limina/lmn_bridge.h`, `src/kosmickrisp/bridge/limina/lmn_device.c`, `src/kosmickrisp/bridge/limina/lmn_memory.c` +7 |  | carry (limina protocol/UAPI); sec: no |
| `a7b124baf6c` | kk: import a 4:2:0 picture shared one surface per plane | `src/kosmickrisp/bridge/limina/lmn_memory.c`, `src/kosmickrisp/bridge/mtl_heap.h`, `src/kosmickrisp/bridge/stubs/mtl_heap.c` +6 |  | carry (limina protocol/UAPI); sec: no |
| `89ecf2e4645` | kk: advertise I420 at LINEAR for pictures shared per plane | `src/kosmickrisp/vulkan/kk_format.c` |  | carry (limina protocol/UAPI); sec: no |
| `5b2984103c6` | kk: make an imported surface's memory device-local only | `src/kosmickrisp/bridge/limina/lmn_memory.c`, `src/kosmickrisp/bridge/mtl_heap.h`, `src/kosmickrisp/bridge/stubs/mtl_heap.c` +3 |  | carry (limina protocol/UAPI); sec: no |
| `9ed3332246a` | kk: advertise no timestamps on a bridge without counter heaps | `src/kosmickrisp/bridge/limina/lmn_device.c`, `src/kosmickrisp/bridge/mtl_device.h`, `src/kosmickrisp/bridge/stubs/mtl_device.c` +2 |  | carry (limina protocol/UAPI); sec: no |
| `8de0260688c` | kk/limina: carry timestamp counter heaps over the transport | `src/kosmickrisp/bridge/limina/limina_gpu_wire.h`, `src/kosmickrisp/bridge/limina/lmn_bridge.h`, `src/kosmickrisp/bridge/limina/lmn_device.c` +3 |  | carry (limina protocol/UAPI); sec: no |
| `f51312d8161` | kk: recognise the X server's virtio-gpu as the device we render on | `src/kosmickrisp/vulkan/kk_wsi.c` |  | carry (limina protocol/UAPI); sec: no |
| `219ed8db56c` | kk: keep a legacy WSI scanout image out of array texture types | `src/kosmickrisp/vulkan/kk_image_layout.c` |  | carry (limina protocol/UAPI); sec: no |
| `2e1d2485bca` | kk/limina: sync the wire header for LMN_ERROR_RESTORE | `src/kosmickrisp/bridge/limina/limina_gpu_wire.h` |  | carry (limina protocol/UAPI); sec: no |
| `c19fc570e80` | kk: keep a guest application's stderr quiet unless asked | `src/kosmickrisp/bridge/limina/lmn_device.c`, `src/kosmickrisp/bridge/mtl_bridge.h`, `src/kosmickrisp/bridge/mtl_bridge.m` +4 |  | carry (limina protocol/UAPI); sec: no |
