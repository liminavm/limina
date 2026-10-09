# KosmicKrisp guest-input bounds probe

Under venus, a guest's Vulkan calls reach KosmicKrisp without validation, so invalid usage is
input KK must refuse or survive. `probe.c` drives KK directly through the Vulkan loader, one case
per process, and reports each as PASS, FAIL or CRASH: descriptor set layouts and allocation,
descriptor set binds, image limits, descriptor writes and copies, and push descriptors.

Every case passes from limina-kk `1bf40e3a14a` on, in builds with and without asserts; before the
fixes each misbehaved or crashed (except `mip-legal`, the control). Some cases pass on an unfixed
KK too, so a pass alone does not prove a fix; run the whole set.

    cc -Wall -I/opt/homebrew/include probe.c -L/opt/homebrew/lib -lvulkan -o probe
    VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./probe [case]

Each case re-executes the binary: a bare `fork()` cannot reach Metal's XPC services.

## `cache-shader-probe.c` — pipeline-cache and dynamic-rendering inputs

`probe.c`'s companion, same harness, covering two more guest-reachable surfaces found by the
virglrs host-validation audit:

- **Serialized shaders via the pipeline cache.** A guest's `vkCreatePipelineCache` `pInitialData`
  is forwarded unchanged. KK registers no `pipeline_cache_import_ops`, so each entry is kept as a
  raw-data cache object under the guest's key; at the next matching pipeline creation
  `vk_pipeline_cache_lookup_object` re-deserializes it through the shader ops
  (`-> kk_deserialize_shader`), a path with no header or BLAKE3 to vet the body. The probe
  round-trips a real compute pipeline to obtain a valid blob and the shader's key, poisons the kk
  shader entry, re-seeds a new cache and recreates the pipeline. `cache-bad-stage` (a stage index
  far past the `MESA_SHADER_STAGES` array) **crashes** an unfixed KK; `cache-huge-code-len` and
  `cache-no-nul` are real but non-crashing (an oversized allocation that then overruns; an
  unterminated string that reaches the MSL compiler), so they pass even unfixed — run the set and
  read them together.
- **Dynamic-rendering bounds.** `render-huge-area` / `render-huge-layers` hand
  `vkCmdBeginRendering` a `renderArea`/`layerCount` beyond KK's advertised
  `maxFramebufferWidth/Height`/`maxFramebufferLayers`; KK must refuse (the error surfaces at
  `vkEndCommandBuffer`). No host failure has been observed from accepting them (Metal bounds the
  target by the real attachment), so this is defence in depth; `render-ok` is the in-range control.

Fixed in limina-kk `kk: validate guest-supplied serialized shaders and render-area bounds`: before
it `cache-bad-stage` crashes and the two `render-huge-*` cases are accepted (FAIL); after it all
cases PASS, with and without asserts.

    cc -Wall -I/opt/homebrew/include cache-shader-probe.c -L/opt/homebrew/lib -lvulkan -o csp
    VK_ICD_FILENAMES=<kk build>/.../kosmickrisp_mesa_devenv_icd.aarch64.json ./csp [case]

## `query-probe.c` — query pools

The third companion, same harness. KK advertises `VK_EXT_primitives_generated_query` and
`VK_EXT_transform_feedback`'s queries, so a guest that enables them creates
`PRIMITIVES_GENERATED` and `TRANSFORM_FEEDBACK_STREAM` pools. Every pool entry point (create,
host and command reset, result read, copy) sizes the reports from `kk_reports_per_query`.
`pg-queries` / `xfb-queries` begin and end four queries with no draws and read them back with
availability, checking the layout: a transform-feedback query is two values (written, needed),
so its availability word is the third. `xfb-stream1` names a vertex stream KK does not have, and
`pipeline-stats` creates a pool of a type KK does not advertise; refusing or ignoring either is
fine. `oq-queries` and `ts-queries` are controls.

Fixed in limina-kk `kk: serve primitives-generated and transform-feedback query pools`. Before
it, an asserts build aborts in `kk_reports_per_query` on all four non-control cases, and a
release build passes them except `xfb-queries`, whose availability lands in the second word.
After it, all six cases PASS with and without asserts. In a guest, piglit's transform-feedback
and query tests (469, over virgl → zink → KK) gave the same results on both KK builds except
`ext_transform_feedback2@counting with pause`, which went from fail to pass (one run per build).

    cc -Wall -I/opt/homebrew/include query-probe.c -L/opt/homebrew/lib -lvulkan -o qp
    VK_ICD_FILENAMES=<kk build>/.../kosmickrisp_mesa_devenv_icd.aarch64.json ./qp [case]

## `tess-probe.c` — tessellation patch sizes

A tessellation draw divides its vertex count by the patch size: on the CPU for a direct draw,
on the GPU for an indirect one. A guest's patch size, from the pipeline or from
`vkCmdSetPatchControlPointsEXT`, reaches KK unvalidated, and the runtime stores it in a
`uint8_t`, so 256 wraps to 0 and 257 to 1. Every case draws a control (3 control points, one
patch covering the target), then the bad draw, then the control again; PASS = the device
survived and still renders. `tess/` holds the GLSL and the SPIR-V headers `tess/gen.sh` makes
from it.

arm64 returns 0 for an integer divide by zero instead of trapping, so a release build passes
every case even unfixed. The oracles are a UBSan build of KK
(`-Db_sanitize=undefined -Db_lundef=false`, in its own build dir) run with
`UBSAN_OPTIONS=halt_on_error=1:suppressions=<this dir>/ubsan.supp`, and an asserts build.
`ubsan.supp` silences `kk_instance.c`'s `&instance->vk` on the NULL instance of
`vkGetInstanceProcAddr(NULL, ...)`, which fires in every case.

Fixed in limina-kk `kk: drop tessellation draws whose patch size is not one we serve`. Before
it, UBSan halts on division by zero at `kk_cmd_draw.c:1505` for `pcp0-direct`, `pcp256-direct`
and `static-pcp0`, and an asserts build aborts in the runtime's `SET_DYN_VALUE` on
`pcpmax-direct`. After it, all twelve cases PASS on UBSan, asserts and release builds. piglit's
tessellation-named tests (and the rest of a broad 21877-test selection) gave identical results
on both KK builds.

    cc -Wall -I/opt/homebrew/include tess-probe.c -L/opt/homebrew/lib -lvulkan -o tp
    VK_ICD_FILENAMES=<kk build>/.../kosmickrisp_mesa_devenv_icd.aarch64.json ./tp [case]

## `indirect-probe.c` — indirect draw counts

KK advertises `maxDrawIndirectCount = 65535`, but a guest's `drawCount` / `maxDrawCount`
reaches the indirect entry points unchecked. Same control-bad-control shape as `tess-probe.c`,
with a 30 s limit per case; the over-limit count is 214748365, which is also the count whose
20-byte draw records overflow a 32-bit size. `plain-over` uses `vkCmdDrawIndirect`,
`count-over` `vkCmdDrawIndirectCount` (the count buffer says 1), `fan-over` a triangle fan,
which KK unrolls into a list.

Fixed in limina-kk `kk: drop indirect draws above maxDrawIndirectCount`. Before it, on a
release build, `plain-over` timed the GPU out (10.9 s, the command buffer discarded in GPU
recovery, which is host-wide) and `fan-over` did not finish in 30 s; `count-over` passed.
After it, all six cases PASS on asserts and UBSan builds. piglit's indirect, multi-draw and
conditional-rendering tests (103) gave identical results on both KK builds.

    cc -Wall -I/opt/homebrew/include indirect-probe.c -L/opt/homebrew/lib -lvulkan -o ip
    VK_ICD_FILENAMES=<kk build>/.../kosmickrisp_mesa_devenv_icd.aarch64.json ./ip [case]
