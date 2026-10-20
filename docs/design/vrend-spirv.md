# vrend shaders as SPIR-V, not GLSL text

**Status:** proposal, not implemented.

## Problem

A classic virgl context compiles every shader twice. The guest's Mesa compiles the application's
GLSL to TGSI; on the host, vrend translates that TGSI back into GLSL **text**, and the host Mesa
(zink on KosmicKrisp) runs its whole GLSL front end on it again — preprocessor, parser, GLSL IR
passes, `glsl_to_nir` — before NIR reaches zink.

Measured 2026-10-20, Flathub Loupe opening its About dialog on the synoik image, unsampled, wake
probe on:

| | `SUBMIT_3D` (vrend's program build) | fences (zink pipelines, flush) |
|---|---|---|
| cold host shader cache | 133 ms | 127 ms |
| warm cache (fresh VM) | 22 ms | 144 ms |

A sampled cold run puts the GLSL-specific front end at about 30% of the program build's samples and
NIR work at 23%, the rest being shared plumbing (hashing, allocation); apportioned, skipping the
GLSL text removes roughly 55% of a cold build. The per-VM shader cache (`--gpu-shader-cache`)
already makes repeats cheap; this is about the first time a guest shows a shader.

## Approach

vrend hands the host **SPIR-V** through `glShaderBinary(GL_SHADER_BINARY_FORMAT_SPIR_V)` and
`glSpecializeShader`, so Mesa goes `spirv_to_nir` → `gl_nir_link_spirv` and never parses text.

### 1. One translator, two emitters (virglrs)

The translator (`src/vrend/shader/`, ~9.6k lines) is a byte-for-byte port of the C's string
emitter: every decision about a shader is made while writing GLSL text, so there is no layer a
second output could share. It first gains the typed intermediate form its module doc already
names as a later change: TGSI → IR → GLSL, with the GLSL held byte-identical to today's against
the shader corpus (`harness/replay/fixtures/vrend-shaders.txt`). The SPIR-V emitter is then a
second backend off the same IR, so the semantic decisions — declarations, the sampler, UBO and
attribute mapping, the workarounds — are made once.

### 2. Bindings by number, not by name

vrend links today by name: `glGetUniformLocation`, `glGetUniformBlockIndex`,
`glGetAttribLocation`, `glBindAttribLocation`, `glTransformFeedbackVaryings`,
`glBindFragDataLocationIndexed`. GL's SPIR-V path has no names to look up; every interface is
decorated explicitly (`Location`, `Binding`, `Index` for dual-source blending,
`XfbBuffer`/`Offset`/`XfbStride` for transform feedback). The IR assigns those numbers, both
emitters use them, and the draw-side binding code reads them from the IR instead of asking GL.
The GLSL emitter keeps its names, so its output stays byte-identical.

### 3. SPIR-V in a GLES context (host Mesa, `limina-kk`)

vrend drives a GLES 3.1 context, and `GL_ARB_gl_spirv` is desktop-only in the Khronos registry.
Our host Mesa accepts SPIR-V in GLES too:

- `glShaderBinary` is already core in GLES, and its SPIR-V branch checks only the driver-level
  `ctx->Extensions.ARB_gl_spirv` (`src/mesa/main/shaderapi.c`), which zink sets.
- `glSpecializeShaderARB` is absent from the GLES dispatch only because its entry in
  `src/mesa/glapi/glapi/gen/ARB_gl_spirv.xml` carries no `es2=` attribute; the patch adds one.
- `gl_nir_link_spirv` and `_mesa_spirv_to_nir` are checked for desktop-only assumptions in an ES
  program (`IsES`, version checks) and fixed where they have any.

This defines GLES behaviour no Khronos spec does, so it is a carried patch, not an upstream one. It
is small and contained to dispatch and the SPIR-V link path.

### 4. Detection and fallback

virglrs probes at init whether the host takes a trivial SPIR-V program in its GLES context, and
uses the SPIR-V emitter only then. Everywhere else — a host Mesa without the patch, the Linux port —
the GLSL emitter stays the path, unchanged. A SPIR-V program that fails to link falls back to the
GLSL emitter for that program and logs it once per shape, so a gap in the SPIR-V backend degrades
to today's behaviour, never to a missing draw.

### 5. Caching

Mesa's disk cache keys GL SPIR-V programs as it does GLSL ones, so the per-VM shader cache keeps
working; its entries change format once, when the emitter switches.

## Verification

- **GLSL unchanged:** the corpus stays byte-identical through the IR refactor; that is the gate for
  step 1, landed on its own before any SPIR-V.
- **SPIR-V valid:** every corpus shader's SPIR-V passes `spirv-val` (targeting the environment
  `spirv_to_nir` accepts) in the harness.
- **Same pixels:** the replay harness renders its traces through both emitters and compares the
  frames.
- **The point of it:** the About-dialog measurement above, cold cache, against today's 133 ms
  program build; and the full suite.

## Order

1. Spike the Mesa patch with a hand-written SPIR-V program in a GLES context, to prove the host path
   before anything depends on it.
2. virglrs: the typed IR, GLSL byte-identical.
3. virglrs: explicit bindings in the IR and the draw path, still emitting GLSL.
4. virglrs: the SPIR-V emitter, the probe and the per-program fallback.
5. Measure; then make SPIR-V the default where the probe passes.
