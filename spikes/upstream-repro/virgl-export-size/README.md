# virgl: report the size of an exported dmabuf

**Bug.** `virgl_drm_winsys_resource_get_handle()` fills in the handle and stride but never
`whandle->size`. `vlVaExportSurfaceHandle()` copies that field into
`VADRMPRIMESurfaceDescriptor.objects[].size` (`src/gallium/frontends/va/surface.c`), so every object
of a VA surface exported through virgl reports size 0.

**Fix.** Ask the kernel: `lseek(fd, 0, SEEK_END)` on the fd `drmPrimeHandleToFD` just returned.
`res->size` is not the answer — it is the size requested at creation, which is 1 for a resource
whose pixels live on the host (`alloc_size = 1` on the staging path), while the BO is a page.

**Reproducer.** `va-export-size.c` creates a 256x256 NV12 and a 256x256 BGRA surface on the
VideoProc entrypoint (no decode support needed), exports each with
`vaExportSurfaceHandle(VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2)`, and compares each object's reported
size with the dma-buf's real size (`lseek(fd, 0, SEEK_END)`).

    cc -o va-export-size va-export-size.c -lva -lva-drm
    ./va-export-size

## Results

| Mesa | Setup | Result |
|---|---|---|
| Fedora 44 `mesa-26.2.3` | QEMU guest, virgl on Intel Iris Plus G7, virglrenderer 1.3.0 | every object `size=0 dmabuf=4096`, FAIL |
| `main` b39d173ca93 | same | every object `size=0 dmabuf=4096`, FAIL, 3/3 |
| `main` + fix (tip 1eea896f4d5) | same | every object `size=4096 dmabuf=4096`, OK |

Measured 2026-10-05.

A reviewer may ask what a consumer does with 4096 for a 256x256 surface: for a classic resource the
exported dma-buf is a placeholder whose pixels live on the host. The field documents the object's
size ("total size of this object"), not the image's, and a consumer that trusts the geometry and
maps the fd is exactly the one that needs to see how little memory it was given.

## MR description (draft)

> **virgl: report the size of an exported dmabuf**
>
> virgl never fills `winsys_handle::size`, so `vaExportSurfaceHandle` reports 0 for every object.
> This sets it from the exported fd itself (`lseek(SEEK_END)`), which is exact for every resource
> kind; the requested size is not (it is 1 for host-backed resources).
>
> Tested under QEMU 10.2 + virglrenderer 1.3.0: NV12 and BGRA VideoProc surfaces report 0 before
> and 4096 — the dma-buf's size — after. Reproducer attached.

The commit message is the one on `upstream/guest-2026-10`; no `Fixes:` (the field was never set,
and nothing is known to break on 0).
