# Results

Measured 2026-10-20, M1 Max, macOS 27.0.1, host Mesa `limina-kk` at `059715d2aa5`, KosmicKrisp ICD
from `/Volumes/mesa-cs/build-kk`, host shader cache disabled.

| Mesa | `GL_NUM_SHADER_BINARY_FORMATS` | `glShaderBinary(SPIR-V)` | `glSpecializeShader` | pixels |
|---|---|---|---|---|
| `limina-kk` as is | 0 | accepted, no error | `GL_INVALID_OPERATION` (not in the GLES dispatch) | — |
| with the patch | 1 | accepted | compiles | triangle 0 255 0 255, background 255 0 0 255: **PASS** |

The context reports `OpenGL ES 3.2 Mesa 26.3.0-devel` on `zink Vulkan 1.4(Apple M1 Max
(MESA_KOSMICKRISP))`. Shaders, specialization and link took 3.87 ms for the two-line program.

The patch (branch `limina-kk-spirv-gles` in the Mesa fork, local):

- `src/mesa/glapi/glapi/gen/ARB_gl_spirv.xml`: `SpecializeShaderARB` gains `es2="3.1"`, so the GLES
  3.1+ dispatch carries it.
- `src/mesa/state_tracker/st_extensions.c`: `NumShaderBinaryFormats` is 1 in GLES too, so
  `GL_SHADER_BINARY_FORMATS` names SPIR-V there and a renderer can detect the path with a query.

`glShaderBinary`'s SPIR-V branch needed nothing: it checks only the driver-level
`ctx->Extensions.ARB_gl_spirv`, not the API. The SPIR-V link path (`gl_nir_link_spirv`) linked an
ES program with a uniform block at an explicit binding without complaint; the audit for
desktop-only assumptions continues with real vrend shaders (transform feedback, dual-source
blending, samplers, atomic counters), which this program does not exercise.

A default-block `uvec4[2]` at an explicit `Location`, set with `glUniform4uiv` by location — the way
vrend feeds its constants (`{p}const0`) — also works in the patched GLES path: unset, it would read
zero and turn the triangle black.
