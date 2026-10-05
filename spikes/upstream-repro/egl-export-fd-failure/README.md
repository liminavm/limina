# egl/dri2: fail eglExportDMABUFImageMESA when the driver cannot export an fd

**Bug.** `dri2_export_dma_buf_image_mesa()` ignores the return value of the
`__DRI_IMAGE_ATTRIB_FD` query. When the driver cannot produce an fd, the call still returns
`EGL_TRUE` and leaves the caller's `fds[]` as it was, so the caller goes on to use whatever was in
the array as a file descriptor. This has been the case since the extension was added. The bug is in
the EGL layer, not in any one driver.

**Reproducer.** `egl-export-emfile.c`: surfaceless EGL + GLES2. It creates a texture and an
EGLImage from it, checks that an export works, then lowers `RLIMIT_NOFILE` to 64 and fills the fd
table with `/dev/null`, so that the driver's dma-buf export (`drmPrimeHandleToFD` /
`vkGetMemoryFdKHR`) has to fail. It then calls `eglExportDMABUFImageMESA()` with `fds[]` preset to
`0x7f7f7f7f`.

    cc -o egl-export-emfile egl-export-emfile.c -lEGL -lGLESv2
    ./egl-export-emfile

Any driver that can export dma-bufs on the surfaceless platform should show it. It was measured on
virgl and on zink.

## Results

| Mesa | Setup | Result |
|---|---|---|
| Fedora 44 `mesa-26.2.3` | QEMU guest, virgl on Intel Iris Plus G7, virglrenderer 1.3.0 | free fds: `EGL_TRUE, fds[0]=6`; fd table full: `EGL_TRUE, fds[0]=0x7f7f7f7f`, FAIL |
| `main` b39d173ca93 | same, virgl | fd table full: `EGL_TRUE, fds[0]=0x7f7f7f7f`, FAIL, 3/3 |
| `main` + fix (series tip e09e44d2d0d) | same, virgl | fd table full: `EGL_FALSE, fds[0]=-1`, OK, 3/3 |
| `main` b39d173ca93 | same, zink on venus (`MESA_LOADER_DRIVER_OVERRIDE=zink`) | `ZINK: vkGetMemoryFdKHR failed`, then `EGL_TRUE, fds[0]=0x7f7f7f7f`, FAIL |
| `main` + fix | same, zink on venus | `ZINK: vkGetMemoryFdKHR failed`, then `EGL_FALSE, fds[0]=-1`, OK |

Measured 2026-10-05. The export with free fds succeeds on every build (`EGL_TRUE`, a real fd).

## MR description (draft)

> **egl/dri2: fail eglExportDMABUFImageMESA when the driver cannot export an fd**
>
> `dri2_export_dma_buf_image_mesa()` ignores the result of the `__DRI_IMAGE_ATTRIB_FD` query, so
> when the driver cannot export an fd it returns `EGL_TRUE` and leaves the caller's `fds[]`
> untouched, and the caller uses uninitialised values as fds. GTK4, for example, builds a
> `GdkDmabufTexture` around such a value and hands back a broken texture instead of falling back.
>
> Reproducer attached: export an EGLImage after filling the fd table (`RLIMIT_NOFILE`), so the fd
> export must fail. Before: `EGL_TRUE` with the sentinel left in `fds[0]`, on both virgl and zink.
> After: `EGL_FALSE` with `fds[]` set to -1, and any fds already exported for earlier planes are
> closed.
>
> No EGL error is raised, the same as the existing early return for images that cannot be exported
> at all. Whether both paths should set `EGL_BAD_ACCESS` is open for review.

## As sent

The commit — message, `Fixes:`, trimmed comments — is on branch `upstream/guest-2026-10` of
`liminavm/mesa`. Whether this path (and the existing early return) should also raise
`EGL_BAD_ACCESS` is a question to put to review.
