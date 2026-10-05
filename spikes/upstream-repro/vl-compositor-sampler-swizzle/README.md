# vl/compositor: don't swizzle the sampler operand of the alpha fetch

**Bug.** Since `210e557f7e0` ("vl: Support blending with gfx compositor", in every 26.2.x),
`create_frag_shader_yuv()` emits `TEX TEMP[0].w, IN[0], SAMP[0].wwww, 2D_ARRAY`. The sampler
swizzle is meaningless for TEX (`tgsi_to_nir` reads it only for TG4, and the `.w` writemask already
selects alpha), but virglrenderer's vrend translates TGSI itself and rejects it: the shader fails to
compile, the context goes into error, and every VA-API post-processing or presentation through the
gfx compositor on virgl draws nothing.

**Reproducer.** Any VA-API video processing on virgl over vrend, e.g.
`../vl-compositor-matrix/va-vpp-csc.c` (or `ffmpeg … -vf scale_vaapi`). The oracle is the host
renderer's log (QEMU's stderr):

    vrend_compile_shader: … Illegal shader
    cannot access field 'wwww' of non-structure
    Dropping rendering due to missing shaders

    cc -o va-vpp-csc ../vl-compositor-matrix/va-vpp-csc.c -lva -lva-drm -lm
    ./va-vpp-csc bgra

## Results

| Mesa | Setup | Result |
|---|---|---|
| `main` b39d173ca93 | QEMU guest, virgl on Intel Iris Plus G7, virglrenderer 1.3.0 | 3 shader-compile errors in the host log per run |
| `main` + fix (tip 1eea896f4d5) | same | no shader errors |

Measured 2026-10-05. The pixels still read back black on stock vrend with the fix, for an
unrelated host-side reason: the compositor binds its colour matrix as a real buffer at constant
slot 0, which virgl encodes as UBO 0, while vrend fills `CONST[0][…]` only from inline constants
(`vrend_shader.c`), so the matrix reads as zero. That is a separate virgl/vrend issue.

## MR description (draft)

> **vl/compositor: don't swizzle the sampler operand of the alpha fetch**
>
> 210e557f7e0 fetches alpha with `TEX TEMP[0].w, IN[0], SAMP[0].wwww`. The sampler swizzle has no
> meaning for TEX — `tgsi_to_nir` ignores it outside TG4 — but virglrenderer's vrend translates
> TGSI directly and fails to compile the shader (`cannot access field 'wwww' of non-structure`),
> so on virgl every gfx-compositor VA-API operation draws nothing since 26.2.0. Passing the
> sampler unswizzled is equivalent for NIR drivers and fixes the compile.
>
> Tested under QEMU 10.2 + virglrenderer 1.3.0 (Intel host) with a VA-API VPP blit: before, three
> shader-compile errors in the host log per run; after, none.
