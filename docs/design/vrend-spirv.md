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

### 1. TGSI stays the IR; a `Layout` pass gives every interface a number (virglrs)

virglrs already parses TGSI into typed instructions, operands and declarations with per-opcode
operand types (`src/vrend/tgsi/`), so a second IR between it and GLSL would add nothing the
SPIR-V emitter needs. What is entangled in the translator (`src/vrend/shader/`, ~9.6k lines, a
byte-for-byte port of the C's string emitter) is its analysis state, not its input: a ~100-field
context, requirement bits set while text is written, a header emitted after the body from state
the body mutated. Rewriting that behind a new IR while holding 9.6k lines byte-identical is
avoided.

The extraction is narrower: a `Layout` pass over the declarations and the `Key` that assigns every
interface its location or binding — inputs and outputs, the constant array, UBOs, samplers, images,
SSBOs, atomic counters, transform feedback, fragment outputs including dual-source. It numbers them
the way the renderer derives them from the masks today, and picks one scheme for separable
programs, whose generics the C numbers two ways (`31 - sid` in the generic declarations,
`location = i` in `emit_match_interfaces`). The GLSL emitter reads `Layout` where it now computes
inline and stays byte-identical; the SPIR-V emitter decorates with it.

### 2. Bindings by number on the SPIR-V path

vrend links by name today — about a dozen lookup sites in `src/vrend/context/draw.rs`
(`glGetUniformLocation`, `glGetUniformBlockIndex`, `glBindAttribLocation`,
`glTransformFeedbackVaryings`, `glBindFragDataLocationIndexed`, …), with UBO and sampler numbers
assigned at link time in mask order across stages. The GLSL path keeps those lookups. A SPIR-V
program is bound by `Layout`'s numbers instead, which are the same numbers, so the draw path differs
only in where it reads them.

### 3. The SPIR-V emitter's scope

Opcode lowering from typed TGSI is mechanical. The cost is the behaviour vrend layers on top from
the `Key` and the stage, mostly in the prologue (`src/vrend/shader/glsl/inst.rs`, first
instruction) and the epilogue emitted at `END`/`RET` (`src/vrend/shader/glsl/exit.rs`): colour
selection, clip-distance loads and moves, the Y flip, the fog fix-up, logic-op emulation, sRGB
encoding, all-colour-buffer writes, stream-output moves, synthesised missing inputs and dummy
outputs, the 2D-array sampler lowering, GLES texture-LOD emulation. The emitter grows through that
list; whatever it does not cover yet falls back to GLSL per program (§5).

### 4. SPIR-V in a GLES context (host Mesa, `limina-kk`)

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

### 5. Detection and fallback

virglrs probes at init whether the host takes a trivial SPIR-V program in its GLES context, and
uses the SPIR-V emitter only then. Everywhere else — a host Mesa without the patch, the Linux port —
the GLSL emitter stays the path, unchanged. A SPIR-V program that fails to link falls back to the
GLSL emitter for that program and logs it once per shape, so a gap in the SPIR-V backend degrades
to today's behaviour, never to a missing draw.

### 6. Caching

Mesa's disk cache keys GL SPIR-V programs as it does GLSL ones, so the per-VM shader cache keeps
working; its entries change format once, when the emitter switches.

## Verification

- **A corpus worth the name first.** The recorded corpus
  (`harness/replay/fixtures/vrend-shaders*.txt`) holds 33 VS/FS blocks over ~22 opcodes with plain
  constants and no recorded key: no UBOs, images, SSBOs, atomics, transform feedback, GS/TCS/TES/CS
  or indirect addressing. It is widened from the C leg's own output before `Layout` lands, so the
  byte-identical gate covers what `Layout` touches.
- **GLSL unchanged:** the widened corpus stays byte-identical through `Layout`.
- **SPIR-V valid:** every corpus shader's SPIR-V passes `spirv-val` (targeting the environment
  `spirv_to_nir` accepts) in the harness.
- **Same pixels:** the replay harness renders its traces through both emitters and compares the
  frames.
- **The point of it:** the About-dialog measurement above, cold cache, against today's 133 ms
  program build; and the full suite.

## Order

1. Spike the Mesa patch with a hand-written SPIR-V program in a GLES context (done:
   `spikes/vrend-spirv-gles/`; UBOs at explicit bindings and a default-block uniform set by location
   both work).
2. virglrs: widen the corpus from the C leg (GS/TCS/TES, UBOs, images, SSBOs, atomics, transform
   feedback, key-dependent output).
3. virglrs: the `Layout` pass, GLSL byte-identical against the widened corpus.
4. virglrs: the SPIR-V emitter over TGSI, `Layout` and `Key`, the probe and the per-program
   fallback, growing through the scope list.
5. Measure; then make SPIR-V the default where the probe passes.
