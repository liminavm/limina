# Large TES→GS interfaces on KosmicKrisp: lost private-array stores under indirect thread dispatch

## Conclusion

Under Metal 4's `dispatchThreadsWithIndirectBuffer` (KosmicKrisp's `KK_GRID_INDIRECT_THREADS`), a
kernel whose private array spills to stack memory loses stores on the threads past the first two of
each threadgroup: they read back the array's initial zeros. Dispatching the same kernel directly, or
with `dispatchThreadgroupsWithIndirectBuffer`, is correct. KosmicKrisp dispatched poly's
indirect-draw kernels that way — the software vertex, control and tessellator kernels of every
indirect tessellated draw, and the vertex/evaluation, count and main kernels of a geometry shader
draw — so any of them that spilled an array produced garbage for most invocations.

piglit's `tes-gs-max-in-out-components` is how it surfaced: from 25 ivec4 outputs on, the evaluation
shader's array spills, and the second patch reaches the geometry shader with zeros from element 20.

Fixed in the `limina-kk` Mesa fork by going back to threadgroup-count indirect dispatch: the setup
kernels write, after each grid's thread counts, the threadgroups that cover them, and every kernel
whose thread count is not a multiple of its threadgroup bounds-checks its own invocations
(`kk: dispatch indirect tessellation grids in threadgroups`; the geometry-shader half is part of
`kk: geometry shaders on poly's compute emulation`).
A guard must load its limit directly from the parameter buffer: a libpoly helper call
(`poly_input_vertices()`) is not linked into runtime shaders and crashes the compile.

## Evidence

- A CPU dump of the heap after the draw: the evaluation shader's output for threads 3-5 holds
  elements 0-19 and zeros after; threads 0-2 are whole.
- The same evaluation kernel dispatched directly: N=24/25/31 green. Indirect with 1-2 threads per
  threadgroup green; 3, 4, 32 and 64 red. `requiredThreadsPerThreadgroup` does not help, nor does
  re-declaring the scratch array as words.
- `cs.c`, a plain compute shader with the same spilled array: correct under direct and
  threadgroup-indirect dispatch.
- Measured 2026-10-08, M1 Max, zink-on-KK, before → after the fix:
  - `tessind 25 300` (indirect): 241/300 vertices wrong → 0. All of N=1/8/25/31 ×
    3/63/300/1023 vertices × direct/indirect are correct, and the vertex shader runs exactly once per
    vertex (the guard holds the over-dispatch that threadgroup dispatch reintroduces).
  - `tesgs` with geometry shaders on: N=25 and 31 red → green; `GOT=1 ONLY=20` patch 2 reads
    `0 0 0` → `180 181 182`.

## Vehicles

Each builds against a zink-on-KK prefix and runs on the host with no guest; point
`VK_DRIVER_FILES` at the KosmicKrisp ICD under test, with
`MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink EGL_PLATFORM=surfaceless`.

- `run.sh <kk-build> <N...>` builds and runs `tesgs.c`: a two-patch tessellated draw whose
  evaluation shader passes N ivec4 to a geometry shader, which checks them. Variants by
  environment: `ONLY=<k>`, `VERT`, `KEEP` (constant-index checks), `LOOP=v|i` with `LIM`,
  `GOT=1 ONLY=<k>` (prints the dynamically read element as the colour; `GOTW` for `.w`), `UNROLL`
  (loop-free evaluation shader), `NOSC`, `DRAW2`. Avoid `DIAG`: it hung the GPU host-wide once.
- `tessind <N> <vertices> [direct]`: a tessellated draw whose vertex shader spills an ivec4[N],
  copies it out and counts its invocations.
- `cs <N> <local> <invocations>` (`INDIRECT=1` for `glDispatchComputeIndirect`): the compute-only
  control.

Build `tessind`/`cs` with
`cc -O1 -o tessind tessind.c -I<prefix>/include -L<prefix>/lib -lEGL -Wl,-rpath,<prefix>/lib -Wl,-rpath,$(brew --prefix vulkan-loader)/lib`.
