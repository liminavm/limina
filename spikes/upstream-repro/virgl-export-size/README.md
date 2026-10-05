# virgl: fill in the size of an exported dmabuf

**Bug.** `virgl_drm_winsys_resource_get_handle()` fills in the handle and stride but never
`whandle->size`. `vlVaExportSurfaceHandle()` copies that field into
`VADRMPRIMESurfaceDescriptor.objects[].size` (`src/gallium/frontends/va/surface.c`), so every object
of an exported VA surface reports size 0.

**Reproducer.** `va-export-size.c` creates a 256x256 NV12 surface and a 256x256 BGRA surface on the
VideoProc entrypoint (no decode support needed), exports each with
`vaExportSurfaceHandle(VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2)`, and prints each object's reported
size next to the dma-buf's real size (`lseek(fd, 0, SEEK_END)`). It fails unless the two match.

    cc -o va-export-size va-export-size.c -lva -lva-drm
    ./va-export-size

## Results

| Mesa | Setup | Result |
|---|---|---|
| Fedora 44 `mesa-26.2.3` | QEMU guest, virgl on Intel Iris Plus G7, virglrenderer 1.3.0 | every object `size=0 dmabuf=4096`, FAIL |
| `main` b39d173ca93 | same | every object `size=0 dmabuf=4096`, FAIL, 3/3 |
| `main` + fix (series tip e09e44d2d0d) | same | every object `size=1 dmabuf=4096`, FAIL, 3/3 |

Measured 2026-10-05.

**The patch as written does not fix it.** `res->size` is the size the guest asked for when it
created the resource. For a classic resource that uses the staging path (`use_staging`, which these
surfaces do) that is 1 byte, because the pixels live on the host
(`virgl_resource.c`, `alloc_size = res->use_staging ? 1 : ...`). The kernel rounds the BO up to a
page, so the dma-buf is 4096 bytes, and `size=1` is no more truthful than 0. Two consequences:

- The value has to come from the kernel's size, not the request. The import path already stores it
  (`res->size = info_arg.size` from `RESOURCE_INFO`). For an allocated resource,
  `align(res->size, getpagesize())` matches what `lseek` reports here. Blob resources are already
  page-aligned at creation.
- For a classic resource, the exported dma-buf is a 4 KiB placeholder whatever the surface size, so
  it cannot hold the pixels. A reviewer will ask what a consumer is supposed to do with that number.
  The MR should say plainly that the field now reports the dma-buf's size, which is what
  `objects[].size` documents ("total size of this object"). It is not the image's size.

## MR description (draft)

> **virgl: fill in the size of an exported dmabuf**
>
> `virgl_drm_winsys_resource_get_handle()` never sets `whandle->size`, so
> `vaExportSurfaceHandle()` reports `objects[].size == 0` for every virgl surface. Report the size
> of the dma-buf, rounded the way the kernel rounds the BO, so that it matches
> `lseek(fd, 0, SEEK_END)` on the exported fd.
>
> Reproducer attached (exports an NV12 and a BGRA VideoProc surface and compares the reported size
> with the fd's). Tested under QEMU 10.2 + virglrenderer 1.3.0. Before: `size=0`, dma-buf 4096.
> After: *(re-measure once the patch reports the rounded size)*.

## Proposed commit message

For the patch reworked to report the page-rounded size:

    virgl: fill in the size of an exported dmabuf

    virgl_drm_winsys_resource_get_handle() fills in the handle and stride
    but never whandle->size, so VA-API reports 0 in
    VADRMPRIMESurfaceDescriptor.objects[].size for every exported surface.

    Report the size of the buffer object. res->size is the size requested
    at creation, which is 1 for a staging-backed resource, while the kernel
    rounds the BO up to a whole page; round it the same way so the value
    matches the size of the exported dma-buf.

    Signed-off-by: Gustavo Noronha Silva <gustavo@noronha.dev.br>

No `Fixes:` tag. Size has never been set here, and VA export through virgl did not exist when the
function was written. No `Cc: mesa-stable` either: nothing is known to break on a 0 size.

## Code-comment trim

Before:

    /* VADRMPRIMESurfaceDescriptor.objects[].size is copied straight from here, and left at 0
     * it tells a consumer nothing about how much memory the fd actually names. */
    whandle->size = res->size;

After (with the rounding the measurement calls for):

    /* The kernel rounds the BO to a page; res->size is the requested size. */
    whandle->size = align(res->size, getpagesize());
