# Blob-backed decode targets

VA decode surfaces have no guest-visible storage. Their planes are allocated as ordinary virgl
resources whose pixels live on the host, so `drmPrimeHandleToFD` yields a one-page stub at every
resolution and an exported dmabuf names no frame memory at all. Measured, with a reproducer, in
`spikes/va-dmabuf-size`.

The fix is to give the decode target storage the guest can see. The stopgap it replaced refused
that export outright, which stopped the SIGBUS in consumers that mmap the fd and took Firefox's
hardware decoder with it — Firefox imports the fd on the GPU, never maps it, and has no fallback
but software. Neither half of that trade was worth keeping. The refusal is gone (§The export
refusal is gone) and the storage is real.

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

One blob per decode target, in place of ordinary per-plane resources. **What backs that
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

The guest side is `virgl_video_create_buffer` (`virgl_video.c:1242`). Deferring to
`vl_video_buffer_create` is what yields ordinary per-plane resources; it allocates from a blob
instead, and still furnishes `get_sampler_view_planes`, which the same function consumes.

NV12 becomes **one object with two layers** at distinct offsets — what real drivers report and
what `VADRMPRIMESurfaceDescriptor` is shaped for. The two-objects-of-4096 form is an artefact of
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

That copy is **not new cost**: the host already CPU-maps every decoded plane and uploads it
with `glTexSubImage2D` (`upload_mapped_plane`, `vrend_video.c:156`), and the per-plane form made
the guest pay a host→guest transfer on top to get pixels into its own memory. One copy into
memory the guest already maps replaces both. The copy engine is a free choice — CPU `memcpy` or a Metal blit —
because coherency constrains neither.

**Do not contort the layout to make pitches agree.** The row-by-row copy and the single
whole-plane `memcpy` are indistinguishable at these sizes; which one wins reordered between runs.
Only bandwidth matters, so the guest picks whatever layout suits it.

## Who dictates layout

**Layout is dictated, never discovered.** The guest must report offsets, pitches and sizes in
the export descriptor and cannot report what it did not choose. Where the storage is guest memory
there is no second allocator to disagree with. Where it is an IOSurface,
`vkr_mtl_iosurface_alloc_planar` supplies an explicit `kIOSurfacePlaneInfo` array — width,
height, bytes-per-element, bytes-per-row and offset for every plane — and then reads
`IOSurfaceGetBytesPerRowOfPlane` back and **refuses the surface if the kernel overrode it**
(`[KK-STRIDE]`). Refusing costs a fallback to the copy path; keeping it would hand the guest
offsets that do not describe the surface.

`IOSurfaceGetPropertyAlignment` is advisory once explicit plane info is supplied: no rounding was
applied in any case tested, odd widths and heights included, and an 854-byte pitch was accepted.
**Letting IOSurface choose is what rounds** — measured 2026-09-04 on an M1 Max, a surface created
without plane info gives plane 0 a pitch of 128 at width 64, 128 at 65 and 384 at 352, while
1280 and 1920 come back exact. That coincidence at common widths is why the plane info is not
optional.

**The pitch we dictate is not the tight one either.** Each plane's is
`align_up(width * bpe, minimumLinearTextureAlignmentForPixelFormat)` for the format the plane is
*sampled* as — R8 or RG88 — queried from Metal rather than hardcoded, because a plane is imported
as its own single-component texture and the composite surface has no `MTLPixelFormat` to align
to. So it equals the guest's tight stride only where `width * bpe` is already a multiple of that
alignment. Aligning for the wrong format has a measured precedent: a whole-surface pitch aligned
as if for the composite slid every row of every GL window sideways by exactly
`(align256 - align16)/4` pixels.

**Dictating the pitch does not make the two layouts agree**, and reading it that way is the
available mistake. It settles who chooses, not what is chosen: the guest's layout stays tight and
the surface's stays aligned, so they part at every width the alignment does not divide — which is
most widths. The copy is still what reconciles them (§Phases).

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
its own width times its own block size. A divergence cannot corrupt silently *while the writeback
copy stands between the two layouts* — too large trips its extent check, too small is visible in
the picture. That protection ends when the surface becomes the storage; see §Phases.

## Capability negotiation, and the order this ships in

A guest that allocates blob decode targets against a host that cannot back them must fall back,
not fail. The host advertises a capset bit; `virgl_video_create_buffer` checks it and otherwise
calls `vl_video_buffer_create` as before. That makes the two sides independently
shippable and fixes the order:

1. **virglrenderer first** — a host that can back blob decode targets, advertising the bit.
   Nothing asks for it yet, so no behaviour changes.
2. **Guest mesa second** — the enhanced tier lights up, via the usual chain: fork commit →
   `scripts/export-mesa-guest-patches.sh` → mesa RPM → `scripts/provision/deliver-payload.sh` →
   `docs/images.md`.

Two capset bits, not one, because a host can do the guest-memory writeback without accepting the
composite shape — which is exactly what shipped first. `VIDEO_GUEST_PLANES` buys real guest
storage per plane and an honest export; `VIDEO_PLANAR_TARGET` buys the composite create.

The bit is necessary, not sufficient: `VIDEO_PLANAR_TARGET` says the host takes the composite
shape at all, and the **sampler bitmask says for which planar formats**. The guest must decide
from those before creating, because a refused create never reaches it — the kernel has already
handed out the handle, and the first thing anyone sees is a poisoned context (§What this does
not fix). So the host advertises a planar format as samplable only when
`vrend_planar_target_backable` accepts it, and the guest asks for the composite shape only then.

Never the reverse; a guest-enabling change ahead of its host fix is the mistake
`limina-enh-delivery` records. It also keeps the capability granular, per `docs/graphics.md`
§3.4 — a partially upgraded guest gets the old path for video and keeps everything else.

## The export refusal is gone

The guard refused an FD export whose laid-out size exceeded the guest storage behind it. It was
written for decode targets and their one-page stub, but the predicate is true of **every staged
texture**: with copy-transfer in both directions `virgl_resource_create_front` gives every
non-shared texture the same `alloc_size = 1`, so `eglExportDMABUFImageMESA` failed for
essentially every GL texture. GTK4's `gdk_texture_download` then consumed an uninitialized fd,
and setting a user avatar in GNOME Settings wrote noise — RMSE 0.774 against the source, 0 on
vanilla 26.1.8 with only libgallium swapped. Dropped in guest mesa `26.1.8-10.limina`
(`patches/mesa-guest/0018`), alongside an EGL layer that now returns `EGL_FALSE` when a driver
cannot export an fd instead of leaving the caller's fd array untouched (`0019`).

What makes dropping it safe is this design. A decode target has real guest memory on every host
that offers a decoder, so the case the guard existed for cannot arise; `whandle->size` is still
filled in, so a consumer that reads the object size sees the memory it was handed; and a
stub-backed fd remains a good name for the host resource to a consumer that re-imports rather
than maps it, which `virgl_resource_from_handle` already marks staged.

The rule that generalises: **a guard whose predicate is broader than the case it was written for
will fire somewhere else first.** This one shipped in `-5` and was found three releases later
from the wrong end, as a corrupted avatar.

## Phases

**Phase 1 — correct, guest-visible frames on guest-memory storage — shipped.** Allocation,
layout contract, capset bits, and the frame landing in the target's blob, confined to
virglrenderer and guest mesa. The export is honest, GStreamer's mmap path reads correct pixels
instead of crashing, and **Firefox has its hardware decoder back** — it needed only that the
export succeed and the EGL import resolve, both of which it had before the refusal existed.

**Phase 2 — the remaining copies — largely shipped.** The storage is an IOSurface: a composite
target is backed by one two-plane surface whose planes are EGL-bound and sampled directly
(`vrend_resource_iosurface_init_planes`), and the plane index reaches Metal. Two things remain.
glupload's direct importers still refuse everything with `cannot produce texture-target 2D` and
fall back to the copy uploader however well-formed the buffer is. And the layout contract below
is still owed, because the guest-memory writeback is what currently hides it.

**The copy is what reconciles the two layouts, so removing it is the open work.** The guest
computes tight; the surface is Metal-aligned (§Who dictates layout); they differ wherever
`width * bpe` is not already aligned. Nothing breaks today because a copy sits between them:
`writeback_plane_to_guest` walks the source at the decoded buffer's own `plane->pitch` and the
destination at `guest_pixels_stride`, moving one tight row at a time (`vrend_video.c:405`). Two
strides in one loop, deliberately. So no host pitch reaches the guest, and
`get_param(PIPE_RESOURCE_PARAM_STRIDE)` correctly describes the guest BO — which is the storage
its consumers map. The extent check bounds the destination, so it too is entirely in guest
arithmetic; the one guard that could fire, `stride < row`, needs the *guest* stride to be short
and a padded host pitch cannot trip it.

When the surface becomes the texture's storage that copy disappears, and the reconciliation with
it. The export then names storage with padded rows while the guest still reports a tight stride,
and the result is a sheared picture rather than an error. So **the layout must then be
transmitted by the host that allocated it** rather than recomputed at both ends. The carrier
exists on the import path — SET_TYPE's `plane_strides`/`plane_offsets` overwrite
`guest_pixels_*`, which is why `vrend_resource_init_planar_guest_layout` is written to lose to it
— but a composite target is created directly and has no such path. **Solve that before the copy
comes out, not after.**

Two rules meanwhile: never derive `guest_pixels_stride` from a surface pitch, which would write
padded rows into tight guest storage; and never treat the two strides as interchangeable because
they agree at the common widths.

**Every other mechanism phase 2 needs already ships.** vrend adopts an IOSurface as a GL
texture's storage in `vrend_resource_iosurface_init`; the VMM's `resource_map_blob` is
context-agnostic, gated only on `map_ptr` succeeding, so a mappable vrend blob needs no VMM work;
VideoToolbox already decodes into IOSurface-backed `CVPixelBuffer`s (`virgl_video_vt.c:857`); and
the import carries a plane index and R8/RG88 formats through `virgl_egl_image_from_iosurface`,
the `EGL_IOSURFACE_LIMINA` target, `dri2_from_iosurface_limina` and zink's
`resource_from_handle`.

The zink→KK step was the one with no carrier, and it now has one. `VK_EXT_external_memory_metal`
imports a surface as a bare `VkImportMemoryMetalHandleInfoEXT::handle` with nowhere to say
"plane 1", so every import defaulted to plane 0 and a biplanar target could expose only its luma.
The index rides in `VkImportIOSurfacePlaneLIMINA`, chained onto the import
(`src/kosmickrisp/bridge/kk_limina_plane.h` on `limina-kk`), and the whole chain is threaded:

    eglCreateImageKHR(EGL_IOSURFACE_LIMINA, {PLANE, FOURCC})
      → dri2_from_iosurface_limina(plane, pipe_format)
      → winsys_handle::plane   (the field already existed for dmabuf)
      → VkImportIOSurfacePlaneLIMINA
      → newTextureWithDescriptor:iosurface:plane:

Both ends are on our branch, so it is an internal contract kept in one header rather than an ABI,
shaped after `VK_EXT_metal_objects`' `VkImportMetalTextureInfoEXT` — which carries exactly this as
an aspect bit — to keep a later migration mechanical. With no attribs the behaviour is unchanged:
whole surface, format from the surface, plane 0, which is the shipping scanout and shared-buffer
import. With attribs the surface's own fourcc is deliberately not consulted, since a planar
surface reports `420f`, the name of the pair, which is neither plane's texture format.

Deriving the plane by matching the dedicated image's dimensions against the surface's was the
alternative and is rejected: it happens to be unambiguous for 4:2:0 and would stop being so,
silently, for any other subsampling. Oracle: `spikes/vrend-iosurface/planeimport-probe.c`.

What remains for phase 2 is the layout contract above, not the index.

**The guest half is the prerequisite, because the host half is inert without it.** Measured
2026-09-01 on a guest still taking the per-plane form, VP9 through `vavp9dec ! glupload`: the
host saw *two* resources per decode target, `PIPE_FORMAT_R8_UNORM` at luma size and
`PIPE_FORMAT_R8G8_UNORM` at chroma size — 107 of each across the clip, and not one planar-format
resource. On the lowered path each plane is already its own resource with its own texture, so no
host-side plane machinery is reachable at all: `vrend_resource_iosurface_init` never sees a
planar format. Any host work here is dead code until the guest routes through
`vl_video_buffer_create_as_resource`.

**Sampling the second plane needed no guest change, because both ends of that path already
existed.** The host keeps a separate EGLImage per plane in `aux_plane_egl_image`, and the guest
already names the plane: `metadata.plane` is written as the sampler view's whole layer dword, so
it arrives as `first_layer = N, last_layer = 0`. A genuine layer range never has `last_layer`
below `first_layer`, which is what makes the encoding unambiguous rather than a guess. A guest
that imports a decode target's planes as separate component-format resources — what the dmabuf
importers do — is therefore already asking for plane N by index. The host work was to put an
image there, which `vrend_resource_iosurface_init_planes` now does; where no image answers the
index it is still cleared, which is correct for the lowered path, where sampling the resource
already is sampling the plane.

**The aux bind sits ahead of the texture-view branch, and must stay there.** A surviving index
sets `needs_view`, and `glTextureView` would then be asked for a zero-layer view and return
EINVAL, putting the whole context in error for its lifetime. The GBM path reaches its aux bind
below the view branch only because its EGLImage fallback strips `VREND_STORAGE_GL_IMMUTABLE` when
`EXT_EGL_image_storage` is absent; on zink-on-KK it is present, so the bit survives and the view
branch would win. Order is load-bearing here, not stylistic.

The guest half of the *allocation* is smaller than the host half, and needs no new protocol
concept. The one-object
shape is `vl_video_buffer_create_as_resource` (`vl_video_buffer.c:517`): it calls
`resource_create` once with the planar format, takes planes 1 and 2 from the chained
`resources[0]->next`, and sets `contiguous_planes`. Gallium's VA frontend already exports that
as one object with two layers (`va/surface.c:1453`), gated on `screen->resource_get_param`,
which virgl installs. The guest work was therefore to route `virgl_video_create_buffer` through that
constructor instead of `vl_video_buffer_create`, give virgl's `resource_create` the plane
chaining a planar format implies, and answer `PIPE_RESOURCE_PARAM_STRIDE` and `_OFFSET` in
`virgl_resource_get_param`, which had handled only `MODIFIER`. All three shipped in guest mesa
`26.1.8-7.limina` (`patches/mesa-guest/0014`), with `get_param` walking the chain by the plane
argument the way every driver that builds one does, because that is how gst-va asks.

That also settles how the host tells a decode target apart from anything else, with no new
flag on the wire: it is a single `PIPE_RESOURCE_CREATE` carrying a planar format, arriving
through the ordinary `vrend_renderer_pipe_resource_create` blob path. `vrend_resource_iosurface_init`
discriminates on format there, and a planar format is the **first** case it dispatches — ahead of
the SCANOUT/SHARED bind gate, which a decode target is neither of and would never pass.

Phase 1 is the correctness win and it stands alone: the Firefox recovery was deliberately not
gated on the plane work or the importer work, and a later phase must not re-couple them.

**Phase 1 costs guest RAM, by design.** Decode targets stop being one-page stubs and become
their real size, and a decoder holds a whole DPB of them — a 16-deep 4K NV12 DPB is ~190 MB
that used to be 16 pages. That is the price of an fd that names the picture it claims to, but
it is large enough to look like a leak to someone bisecting guest footprint later, so it is
written down here. Encode source buffers ride the same flag and grow the same way; harmless,
and equally not a leak.

## Spikes

1. ✅ **Can VideoToolbox decode into surfaces we supply?** No — but it honours a layout we
   dictate, and the copy that follows costs ~0.10 ms at 1080p. `spikes/vt-blob-decode-target/`.
2. ✅ **Layout agreement.** IOSurface honours dictated per-plane pitches exactly, odd dimensions
   included, once explicit `kIOSurfacePlaneInfo` is supplied — and rounds when it is left to
   choose. Same spike. The pitch we dictate is Metal's linear alignment for the plane's own
   sampled format, not the guest's tight stride; §Who dictates layout.
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

Plus `l2_video_vaapi_restore`: a hardware decode in flight across a managed suspend/restore.
The codec and its video buffers are journaled and re-created at replay, and the re-created codec
drops inter frames until the stream's next keyframe, so the oracle compares the *tail* of the
hardware output against the software decoder and requires zero "decoding into nothing" lines
after the restore. The decode targets themselves are ordinary journaled blob resources and need
nothing more.

## What this does not fix

- **A composite view is filled on the GPU, and only for consumers that build one.** A planar
  target has two kinds of consumer. Per-plane import — Firefox, every VA-API client — samples
  the IOSurface planes through their own images. A *composite* view samples the resource's own
  RGBA texture: dri2 builds one whenever the driver reports the planar format samplable
  (`dri2_create_image_from_winsys`, the `use_lowered` gate), which is exactly what glupload's
  `DirectDmabuf` path gets for its one EGLImage over the whole NV12 buffer. Nothing on the decode
  path filled that texture — the delivery wrote the IOSurface planes and returned — so Showtime
  decoded every frame in hardware and drew black, with a clean host log. Reporting NV12
  unsamplable in the guest would lower the import to per-plane views, but the same report types
  an untyped guest-memory blob at import, and those have no plane images; the gap is host-side.
  `vrend_renderer_convert_planes_gl` (vrend_blitter.c) draws the two planes into the base texture
  with a BT.601 program in the blit context — the CPU converter's matrix and range, so the two
  fills agree — at the first composite view of a resource and at every delivery once one exists,
  never inside a draw's sampler bind. The pass's plane textures are made once per resource and
  die before the plane images at destroy, so the retain-count trace keeps its meaning. Firefox
  never creates a composite view and never pays (1,115 per-plane views, 0 composite, one run).
  The host log says when a resource enters this path: `composite view: WxH ... is sampled whole`.
  Colour space is the standing limitation of both fills: the guest's EGL hint never reaches the
  host, so HD content encoded BT.709 comes out slightly off.
- **A refused composite create is invisible to the guest, so the capset is the contract.** The
  kernel hands out the handle before the host is asked; a create the host refuses leaves the
  guest attaching backing and building sampler views on a resource that does not exist, the host
  reports `Illegal resource`, the context's error flag latches, and every later submission from
  that process — the decode included — is dropped. gst-va provokes this at plugin registration,
  creating a 64×64 surface of every fourcc it knows to learn each one's derived layout. So the
  guest takes the composite shape only for a multi-plane format the host's sampler bitmask lists,
  and the host lists only what its planar IOSurface backing can take (NV12, NV21). The refusal in
  `vrend_resource_alloc_texture` stays as a loud last line for a broken contract, not as a
  negotiation. Measured on the F44 enhanced guest: 0 of 50 gst-va decodes produced a picture
  before, 50 of 50 after; the stock guest, which never consults the bit for a planar format,
  decoded 50 of 50 throughout.
- **Every planar surface must die with its resource, and two things kept them alive.** Each
  planar IOSurface is held by our handle, the registry, and one Metal texture per plane
  (KosmicKrisp adopts the imported texture with one retain per image plane). The per-plane
  EGLImages were destroyed only when the resource also had a base image, which a planar target
  never has; and zink filed the plane-1 import as an aux plane — its test is "plane index at or
  past the plane count of the handle's format", and our import passes the plane's own
  single-plane format — so it skipped `DestroyImage` and the plane's texture retain never came
  off. Either alone holds every decode target and every 64×64 probe forever: ~90 surfaces per
  gst-va process, `IOSurfaceCreate` refusing at ~16.4k live, and from then on every hardware
  decode poisoning its context. Both fixed (virglrenderer `ede7bb19`, limina-kk `78d7ac6602b`).
  The oracle is `LIMINA_GPU_MEM_BUDGET_CENSUS=<secs>`: `DEALLOC iosurface N (alive M)` must
  track the allocations, with M the scanout ring; `LIMINA_SURF_REFTRACE=N` prints the retain
  count around each plane image's teardown when it does not. Measured: 15,009 of 15,014
  deallocated over 200 runs, worker steady at 23 IOSurface mappings.
- **A keyframe in ~100 reached the decoder zeroed, wholly or from an arbitrary offset on** —
  VideoToolbox refused it (`-12909`) and every frame behind it failed for want of a reference,
  so that run showed nothing. Cause, host-side: the bitstream buffer is a `PIPE_BIND_CUSTOM`
  resource, which vrend backs with a zero-filled host shadow (`res->ptr`), and
  `vrend_pipe_resource_attach_iov` wrote that shadow into the guest backing on every
  `ATTACH_BACKING`. The guest kernel queues `RESOURCE_CREATE` + `ATTACH_BACKING` and returns to
  userspace without waiting, so mesa's `memcpy` of the bitstream races the host's attach: when
  the copy came first, the attach zeroed it behind the guest's back (whole buffer, or from
  wherever the copy had reached when the shadow write overtook it). Fixed in virglrenderer:
  the shadow is written back on attach only once it holds content the backing does not (a
  detach copied the backing into it, or a transfer wrote it while unattached — `ptr_valid`).
  Upstream `main` carries the same unconditional write-back. Measured after the fix: 0 of 300
  runs, and 0 of 60 with the arm-time write-watch that made the race fire 30-50% of the time
  before. The general rule: **the host must never write into guest backing at attach time
  unless it is restoring content the guest cannot have** — a guest that has the handle may
  already be writing through its mapping.
- **The GStreamer registry outlives the fix.** The pre-`-8` abort left `libgstva.so`
  blacklisted in `~/.cache/gstreamer-1.0/registry.*.bin`, and the registry re-validates a plugin
  only when the plugin file itself changes, not its dependencies — so after the mesa upgrade
  every GStreamer app still reports `no element "vavp9dec"` until that cache is removed. A fresh
  enhanced image ships with the blacklist already in place. Delivery has to clear it.
- **`glimagesink` poisons its virgl context** — a separate fault, on a different path, found
  alongside the above and not explained by it: 13 `CREATE_OBJECT` failures with EINVAL, after
  which 2652 consecutive `[SUBMIT3D]`s fail. It does not reproduce under `gldownload`.
- **The stock tier**, which runs vanilla mesa and keeps the one-page stub. The route there is
  upstreaming, not shipping our mesa to stock images.
- **A host with no AV1 silicon.** Pre-M3 hosts advertise no AV1 profile at all, on purpose, so
  `av01` content decodes in the guest whatever happens here (`docs/design/av1-decode.md`). On an
  M3-or-later host AV1 *is* offered, and this design is the only thing standing between Firefox
  and it — that is the dogfood Mac's case, not an independent one.
