# An exported VA surface's dmabuf is one page, at every resolution

GNOME Videos (Showtime) renders a black window on the enhanced tier while Firefox plays the
same file correctly, and a minimal `gst-launch … ! vaXXXdec ! glimagesink` dies with SIGBUS in
`__memcpy_generic`, called from GStreamer's `_gl_mem_create`. That is what a mapping which ends
early looks like, so the question is whether the exported surface is as large as it claims.

It is not, and not by a little:

```
surface 1920x1080 fourcc NV12: 2 object(s), 2 layer(s)
  object 0: fd 8  declared size 0  ACTUAL dmabuf size 4096  modifier 0x0
  object 1: fd 9  declared size 0  ACTUAL dmabuf size 4096  modifier 0x0
  layer 0 plane 0: offset 0 pitch 1920  -> ends at 2073600
  layer 1 plane 0: offset 0 pitch 1920  -> ends at 2073600
```

**Three sources disagree about one buffer.** `VADRMPRIMESurfaceDescriptor.objects[].size` says
`0`, the layer/pitch geometry implies 2,073,600 bytes per plane, and the object behind the fd is
4096 bytes. A consumer that trusts the geometry — which is what GStreamer's dmabuf uploader
does when it mmaps and copies — walks off the end of the first page and takes SIGBUS.

**The 4096 is fixed.** 320x240, 1280x720, 1920x1080 and 3840x2160 all export exactly one page
per object. This is not a sizing arithmetic error; the export is not backing the surface at all.

## Not a codec bug, and not new

| pipeline | result |
| --- | --- |
| VA decode → system memory → PNG | correct frame |
| VA decode only, 900 frames, no sink | exit 0, zero decode failures |
| `vah265dec ! glimagesink` | **SIGBUS** |
| `vah264dec ! glimagesink` | **SIGBUS** |
| `vavp9dec ! glimagesink` | **SIGBUS** |
| `vah265dec ! video/x-raw ! glimagesink` (forced download) | plays fine |

All three codecs fail identically, VP9 included — which shipped before H.264 and HEVC existed,
so the fault is older than either. Forcing a copy through system memory avoids it, because
nothing then maps the dmabuf. Decode itself is exonerated twice over: bit-exact against the
software decoder, and 900 frames through GStreamer with no sink and no failures.

The `-12909` / `-17694` decode errors visible in the worker log during a Showtime session are
**downstream of this**, not a decode fault: with the sink failing, the pipeline flushes and
resubmits frames whose reference pictures are gone. A decode-only run of the same file produces
none of them. Worth remembering — they name a real VideoToolbox error and look exactly like a
decoder bug.

## What the host and guest each report

Guest kernel: `virtio_gpu_dequeue_ctrl_func *ERROR* response 0x1200 (command 0x207)` —
`VIRTIO_GPU_RESP_ERR_UNSPEC` to `VIRTIO_GPU_CMD_SUBMIT_3D`.

Host renderer, naming the context after the process:

```
vrend_decode_ctx_submit_cmd: context error reported 15 "showtime" Illegal command buffer 394753
vrend_check_no_error:        context error reported 15 "showtime" Unknown 1282
context 15 failed to dispatch CREATE_OBJECT: 22
```

`1282` is `GL_INVALID_OPERATION`, `22` is `EINVAL`.

There is a second, separate defect on the same path: glupload's direct importers are all
refused (`DirectDmabufExternal … cannot produce texture-target 2D`), which is why it falls back
to the mmap-and-copy uploader that crashes. Even with the size fixed, that fallback is a full
frame copy per frame — the zero-copy import is the thing actually worth having.

## Root cause, in the guest driver we own

Two independent defects, both in guest mesa (`limina-guest`):

**The size field is never filled.** `vlVaExportSurfaceHandle` sets
`desc->objects[].size = whandle.size` (`frontends/va/surface.c`), and virgl's
`virgl_drm_winsys_resource_get_handle` (`winsys/virgl/drm/virgl_drm_winsys.c`) sets
`whandle->stride` and never `whandle->size` — zero occurrences in the function. So the
descriptor reports 0. Cheap to fix and worth fixing, but it is not what crashes: GStreamer
trusts the geometry, not this field.

**The surfaces have no guest-visible storage to export.** Video buffer planes are allocated as
ordinary virgl resources — `PIPE_BIND_CUSTOM, PIPE_USAGE_STAGING` in
`drivers/virgl/virgl_video.c` — whose storage lives on the host. `drmPrimeHandleToFD` on such a
resource's BO yields a stub with no real guest pages, which is exactly the fixed 4096 bytes,
independent of resolution. A dmabuf export of a host-only resource cannot produce frame memory,
because there is none in the guest to point at.

That makes this architectural rather than a sizing bug. Zero-copy needs the decode targets
allocated as host-mappable blob resources (`VIRTGPU_BLOB_MEM_HOST3D`, which the winsys already
knows how to create for other paths) so the exported fd names real, mappable memory.

## Two ways forward

- **Stop advertising what we cannot back.** If the VA driver did not offer dmabuf export for
  decode surfaces, GStreamer would negotiate system memory and Showtime would work today, at
  one frame copy per frame. Small and targeted; it trades performance for correctness and
  removes a crash.
- **Allocate decode targets as mappable blobs.** The real fix, and the one that makes the
  direct importers usable at all. Larger, and it spans guest mesa, virglrenderer and possibly
  libkrun.

Note the second defect on this path is not fixed by either on its own: glupload's direct
importers are refused with `cannot produce texture-target 2D`, so even a correctly sized dmabuf
would still fall back to a copy until that is addressed.

## Running it

```
gcc -O1 -Wall -o va-probe probe.c $(pkg-config --cflags --libs libva libva-drm)
./va-probe [width] [height]
```

Needs no codec and no decoding: a plain `vaCreateSurfaces` plus `vaExportSurfaceHandle` asks
the whole question, which is why it reproduces on any guest with the VA driver present.
