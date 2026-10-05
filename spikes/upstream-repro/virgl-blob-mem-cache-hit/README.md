# virgl: report blob_mem for a resource that was already imported

**Bug.** `virgl_drm_winsys_resource_create_from_handle()` sets `*blob_mem` only on the path that
allocates a new `virgl_hw_res` and runs `RESOURCE_INFO`; a hit in `bo_handles`/`bo_names` returns
with it still 0. A multi-planar dma-buf carries every plane in one fd, `dri_create_image_from_winsys()`
imports the planes last to first, and `virgl_resource_from_handle()` emits `SET_TYPE` only for plane
0 — which is therefore always the cache hit. For an untyped blob (memory exported from venus,
for example) `SET_TYPE` is never sent; the host rejects the first sampler view on the resource and
the context goes into error. Plane 0 is also laid out as a classic resource, with the winsys
stride/offset/modifier discarded.

**Reproducer.** `nv12-blob-import.c`: allocates host-visible, dma-buf-exportable memory through
Vulkan (venus, so the dma-buf is an untyped `HOST3D` blob), fills it with solid NV12 red, exports
it, imports it into EGL/GLES (virgl) as NV12 with both planes on the one fd, samples it into an FBO
and reads the centre pixel back. The program interposes `ioctl()` and prints every
`VIRGL_CCMD_PIPE_RESOURCE_SET_TYPE` it submits. Mode `r8` imports only the luma plane as a
single-plane control.

    cc -o nv12-blob-import nv12-blob-import.c -I/usr/include/libdrm -lvulkan -lEGL -lGLESv2 -ldl
    ./nv12-blob-import r8      # control
    ./nv12-blob-import nv12

Needs a virtio-gpu guest with venus and virgl and `blob=true`.

## Results

| Mesa | Setup | Result |
|---|---|---|
| Fedora 44 `mesa-26.2.3` | QEMU guest, virgl + venus on Intel Iris Plus G7, virglrenderer 1.3.0 | `nv12`: no `SET_TYPE`; host `vrend_decode_create_sampler_view: … Illegal resource`; pixel (0,0,0,0) |
| `main` b39d173ca93 | same | `r8`: one `SET_TYPE` (format 64 = R8, 1 plane), pixel r=81, OK, 3/3. `nv12`: **0** `SET_TYPE`, host `Illegal resource`, pixel (0,0,0,0), 3/3 |
| `main` + fix (series tip e09e44d2d0d) | same | `r8`: unchanged, OK, 3/3. `nv12`: **1** `SET_TYPE` (format 64, 2 planes, strides 64/64, offsets 0/4096), 3/3; host then fails `vrend_renderer_pipe_resource_set_type: failed to create egl image`; pixel (0,0,0,0) |

Measured 2026-10-05.

The fix is necessary but not sufficient on stock virglrenderer. With it, `SET_TYPE` goes out,
but it carries plane 0's *lowered* format (R8, because this host does not advertise NV12 sampling)
with a plane count of 2. vrend 1.3.0 turns that into a two-plane `DRM_FORMAT_R8` EGL import, which
the host EGL rejects (`src/vrend/vrend_renderer.c`, `vrend_renderer_pipe_resource_set_type`).
Rewriting the format to `VIRGL_FORMAT_NV12` (166) in flight made the host accept the import, but
the colour came back wrong, (0,198,0,255), because the guest still samples it through R8/RG8
views. So sampling a lowered multi-planar blob correctly end to end needs follow-up work, either in
what the guest puts in `SET_TYPE` or in how vrend handles it. This patch only fixes the missing
`SET_TYPE`.

## MR description (draft)

> **virgl: report blob_mem for a resource that was already imported**
>
> `virgl_drm_winsys_resource_create_from_handle()` only reports `blob_mem` when it creates the
> `virgl_hw_res`. A multi-planar dma-buf imports every plane from one fd, planes are imported last
> to first, and `SET_TYPE` is emitted for plane 0 only, so plane 0 is always the cache hit and an
> untyped blob never gets `SET_TYPE`.
>
> Reproducer attached (venus-allocated NV12 memory imported into GLES on virgl, with an `ioctl()`
> interposer that logs `SET_TYPE`). Tested under QEMU 10.2 + virglrenderer 1.3.0 on an Intel host.
> Before: no `SET_TYPE`, and the host rejects the sampler view (`Illegal resource`), 3/3. After:
> `SET_TYPE` is sent for the two-plane resource, 3/3. The single-plane control behaves the same
> either way.
>
> Note: on vrend 1.3.0, sampling still fails after this change, because the `SET_TYPE` for a
> lowered (R8 + RG88) NV12 import carries format R8 with two planes and the host's EGL import
> rejects it. That needs separate work. This MR fixes the guest-side omission that stops
> `SET_TYPE` from being sent at all.

## As sent

The commit — message, `Fixes:`, trimmed comments — is on branch `upstream/guest-2026-10` of
`liminavm/mesa`.
