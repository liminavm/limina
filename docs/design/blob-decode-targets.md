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

**Phase 2 costs four layers, and the reason is worth knowing before starting it.** vrend already
adopts an IOSurface as GL texture storage — the shipping path that composites a venus client's
window buffer (`vrend_renderer.c:15159`). But that import is whole-surface and 8888-only: every
layer drops the plane index, right down to Metal, whose `newTextureWithDescriptor:iosurface:plane:`
takes one and is passed a hardcoded `0`. An NV12 target needs a plane index and R8/RG88 formats
threaded through `virgl_egl_image_from_iosurface` (`vrend_winsys_egl.c:852`), the attribute-less
`EGL_IOSURFACE_LIMINA` target (`egl_dri2.c:2660`), `dri2_from_iosurface_limina` (`dri2.c:940`),
zink's `resource_from_handle`, and KK's `mtl_new_texture_with_descriptor_iosurface`
(`mtl_device.m:399`) — the last three on the host mesa `limina-kk` branch. Nothing works until
the whole chain lands, which is precisely why it is not phase 1.

Phase 1 is the correctness win and it stands alone. Do not gate the Firefox recovery on either
the plane work or the importer work.

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

- **The stock tier**, which runs vanilla mesa and keeps the one-page stub. The route there is
  upstreaming, not shipping our mesa to stock images.
- **A host with no AV1 silicon.** Pre-M3 hosts advertise no AV1 profile at all, on purpose, so
  `av01` content decodes in the guest whatever happens here (`docs/design/av1-decode.md`). On an
  M3-or-later host AV1 *is* offered, and this design is the only thing standing between Firefox
  and it — that is the dogfood Mac's case, not an independent one.
