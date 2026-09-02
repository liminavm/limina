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

## Stock Fedora reproduces it byte for byte

Fedora's own mesa `26.0.3-4.fc44`, on a 4 KiB-page stock guest with no limina guest components,
gives the same answer at every resolution:

```
VA-API 1.23, driver: Mesa Gallium driver 26.0.3 for virgl
surface 1920x1080 fourcc NV12: 2 object(s), 2 layer(s)
  object 0: fd 6  declared size 0  ACTUAL dmabuf size 4096  modifier 0x0
  object 1: fd 8  declared size 0  ACTUAL dmabuf size 4096  modifier 0x0
  layer 0 plane 0: offset 0 pitch 1920  -> ends at 2073600
```

So this is upstream virgl behaviour, not a limina delta and not a consequence of 16 KiB pages.
That cuts two ways: the fix is a legitimate upstream patch rather than a local workaround, and
the stock tier can only receive it by way of upstreaming — nothing the host advertises gates
the export. The VA frontend gates only on `mem_type`, flags and `interlaced`, and
`drivers/virgl/virgl_video.c` consults no capability at all.

(`mesa-va-drivers` is not a separate package on F44; `virtio_gpu_drv_video.so` ships in
`mesa-dri-drivers`, so a stock guest has the VA driver without installing anything.)

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

## Why the stub is one page

`virgl_resource_create_front` picks the guest BO size three ways
(`drivers/virgl/virgl_resource.c`):

```c
res->use_staging = virgl_can_copy_transfer_from_host(vs, res, vbind);

if (res->use_staging)
   alloc_size = 1;                                   /* -> one page */
else if (templ->bind & PIPE_BIND_SHARED)
   alloc_size = virgl_resource_shared_tex_size(res); /* layout at a 256-aligned stride */
else
   alloc_size = res->metadata.total_size;
```

The 4096 is `alloc_size = 1` rounded up to a page. `virgl_can_copy_transfer_from_host` begins
`... && !(bind & VIRGL_BIND_SHARED)`, so a resource created for sharing never takes that branch:
Wayland client buffers, EGL images and mutter's scanout buffers are sized from their own layout
(`virgl_resource_shared_tex_size`'s comment: *"Size of a shared buffer is validated by WSI. WSI
retrieves BO size from resource's dmabuf with lseek()."*).

**Every other texture is a stub, and exporting one is normal.** With copy-transfer in both
directions every non-shared texture stages, and `eglExportDMABUFImageMESA` on a GL texture is an
everyday operation — GTK4's `gdk_texture_download` exports the rendered texture and re-imports
it. That is sound: the fd names the host resource, a GPU-side re-import resolves to the same
host storage, and `virgl_resource_from_handle` marks the import staged precisely because the
guest storage is smaller than the layout. The one consumer that cannot cope is one that
**mmaps** the fd and trusts the geometry, and the one path that does so is the VA-API surface
descriptor's. Nothing in `virgl_resource_get_handle` can tell the two apart, which is why a
refusal at the export is the wrong tool (below).

## What was done, and what is still owed

**Decode targets get real guest memory.** `virgl_resource_create_front` opts a resource carrying
`VIRGL_RESOURCE_FLAG_VIDEO_TARGET` out of staging when the host advertises
`VIRGL_CAP_V2_VIDEO_GUEST_PLANES`, and the host writes each decoded frame into that memory
(`writeback_plane_to_guest`, vrend_video.c). The exported fd then names storage that genuinely
holds the picture, `whandle->size` reports it, and a consumer that mmaps reads the frame. Our
virglrenderer advertises the bit whenever it offers a decoder at all, so on every supported
host no decode target is a stub.

**There is no export refusal.** One shipped (mesa `26.1.8-5` through `-9`: `virgl_resource_get_handle`
returned false for an FD export whose laid-out size exceeded the guest storage) and was removed.
Its predicate is true of every staged texture, not just decode targets, so it failed every
`eglExportDMABUFImageMESA` on the tier; GTK4 then consumed an uninitialized fd, and setting a
user avatar in GNOME Settings wrote 496 KB of noise (RMSE 0.774 against the source with our
libgallium, 0 with vanilla, everything else equal). A refusal also cannot tell an mmap consumer
from a re-importing one — it cost Firefox its hardware decoder (below) while it was in. The
EGL layer now returns `EGL_FALSE` when a driver cannot export an fd
(`dri2_export_dma_buf_image_mesa`), so a driver that legitimately refuses fails cleanly instead
of leaving the caller's fd array untouched.

**Still owed: zero-copy.** The writeback is a copy per frame into guest memory; a decode target
that is a host-mappable blob would make the exported fd name the host's own storage. Booked in
`docs/hardening-backlog.md` and `docs/design/blob-decode-targets.md`. A correctly sized dmabuf
is necessary but not sufficient for that: glupload's direct importers are refused with
`cannot produce texture-target 2D`, so a right-sized export still goes through the copy
uploader until that half is done too.

## Measured on the refusal, mesa 26.1.8-5

Guest mesa `26.1.8-5.limina` (the refusal + `whandle->size`), enhanced tier, seated GNOME:

```
va-probe 1920x1080   vaExportSurfaceHandle: invalid VASurfaceID   (was: two 4096-byte objects)
va-probe 3840x2160   vaExportSurfaceHandle: invalid VASurfaceID

vah265dec ! glupload ! glimagesink   60 buffers into the sink, 0 errors, rc 0
vah264dec ! glupload ! glimagesink   60 buffers into the sink, 0 errors, rc 0
```

The caps prove the renegotiation rather than merely the absence of a crash: the decoder now
outputs plain `video/x-raw` and glupload hands `video/x-raw(memory:GLMemory)` to the sink. The
SIGBUS path is not being survived, it is not being taken.

GNOME Videos plays the file correctly (human-verified), and the host worker log emits none of
the `Illegal command buffer` / `Unknown 1282` / `CREATE_OBJECT: 22` storm that accompanied
every previous Showtime session — those were downstream of the failing sink, as this spike
predicted.

The seated desktop came up unchanged on the swapped mesa, which is the empirical half of the
scoping argument above; 21 VideoToolbox sessions across the run reported hardware
acceleration and none reported software.

## Running it

```
gcc -O1 -Wall -o va-probe probe.c $(pkg-config --cflags --libs libva libva-drm)
./va-probe [width] [height]
```

Needs no codec and no decoding: a plain `vaCreateSurfaces` plus `vaExportSurfaceHandle` asks
the whole question, which is why it reproduces on any guest with the VA driver present.

## Why one fd cannot serve both consumers

Refusing the export fixed every consumer that mmaps the fd and broke the one that does not.
Firefox's VA-API path is `GetVAAPISurfaceDescriptor()` → `vaExportSurfaceHandle`, and it has no
system-memory renegotiation to fall back to the way GStreamer does. Measured on the r18 enhanced
tier (guest mesa `26.1.8-5.limina`, firefox 154.0-5), a local VP9 clip with VA-API forced on:

```
FFmpegVideoDecoder, init, IsHardwareAccelerated=true
Requesting pixel format VAAPI_VLD
Frame decode takes 3.48 ms ... decoded 1 frames
GetVAAPISurfaceDescriptor(): vaExportSurfaceHandle failed
ProcessFlush() / FFmpegDataDecoder: shutdown
Using preferred software codec vp9            <- and it never tries again
```

It hardware-decoded exactly **one** frame, could not export it, tore the decoder down, and
rebuilt as software for the rest of the session. The same sequence reproduced on YouTube with
`media.av1.enabled=false` to force VP9.

**Why the stub worked for Firefox and not for GStreamer.** Firefox imports the fd as an EGL image
— a GPU-side import that resolves to the host resource through the kernel — and never maps it on
the CPU. The guest BO being one page is irrelevant to it; the pixels it samples are the host's.
GStreamer's fallback uploader mmaps and copies, so the same fd is fatal there.

**Honest sizing alone does not protect the mmap consumer either.** gst-va does not read
`objects[].size` for sizing (`gst-libs/gst/va/gstvaallocator.c`,
`gst_va_dmabuf_allocator_setup_buffer_full`):

```c
/* prime descriptor reports the total size of the object, including regions
 * which aren't part surface's space. Let's just grab the surface's size: */
gsize size = _get_fd_size (fd);                 /* lseek(fd, 0, SEEK_END) */
GstMemory *mem = gst_dmabuf_allocator_alloc_with_flags (allocator, fd, size, ...);

if (desc.objects[i].size < size)
   GST_WARNING_OBJECT (self, "driver bug: fd size ... is bigger than object descriptor size");
```

It sizes the `GstMemory` by `lseek` and consults `objects[].size` only to log, so the
`GstMemory` was already correctly 4096 bytes; the overrun is glupload's `_gl_mem_create`
copying by video geometry while ignoring the size of the memory it was handed. The only answer
that serves both consumers is storage that actually holds the frame, which is what real guest
memory for decode targets provides.

**AV1 is offered by the host chip, not the guest tier.** `virgl_video_vt.c` advertises
`PIPE_VIDEO_PROFILE_AV1_MAIN` only when `vt_can_decode(kCMVideoCodecType_AV1)` is true, which
needs an M3 or later; a host without it advertises nothing for AV1 on purpose, so the guest
keeps its own better-tested dav1d rather than being routed onto a host software path for no
gain. The dogfood Mac is an M4 Pro and its guest does advertise `VAProfileAV1Profile0 :
VAEntrypointVLD`; on a pre-M3 host the fallback to dav1d in the RDD process remains correct
behaviour.
