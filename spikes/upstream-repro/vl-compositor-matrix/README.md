# vl/compositor: upload the matrix the frontend set, not the init default

**Bug.** The gfx compositor uploads `vl_compositor_state::csc_matrix` as fragment constants 0..2.
The compute compositor reads `::yuv2rgb` and `::rgb2yuv` instead. f5eb8ab7151 ("vl: Add
pipe_video_codec proc using vl_compositor") writes `csc_matrix` for RGB→RGB and YUV→RGB in
`compositor_proc_process_frame()`, but not for:

- **RGB→YUV** (`vl_compositor_proc.c`, the final `else`): only `rgb2yuv` is written. The
  `fs_rgb_yuv` shaders then run with the init seed in `csc_matrix`, which is the BT.709-limited
  YUV→RGB matrix, and produce wrong Y/U/V.
- **1-component sources** (the "Identity" branch): `yuv2rgb` and `rgb2yuv` are written,
  `csc_matrix` is not, and the seed is applied.

Every driver without `prefer_compute_for_multimedia` uses the gfx compositor (virgl, r600,
nouveau). On main, the patch removes `csc_matrix` altogether and has the gfx path pick
`rgb2yuv` for the `fs_rgb_yuv` layers and `yuv2rgb` for everything else. That fixes the two
remaining cases. Its RGB→RGB / YUV→RGB part now only replaces the writes f5eb8ab7151 added with
the shared fields. YUV→YUV through the gfx path uses the deinterlace shaders, which read no
matrix, so neither main nor the patch changes it.

**Reproducer.** `va-vpp-csc.c`: fills a BGRA surface with six solid colours (through
`vaDeriveImage`), runs one `VAProcPipelineParameterBuffer` pass per colour into an NV12 surface
(BT.709, limited range), reads the result with `vaGetImage` and compares Y/U/V with the BT.709
values (±2). Mode `bgra` does BGRA→BGRA as a control.

    cc -o va-vpp-csc va-vpp-csc.c -lva -lva-drm -lm
    ./va-vpp-csc nv12
    ./va-vpp-csc bgra

## Results

| Mesa | Setup | Result |
|---|---|---|
| Fedora 44 `mesa-26.2.3`, `main` b39d173ca93, `main` + fix (series tip e09e44d2d0d) | QEMU guest, virgl on Intel Iris Plus G7, virglrenderer 1.3.0 | every colour reads back (0,0,0) in both modes, on all three builds; host: `vrend_compile_shader: … Illegal shader`, `cannot access field 'wwww' of non-structure`, `Dropping rendering due to missing shaders` |
| all three | same, with `vrend-swizzle-shim.so` preloaded | the shader compile error is gone, but every colour still reads (0,0,0) in `nv12` and `bgra`. ffmpeg `scale_vaapi` YUV→BGRA and scaled BGRA→BGRA give (0,0,0,255): alpha passes through, RGB is 0 |

Measured 2026-10-05.

**Not measurable on stock virglrenderer.** Two independent host-side blockers stop the gfx
compositor from producing pixels through vrend, whatever this patch does:

1. **The shader does not compile.** Since 210e557f7e0 ("vl: Support blending with gfx
   compositor", in every 26.2.x release), `create_frag_shader_yuv()` emits
   `TEX TEMP[0].w, IN[0], SAMP[0].wwww, 2D_ARRAY`. vrend 1.3.0 cannot translate a swizzled sampler
   operand, the context goes into error, and nothing is drawn. The swizzle is redundant, because
   the `.w` writemask already selects alpha. `vrend-swizzle-shim.c` (a measurement aid, not part of
   the reproducer) blanks it out of the TGSI text in flight. This breaks every virgl+vrend VA-API
   post-processing and presentation path, so it is worth its own one-line Mesa fix (drop the
   `ureg_scalar()`).
2. **The constants never reach the shader.** The compositor binds `shader_params`, a real buffer,
   at constant slot 0. virgl encodes that as a UBO at index 0 (`virgl_set_constant_buffer`), but
   vrend only maps `CONST[x][y]` with `y != 0` to UBOs (`src/vrend/vrend_shader.c:1947`). Plain
   `CONST[0..2]` becomes `uniform uvec4 fsconst0[]`, which is filled only from inline constants
   (`:6676`), so the matrix reads as zero. That matches the (0,0,0,255) readback: the sampled alpha
   survives the `MOV`, and RGB is a dot product with a zero matrix.

So the remaining part of the patch rests on code reading against main (above), not on a
before/after measurement. The patch was originally motivated by a host renderer that does run
this path: there, BGRA→BGRA blue (0,0,255) came back (209,0,0), which is the seed matrix applied
to (0,0,1). RGB→RGB is fixed on main by f5eb8ab7151. RGB→YUV is not.

## MR description (draft)

> **vl/compositor: upload the matrix the frontend set, not the init default**
>
> The gfx compositor uploads `vl_compositor_state::csc_matrix`, while the frontend's direction
> is in `yuv2rgb`/`rgb2yuv`. f5eb8ab7151 writes `csc_matrix` for RGB→RGB and YUV→RGB, but
> RGB→YUV and the 1-component identity case still convert with the init-time BT.709 YUV→RGB
> matrix on every driver that uses the gfx compositor.
>
> Rather than add two more writes, this drops `csc_matrix` and has the gfx path choose the matrix
> from the layer: `rgb2yuv` for the RGB→YUV shaders, `yuv2rgb` for everything else. Both fields
> are seeded at init with what `csc_matrix` used to hold, so paths that set neither behave as
> before.
>
> Reproducer attached (VA VPP BGRA→NV12, compares Y/U/V with BT.709). I could not measure it on
> QEMU + virglrenderer 1.3.0: vrend fails to compile the compositor's video-buffer shader since
> 210e557f7e0 (separate MR), and it does not read a resource-backed constant buffer at slot 0, so
> every gfx-compositor result there is black.

## Proposed commit message

    vl/compositor: upload the matrix the frontend set, not the init default

    The gfx compositor uploads vl_compositor_state::csc_matrix as its
    colour-conversion constants, while the compute compositor reads
    ::yuv2rgb and ::rgb2yuv. compositor_proc_process_frame() writes
    csc_matrix for RGB->RGB and YUV->RGB, but for RGB->YUV it only sets
    rgb2yuv, and for 1-component sources it sets yuv2rgb and rgb2yuv. In
    both cases the gfx path converts with the matrix vl_compositor_init_state()
    seeded csc_matrix with, BT.709 limited-range YUV->RGB.

    Remove csc_matrix and let the gfx path pick the direction from the
    layers being drawn: the RGB->YUV shaders get rgb2yuv, everything else
    yuv2rgb. Both fields are seeded at init with the matrix csc_matrix used
    to hold, so a user that sets neither draws as before.

    Fixes: f5eb8ab7151 ("vl: Add pipe_video_codec proc using vl_compositor")
    Cc: mesa-stable
    Signed-off-by: Gustavo Noronha Silva <gustavo@noronha.dev.br>

f5eb8ab7151 is in 26.2.0 and later, so `Cc: mesa-stable` applies to 26.2. The original message
described the pre-f5eb8ab7151 state ("nothing writes csc_matrix after init", the
5bc0df5aada/a337a97429a history). That no longer matches main and is left out.

## Should the patch be reduced?

Its diff is already the reduced form. On main, the `vl_compositor_proc.c` hunk only deletes the
two `csc_matrix` writes that f5eb8ab7151 added, which have to go once the field goes. The smaller
alternative is to keep the field and add `csc_matrix` writes to the RGB→YUV and identity branches,
four lines in one file. It fixes the same two cases, but it leaves two places that must agree. The
deletion makes that drift impossible, which is why it is preferred here. It is a reviewer's call.

## Code-comment trims

`vl_compositor_init_state()`, before:

    /* Seed both directions, so a frontend that never sets them draws with what the gfx
     * compositor used to hold rather than with a zero matrix. */

After:

    /* Seed both directions for users that never set them. */

`set_csc_matrix()`, before:

    /* Both fragment shaders read the matrix from constants 0..2, so which direction belongs
     * there is decided by the layer in play: the RGB->YUV shaders convert the other way. */

After:

    /* Constants 0..2 hold rgb2yuv for the RGB->YUV shaders, yuv2rgb otherwise. */
