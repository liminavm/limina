# KosmicKrisp geometry and tessellation probes

**`indirect-tess-count.c`** counts the vertex shader invocations of a tessellated draw, issued directly
and indirectly. KosmicKrisp runs that vertex shader as compute; each vertex must be shaded once either
way, so an indirect draw that over-dispatches its compute grid shows up as extra invocations. It needs a
GL 4.3 core context, so run it under zink-on-venus (`MESA_LOADER_DRIVER_OVERRIDE=zink`); vrend's GLES
flavour offers none and the test skips.

To build it in the guest's piglit tree: copy it to
`tests/spec/arb_tessellation_shader/kk-indirect-tess-count.c`, append
`piglit_add_executable (kk-indirect-tess-count kk-indirect-tess-count.c)` to that directory's
`CMakeLists.gl.txt`, then `cmake . && ninja kk-indirect-tess-count`. Run it as
`PIGLIT_PLATFORM=gbm MESA_LOADER_DRIVER_OVERRIDE=zink bin/kk-indirect-tess-count -auto -fbo`.

The two host probes below run on the GL stack vrend uses (EGL surfaceless → Mesa st → zink →
KosmicKrisp), with no guest. Build each against a zink-on-KK prefix `P`:
`cc -I$P/include <probe>.c -L$P/lib -lEGL`. Run with `VK_ICD_FILENAMES=<KK icd json>`,
`DYLD_LIBRARY_PATH=$P/lib:/opt/homebrew/lib`, `LIBGL_DRIVERS_PATH=$P/lib`,
`MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink EGL_PLATFORM=surfaceless`. A private prefix
carries its own `libvulkan_kosmickrisp.dylib`, which `DYLD_LIBRARY_PATH` loads ahead of the ICD
json's path, so rebuild the prefix together with KK or the probe runs the old driver.

**`xfb-draw-auto.c`** checks that `glDrawTransformFeedback` draws what a geometry shader captured.
zink turns it into `vkCmdDrawIndirectByteCountEXT`, and after a GS capture only the GPU knows the
byte count, so KK must build the draw on the GPU. Exit 0 = drawn as captured.

**`cube-array-layers.c`** renders each of the 18 layer-faces of a cube-map array through an FBO
layer and samples them back through a `samplerCubeArray`, the way piglit's
`arb_texture_cube_map_array-fbo-cubemap-array` does. It passes on the host stack in every variant
(`FIXEDFN=1`, `quads`/`tris`), which places that test's failure under zink-on-venus in the guest
tier, not in KK; it fails there with geometry shaders off too.
