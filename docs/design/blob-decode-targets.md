# Blob-backed decode targets

VA decode surfaces have no guest-visible storage. Their planes are allocated as ordinary virgl
resources whose pixels live on the host, so `drmPrimeHandleToFD` yields a one-page stub at every
resolution and an exported dmabuf names no frame memory at all. Measured, with a reproducer, in
`spikes/va-dmabuf-size`.

The stopgap in place today refuses that export. It stops the SIGBUS in consumers that mmap the
fd, and it takes Firefox's hardware decoder away outright — Firefox imports the fd on the GPU,
never maps it, and has no fallback but software. Neither half of that trade is worth keeping.
This is the fix that removes the trade: give the decode target storage the guest can see.

## What the stack already does, and why this is a small change

A host-visible blob is not new machinery. venus has used one for every frame for months, and
the shape it uses is exactly the shape a decode target needs:

- A context's `get_blob` returns a `virgl_context_blob` carrying `map_ptr` (a **host virtual
  address**), `map_info`, and on macOS an `iosurface_id`
  (`virglrenderer.c:1274-1305`).
- libkrun's `resource_map_blob` `hv_vm_map`s that host VA into the guest, so the guest's BO is
  a window onto host memory rather than a stub.
- The pointer is deliberately *borrowed from the driver's own mapping* so that "the VMM
  `hv_vm_map`s the exact memory the GPU binds — **one mapping, guest+GPU coherent**"
  (`virglrenderer.c:1301`).
- `iosurface_id` is what lets `SET_SCANOUT_BLOB` present a guest image zero-copy, by importing
  the IOSurface as an `MTLTexture` instead of copying it (`docs/graphics.md` §4).

So the platform question — can guest-mapped host memory hold pixels the GPU also touches — is
already answered affirmatively, in production, on the venus path. **IOSurface is the macOS
dmabuf**, and the blob is how it reaches the guest.

Two older claims that contradict this are dead and have been deleted rather than corrected,
because they described the pre-KosmicKrisp stack and were cited as live constraints twice on
2026-09-01: that the guest CPU sits outside the GPU's coherency domain, and that present is a
CPU readback. The coherency bug behind the first was **fixed 2026-07-03** (libkrun 0043 +
virglrenderer 0023 + the guest-kernel patch); venus's host-visible feedback buffers — written
by the GPU via `vn_CmdCopyBuffer`/`vn_CmdFillBuffer` and polled by the guest CPU — have been
enabled in every shipped enhanced guest since 2026-07-25.

## Shape

One blob per decode target, replacing today's ordinary per-plane resources. **What backs that
blob is a phase choice, and the protocol is the same either way** — which is what lets the
cheap version ship first and the zero-copy version replace the storage underneath it.

```
VideoToolbox  ->  its own CVPixelBuffer pool
                        |
                        |  one host copy into the target's storage  (see below)
                        v
              the decode target's storage
             |                    |                         |
        guest BO            GPU samples it              scanout
   (mmap, dmabuf export)                            (phase 2 only)
```

**Phase 1 backs it with guest memory** (`BLOB_MEM_GUEST`). The host writes each decoded frame
into the blob's pages through the iovecs it already holds, and host-side sampling reuses the
path vrend already has for exactly this: re-read the guest's bytes into a GL texture before
every batch that samples it (`vrend_renderer.c:15211`, `vrend_resource::guest_pixels`). That
path exists *because* of video — it is how a software-decoded frame reaches the GPU at all
today — so phase 1 adds no new sampling machinery, only a correctly-sized and honestly-described
allocation.

**Phase 2 backs it with an IOSurface instead**, which is what buys the zero copies: the GPU
binds the surface as texture storage, and scanout takes it by `iosurface_id`.

The guest side is `virgl_video_create_buffer` (`virgl_video.c:1242`), which today defers to
`vl_video_buffer_create` and so gets ordinary per-plane resources. It allocates from a blob
instead, and must still furnish `get_sampler_view_planes`, which the same function consumes at
line 1262.

NV12 becomes **one object with two layers** at distinct offsets — what real drivers report and
what `VADRMPRIMESurfaceDescriptor` is shaped for. Today's two-objects-of-4096 is an artefact of
per-plane resources, not a format requirement. This is also what VideoToolbox already hands back:
its output is IOSurface-backed even when the IOSurface properties are omitted entirely, and NV12
arrives as one surface with two planes at distinct offsets.

## The copy, measured

**One copy per frame, and it is cheap enough not to shape anything else.** Settled by
`spikes/vt-blob-decode-target/` (2026-09-01): ~0.10 ms at 1080p and ~0.42 ms at 4K, against
16.7 ms of frame budget at 60 fps.

VideoToolbox will not decode into a buffer we supply — no decode entry point accepts one, and
the `frameOptions` dictionary added in macOS 15 admits only the two `ContentAnalyzer` keys. The
alternative of mapping VideoToolbox's own pool into the guest is dead too: the pool is not a
bounded set. It appears to recycle five surfaces only because the consumer releases each buffer
immediately; hold them and 107 frames mint 107 distinct surfaces. A decode target is exactly the
held case, since the guest keeps reference frames alive for its DPB. Ordering rules it out
independently — VA-API names the render target in `vaBeginPicture` *before* the frame decodes,
while VideoToolbox reveals its choice afterwards.

That copy is **not new cost**: today the host already CPU-maps every decoded plane and uploads it
with `glTexSubImage2D` (`upload_mapped_plane`, `vrend_video.c:156`), and the guest pays a
host→guest transfer on top to get pixels into its own memory. One copy into memory the guest
already maps replaces both. The copy engine is a free choice — CPU `memcpy` or a Metal blit —
because coherency constrains neither.

**Do not contort the layout to make pitches agree.** The row-by-row copy and the single
whole-plane `memcpy` are indistinguishable at these sizes; which one wins reordered between runs.
Only bandwidth matters, so the guest picks whatever layout suits it.

## Who dictates layout

**The guest computes the layout; the host allocates to match, or refuses.** The guest must
report offsets, pitches and sizes in the export descriptor and cannot report what it did not
choose. In phase 1 that is trivially satisfied — the storage *is* guest memory, so there is no
second allocator to disagree with. The rest of this section is the phase 2 contract, and it
holds: IOSurface allocates *exactly* the requested per-plane `bytesPerRow` with no
rounding in any case tested, odd widths and heights included, and the
`IOSurfaceGetPropertyAlignment` values are advisory once explicit `kIOSurfacePlaneInfo` is
supplied (128-byte rows reported, an 854-byte pitch accepted). The refusal path stays anyway, so
a future case IOSurface will not honour fails the buffer creation and the guest falls back,
rather than two ends proceeding with disagreeing pictures of one allocation.

`destinationImageBufferAttributes` then holds VideoToolbox to the same layout — every row and
plane alignment requested was applied exactly, on the hardware decoder, at no measurable cost.
But **alignment is paid per row**: a 16384-byte row alignment took a 352x240 surface from 136 KiB
to 5.8 MiB, and would take a 4K surface from ~12 MB to ~53 MB. Ask for what the layout needs,
never for the host page size by reflex.

One thing the arithmetic must get right on both ends: chroma needs `ceil(w/2)*2` bytes per row,
so at an odd width it is *wider* than a luma pitch of exactly `w`. Size each plane in its own
right; inheriting luma's pitch makes every odd width fail, and it fails looking like an IOSurface
restriction.

Note this interacts with a trap already recorded in `docs/graphics.md` §4.5: **decode into the
layout the guest allocated**, rather than converting. ffmpeg's VA-API path allocates I420 while
asking for NV12 elsewhere, and VideoToolbox will produce either.

## The composite shape, and why the parent is not planar

A decode target reaches the host as **one resource named by its planar format, with a chain of
plane resources over the same allocation** — the shape radeonsi builds for NV12. The per-plane
form cannot work: only a composite create names a planar format, and that is what a host-side
planar allocation keys off, so a host seeing separate R8 and R8G8 creates has nothing to
allocate one surface from.

**Every element of the chain carries its own component format, the parent included.** The wire
format and the pipe format part ways deliberately: the create sends the planar format, while the
parent's `pipe_resource` reports plane 0's component format. That divergence is the discriminator
the host samples by — a plane index only reaches the host for planes 1 and 2, so plane 0 is
recognised by its view naming `R8` where the resource is `NV12`. A parent left planar makes a
luma view indistinguishable from a composite consumer asking for the converted RGBA, and the
luma sampler silently reads RGBA. All elements must share one `hw_res`, which the SET_TYPE plane
walk requires of anything that later re-enters through an import.

**Nothing on the wire carries the plane layout**, and the channel that looks like it should is
not one: SET_TYPE transmits `plane_strides`/`plane_offsets` only for untyped blobs arriving
through `resource_create_from_handle`, and a composite target is created directly.
`VIRGL_CAP_V2_RESOURCE_LAYOUT` is unrelated — it gates a query about a target handle. So both
ends compute the same canonical layout instead: tight, in plane order, each plane's stride being
its own width times its own block size. A divergence cannot corrupt silently — too large trips
the writeback's extent check, too small is visible in the picture.

## Capability negotiation, and the order this ships in

A guest that allocates blob decode targets against a host that cannot back them must fall back,
not fail. The host advertises a capset bit; `virgl_video_create_buffer` checks it and otherwise
calls `vl_video_buffer_create` exactly as today. That makes the two sides independently
shippable and fixes the order:

1. **virglrenderer first** — a host that can back blob decode targets, advertising the bit.
   Nothing asks for it yet, so no behaviour changes.
2. **Guest mesa second** — the enhanced tier lights up, via the usual chain: fork commit →
   `scripts/export-mesa-guest-patches.sh` → mesa RPM → `scripts/provision/deliver-payload.sh` →
   `docs/images.md`.

Two capset bits, not one, because a host can do the guest-memory writeback without accepting the
composite shape — which is exactly what shipped first. `VIDEO_GUEST_PLANES` buys real guest
storage per plane and an honest export; `VIDEO_PLANAR_TARGET` buys the composite create.

Never the reverse; a guest-enabling change ahead of its host fix is the mistake
`limina-enh-delivery` records. It also keeps the capability granular, per `docs/graphics.md`
§3.4 — a partially upgraded guest gets the old path for video and keeps everything else.

## The export refusal is not removed — it lifts itself

The guard refuses an FD export whose laid-out size exceeds the guest storage behind it. A
blob-backed target has storage at least as large as its layout, so the export simply passes.
Nothing needs deleting, and the guard goes on protecting every resource that still arrives
unbacked — including the stock tier, which keeps today's behaviour until the refusal is
upstreamed. The phase that fixes video must not be the phase that reopens the SIGBUS for
everything else.

## Phases

**Phase 1 — correct, guest-visible frames, on guest-memory storage.** Allocation, layout
contract, capset bit, and the frame landing in the target's blob. Confined to virglrenderer and
guest mesa. At the end of it the export is honest, GStreamer's mmap path reads correct pixels
instead of crashing, and **Firefox has its hardware decoder back** — it needs only that the
export succeed and the EGL import resolve, both of which it had before the refusal existed. The
per-frame cost is what it is today: one host copy in, one re-read out.

**Phase 2 — the remaining copies.** Move the storage to an IOSurface bound as the GL texture's
storage, so sampling needs no re-read and scanout can take the surface by id. Then fix
glupload's direct importers, which refuse everything today with `cannot produce texture-target
2D` and fall back to the copy uploader however well-formed the buffer is.

**Phase 2's cost is a plane index that no layer carries.** Every mechanism it needs already
ships. vrend adopts an IOSurface as a GL texture's storage in
`vrend_resource_iosurface_init` (`vrend_renderer.c:9428`) for SCANOUT and SHARED resources;
the VMM's `resource_map_blob` is context-agnostic, gated only on `map_ptr` succeeding, so a
mappable vrend blob needs no VMM work; VideoToolbox already decodes into IOSurface-backed
`CVPixelBuffer`s (`virgl_video_vt.c:857`). What is missing is that the existing import is
whole-surface and 8888-only. An NV12 target needs a plane index and R8/RG88 formats through
`virgl_egl_image_from_iosurface` (`vrend_winsys_egl.c:852`), the attribute-less
`EGL_IOSURFACE_LIMINA` target (`egl_dri2.c:2660`), `dri2_from_iosurface_limina` (`dri2.c:925`,
whose per-plane geometry comes from `IOSurfaceGet*OfPlane`), zink's `resource_from_handle`, and
KK's `mtl_new_texture_with_descriptor_iosurface` (`mtl_device.m:379`), which passes a hardcoded
`plane:0` to Metal.

Of those, only the zink→KK step has no carrier: `winsys_handle::plane` already exists and the
dmabuf path uses it, but zink conveys the surface to KK as a bare
`VkImportMemoryMetalHandleInfoEXT::handle`, which has nowhere to put an index. Both halves are
ours, so the index travels in a limina-private struct chained onto that import — an explicit
index rather than letting KK infer the plane by matching the dedicated image's dimensions
against the surface's, which happens to be unambiguous for 4:2:0 and would silently stop being
so for any other subsampling.

Nothing works until the whole chain lands, which is precisely why it is not phase 1.

**The guest half comes first, because the host half is inert without it.** Measured
2026-09-01, VP9 through `vavp9dec ! glupload` on the mesa -6 guest: the host sees *two*
resources per decode target, `PIPE_FORMAT_R8_UNORM` at luma size and
`PIPE_FORMAT_R8G8_UNORM` at chroma size — 107 of each across the clip, and not one
planar-format resource. That is the lowered path, one `resource_create` per plane, and on it
each plane is already its own resource with its own texture, so no host-side plane machinery
is reachable: `vrend_resource_iosurface_init` is never called with a planar format at all.
Routing the guest through `vl_video_buffer_create_as_resource` is therefore the prerequisite
for the host work, not a follow-on to it.

**Sampling the second plane needs no guest change at all — both ends of that path already
exist.** The host keeps a separate EGLImage per plane in `aux_plane_egl_image`
(`vrend_renderer.h:106`), filled today only from a GBM bo (`vrend_renderer.c:9798`) and so
always NULL on macOS. The guest already names the plane: `metadata.plane`
(`virgl_resource.c:636`, set from the winsys handle on import at `:875`) is written as the
sampler view's whole layer dword (`virgl_encode.c:1180`), arriving as `first_layer = N,
last_layer = 0`. So a guest that imports a decode target's planes as separate component-format
resources — which is what the dmabuf importers do — is already asking for plane N by index, and
the host is already looking for an image to answer with. Phase 2's host work is to put one
there: a plane-carrying import into `aux_plane_egl_image[1]`.

This does not explain the near-blank chroma, and nothing yet does. That symptom would need
plane-indexed views of one shared resource, and the decode path does not produce them on
either upload branch — measured 2026-09-01, the raw uploader and `Dmabuf Passthrough` both
put the same 107 `R8` + 107 `R8G8` per-plane resources on the wire and no indexed view at
all. Whatever the near-blank is, it is not the cleared index.

**Filling it re-arms a context poison, so the branch order must change with it.** A surviving
index sets `needs_view`, and the `glTextureView` branch (`vrend_renderer.c:2874`) then computes
`num_layers = 0` and returns EINVAL — which puts the whole context in error for its lifetime.
The aux bind (`:2977`) is an `else if` below it, reached in the GBM world only because that
path strips `VREND_STORAGE_GL_IMMUTABLE` when `EXT_EGL_image_storage` is absent; on zink-on-KK
it is present, so the bit survives and the view branch wins. The aux image must therefore be
consulted *before* the texture-view branch, not only after it.

The guest half of the *allocation* is smaller than the host half, and needs no new protocol
concept. The one-object
shape is `vl_video_buffer_create_as_resource` (`vl_video_buffer.c:517`): it calls
`resource_create` once with the planar format, takes planes 1 and 2 from the chained
`resources[0]->next`, and sets `contiguous_planes`. Gallium's VA frontend already exports that
as one object with two layers (`va/surface.c:1453`), gated on `screen->resource_get_param`,
which virgl installs. So the guest work is to route `virgl_video_create_buffer`
(`virgl_video.c:1243`) through that constructor instead of `vl_video_buffer_create`, give
virgl's `resource_create` the plane chaining a planar format implies, and answer
`PIPE_RESOURCE_PARAM_STRIDE` and `_OFFSET` in `virgl_resource_get_param`, which today handles
only `MODIFIER` (`virgl_resource.c:942`).

That also settles how the host tells a decode target apart from anything else, without a new
flag on the wire: it is a single `PIPE_RESOURCE_CREATE` carrying a planar format, arriving
through the ordinary `vrend_renderer_pipe_resource_create` blob path. `vrend_resource_iosurface_init`
already discriminates on format there — it returns early for anything that is not BGRA/RGBA —
so a planar format is simply a case it does not handle yet.

Phase 1 is the correctness win and it stands alone. Do not gate the Firefox recovery on either
the plane work or the importer work.

**Phase 1 costs guest RAM, by design.** Decode targets stop being one-page stubs and become
their real size, and a decoder holds a whole DPB of them — a 16-deep 4K NV12 DPB is ~190 MB
that used to be 16 pages. That is the price of an fd that names the picture it claims to, but
it is large enough to look like a leak to someone bisecting guest footprint later, so it is
written down here. Encode source buffers ride the same flag and grow the same way; harmless,
and equally not a leak.

## Spikes

1. ✅ **Can VideoToolbox decode into surfaces we supply?** No — but it honours a layout we
   dictate, and the copy that follows costs ~0.10 ms at 1080p. `spikes/vt-blob-decode-target/`.
2. ✅ **Layout agreement.** IOSurface allocates the guest's pitches exactly, odd dimensions
   included. Same spike.
3. ✅ **Can an IOSurface's base address be mapped into a guest at all?** Yes —
   `spikes/hv-iosurface-map/`. `hv_vm_map` accepts IOKit-owned pages, the guest reads and
   writes them coherently across the whole allocation, and the mapping survives the host
   cycling `IOSurfaceLock` underneath it (which the guest can never take part in). Two
   constraints fall out: only whole granules map, so size the surface to a granule multiple
   rather than leaving a tail unmapped; and the GPU arm is untested — a Metal texture bound to
   the same surface writing while the guest reads is what phase 2 needs, and is worth its own
   check rather than an inference from #28.

## Verifying

**Measured 2026-09-01** (VP9, `spikes/vt-vp9-decode/vp90-2-09-aq2.webm`, 352x240, 107 frames,
ffmpeg VA-API on a stock-shaped guest): the composite shape reaches the host and lands on one
planar surface. 12 decode targets created, each
`PIPE_FORMAT_Y8_U8V8_420_UNORM` backed by a single two-plane EGL-bound IOSurface; 214 plane
writebacks (107 x 2), plane 1 at offset 84480 = 352 x 240, all `-> write`; no create refused, no
plane view refused, no frame skipped. Against the per-plane form this replaces
107 x `R8_UNORM` + 107 x `R8G8_UNORM` unrelated textures and no surface at all.

The success line is `virgl_info`, which libkrun maps to a Rust `info!` on `krun_rutabaga_gfx` —
so it needs that target in `RUST_LOG`, not just `VIRGL_LOG_LEVEL=info`. Both refusal paths are
`warn`/`error` and survive a `warn` filter, so a run showing neither success nor refusal is a
muted log, not a silent failure.


Two consumers, because they fail differently and neither failure is a crash:

- **GStreamer (mmap path).** Frames checksummed against the software decoder, not merely
  "no SIGBUS". A stale or torn frame plays and looks like video; byte-equality is the only
  thing that catches it. VP9, H.264 and HEVC are all normatively exact, so this oracle is
  available for each.
- **Firefox.** `spikes/vt-vp9-decode/guest-ff-vaapi-check.sh` and its three verdict lines, with
  hardware **retained across a full session**. The current regression is precisely a decoder
  that reports `IsHardwareAccelerated=true` and then falls back, so a single-frame check would
  pass against the very bug it is meant to catch.

Plus the `l2_video_vaapi` extension and one suspend/resume cycle mid-playback: a mapped video
blob must survive replay, and should ride the machinery the m9 restore arc already has for
IOSurface-backed venus memory. The parked suspend/resume hardware-decode bounce is a candidate
to be explained by this, and should not be left to discover the restore path for us.

## What this does not fix

- **glupload's `DirectDmabuf` path renders near-blank**, and phase 1 is what exposed it. The
  guest's EGL advertises 63 importable dmabuf fourccs including NV12, so glupload builds ONE
  EGLImage over the whole planar buffer via
  `gst_egl_image_from_dmabuf_direct_target_with_dma_drm` and samples it as a single RGBA 2D
  texture; the result carries 2–25 distinct luma values against 256 in the source. The claim was
  always false — phase 1 only made the decoder export NV12, so something finally exercised it.
  The host log is clean across the failure, so this is guest-side EGL, not a rejected
  submission. Phase 2's per-plane import is the real fix; dropping NV12 from the advertised
  list, so glupload falls back to the copy uploader it already uses successfully, is the
  standing companion patch if phase 2 lands and the pipeline is still flat.
- **The composite RGBA view samples an empty texture on an IOSurface-backed target.**
  `upload_mapped_plane` writes the picture into the IOSurface planes and returns before it
  reaches `res->gl_id`, so a consumer that views the resource in its planar format — rather than
  importing the planes — gets whatever that texture last held. Per-plane import, which is what
  Firefox and every VA-API client do, does not go near it. Fill it on demand, or refuse the
  composite view on an IOSurface-backed target so the consumer takes the converting path.

  This gap is real but it is **not** what empties a gst-va picture; see below.

- **A context error latches, and every later decode is dropped without a word.**
  `vrend_hw_switch_context` refuses a context with `in_error` set, and
  `vrend_decode_ctx_submit_cmd` turns that refusal into a bare `EINVAL` before it decodes a
  single command. The flag never clears on the submit path, so one early error — a
  `create_sampler_view` naming a resource the context has not been given, which the gst-va
  import path provokes — silently kills every video submission that context makes for the rest
  of its life. Measured on the F44 enhanced image, mesa 26.1.8-8: a 30-frame `vavp9dec` run
  produces 1329 `ComponentError(22)` submissions, 0 VideoToolbox deliveries and 0 IOSurface
  writes, and hands back a luma plane of one distinct value. Firefox on the same boot decodes
  normally — 600 IOSurface writes carrying real pixels — because its context is never poisoned.
  The decoder reports success throughout; nothing in the host log names the cause, because the
  rejection path has no log line at all.

  Clearing the flag on the submit path is not the end of it. With the latch bypassed the
  commands execute and the decode reaches VideoToolbox — `create_codec`, a session reporting
  `hardware accelerated: yes`, ten `decode_bitstream`/`end_frame` pairs — and VT then refuses
  the data itself: `status -12909` (`kVTVideoDecoderBadDataErr`) on all ten frames, `decode
  produced no picture`, luma still one distinct value.

  The submission is not what differs. Traced where the host hands VideoToolbox the frame,
  the browser and the GStreamer pipeline carry byte-identical bitstreams — equal FNV hashes
  per frame, in the same order — with the same session parameters (`1920x1080 prof 0 depth 8
  sub 1 target fmt 166 cv '420v'`, `hardware accelerated: yes`) against the same kind of
  composite planar target. VideoToolbox accepts 300 of 300 from the browser and refuses 10 of
  10 from the pipeline. So the difference lives in the codec object's state rather than its
  input — the format description we build, or the session, per codec instance — and that is
  where the next probe goes.

  So an empty hardware-decoded picture here is at least three faults deep, and only the last
  one is about pixels: the latch drops every submission, VT rejects the submissions that do get
  through, and whichever sampling path the consumer picks then decides which shade of nothing
  it shows. Give the rejection a voice before anything else — a silent `EINVAL` on every submit
  is what made this read as a sampling bug for a day, and it hid the VT rejection behind it.
- **`glimagesink` poisons its virgl context** — a separate fault, on a different path, found
  alongside the above and not explained by it: 13 `CREATE_OBJECT` failures with EINVAL, after
  which 2652 consecutive `[SUBMIT3D]`s fail. It does not reproduce under `gldownload`.
- **The stock tier**, which runs vanilla mesa and keeps the one-page stub. The route there is
  upstreaming, not shipping our mesa to stock images.
- **A host with no AV1 silicon.** Pre-M3 hosts advertise no AV1 profile at all, on purpose, so
  `av01` content decodes in the guest whatever happens here (`docs/design/av1-decode.md`). On an
  M3-or-later host AV1 *is* offered, and this design is the only thing standing between Firefox
  and it — that is the dogfood Mac's case, not an independent one.
