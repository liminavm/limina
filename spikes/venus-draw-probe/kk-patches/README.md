# KosmicKrisp carried patches (limina)

The mesa checkout lives on the regenerable case-sensitive volume
(`third_party/mesa-cs.sparseimage`, mounted at `/Volumes/mesa-cs`) and is gitignored —
these diffs are the durable copy. Apply with `git apply` in the mesa checkout, rebuild
with `PATH=/opt/homebrew/opt/llvm/bin:$PATH ninja -C /Volumes/mesa-cs/build-kk`.

- `kk-fixes.patch` — real fixes, upstream MR candidates:
  - `kk_device_memory.c`: kk_AllocateMemory asserted on `metal_info->handleType` with a
    NULL `metal_info` for host-pointer-only imports (VK_EXT_external_memory_host).
    (Also contains the env-gated `[LIMINA-KK-MAP]` probe — strip before upstreaming.)
  - `kk_device_memory.c`: `newBufferWithBytesNoCopy` returns nil on a rejected
    pointer/length and the nil went straight into the device residency set — the next
    submit SIGSEGV'd inside the AGX residency-set commit (`kk_queue_submit` →
    `kk_device_make_resources_resident`). Fail the allocation loudly instead
    (`[LIMINA-KK-IMPORT]` line names the bad ptr/size).
  - `bridge/mtl_buffer.m`: Metal requires buffer-backed (linear) textures to be
    MTLTextureType2D; demote single-layer 2DArray descriptors (KK promotes
    INPUT_ATTACHMENT-usage images to 2DArray).
- `kk-xfb.patch` — **VK_EXT_transform_feedback + VK_EXT_primitives_generated_query
  implementation** (the ES3/WebGL2 gate for zink): capture lowered to VS global stores
  (`kk_nir_lower_xfb.c`), per-draw xfb_base/active-mask/verts-per-instance in the root
  descriptor, CPU counter-buffer shadow ring (KK replays vkCmd* sequentially at submit,
  so CPU accounting at replay is sound), query results written via the GPU-ordered
  imm-write list (CPU stores get clobbered by earlier-recorded ResetQueryPool
  dispatches). Exact for the GLES3-legal surface: non-indexed list topologies;
  indexed/indirect/tess draws mask capture off with a one-time warning. TODOs: GPU
  counter-buffer writes, indexed/strip capture via compute prepass, partial-fit capture.
  Upstream MR candidate (carries its `[LIMINA-KK-XFB]`/draw probes — strip for upstream).
  Verified end-to-end: xfb-test.c (capture byte-correct, pause/resume appends,
  primitives_written==4) and Firefox WebGL2 aquarium on the seated KK desktop.
- `kk-perf.patch` — env-knobbed perf changes (`LIMINA_KK_SLIMPUSH` latest-layout push-
  descriptor sizing + the `LIMINA_KK_STATS` oversize check, `LIMINA_KK_BOCACHE` cmd-pool
  buffer cache). `LIMINA_KK_NOLISTRESTART`/`LIMINA_KK_EARLYZ` ride in kk-xfb.patch's
  kk_cmd_draw.c. Knob defaults/verdicts live in `boot-seated-kk.sh`.
- `kk-probes.patch` — `LIMINA_KK_RTLOG`-gated instrumentation only (render-pass/state/
  texture logging in the bridge) plus the `LIMINA_KK_CAPTURE=<width>` +
  `METAL_CAPTURE_ENABLED=1` targeted single-pass Metal GPU capture in `mtl_encoder.m`.
  Debug tooling; not for upstream. (Draw/vertex/bind probes now ride in kk-xfb.patch's
  kk_cmd_draw.c.)

Known upstream KK bugs (no patch yet, see RESULTS.md rounds 2–12):
- `kk_buffer.c` kk_bind_buffer_memory drops `info->memoryOffset` for non-heap (imported)
  memories.
- Latent: the Metal render PSO lives on the *vertex* kk_shader and is re-emitted only on
  IS_SHADER_DIRTY(VERTEX) (`kk_cmd_draw.c` kk_flush_pipeline); the fragment function
  (with the nir_lower_blend-baked colormask) is compiled into that PSO. Unexercised by
  zink today (pipeline binds always rebind all stages), but a fragment-only shader-object
  rebind would keep a stale PSO. Related: KK advertises no EDS3 colorWriteMask, so
  dynamic colormask never reaches it.

RESOLVED (round 13, see RESULTS.md): the "draws produce zero fragments in linear+DS
passes" blocker was NOT a KK bug — it was guest zink 25.3 missing
`gfx_pipeline_state.dirty` in the draw-time pipeline-update gate (upstream mesa
regression `32b4412d`, fixed by `5a5a87ec`, MR 39381; no stable backport). Only triggers
when colormask is static (no full-ds3) — exactly the KK feature level. Guest mesa ≥26.x
(our /opt/mesa-zink) is fine; stock Fedora 43 (mesa 25.3.6) hits it.
