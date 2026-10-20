# vrend-spirv-gles

Does the host Mesa take SPIR-V in vrend's kind of context — EGL surfaceless, GLES 3.1, zink on
KosmicKrisp? Step 1 of `docs/design/vrend-spirv.md`.

`run.sh [prefix]` compiles `tri.vert`/`tri.frag` to OpenGL SPIR-V with glslang (`-G`), validates
them, and runs `spirvprobe` against a zink-on-KK prefix: `glShaderBinary(SPIR-V)` +
`glSpecializeShader`, link, draw into an FBO, read the pixels back. It exits 0 only when the
triangle shows the uniform block's colour and the background the clear colour.

The patched Mesa is built from a worktree of the `limina-kk` branch into its own build directory and
prefix, so the shared host build stays untouched:

    MESA_SRC=/Volumes/mesa-cs/mesa-spirv-gles MESA_BUILD=/Volumes/mesa-cs/build-zink-kk-spirv \
    MESA_PREFIX=/Volumes/mesa-cs/zink-kk-spirv-prefix spikes/virgl-zink-kk/build-mesa-zink-kk.sh
    spikes/vrend-spirv-gles/run.sh /Volumes/mesa-cs/zink-kk-spirv-prefix

Results: `RESULTS.md`.
