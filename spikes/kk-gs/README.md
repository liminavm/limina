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
