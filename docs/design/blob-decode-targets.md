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

**The `fd_type` caveat does not apply here.** vrend cannot import a dmabuf on macOS and instead
re-reads guest iovecs before each sampling batch (`docs/graphics.md` §3.2) — but that is the
path for a `BLOB_MEM_GUEST` resource, whose pixels genuinely live in guest RAM. A decode target
is host-allocated; the host already holds the memory and never reads guest pages for it.

## Shape

One IOSurface per decode target, allocated host-side and mapped into the guest for the
target's lifetime:

```
VideoToolbox  ->  its own CVPixelBuffer pool
                        |
                        |  one host copy into the target's IOSurface  (see below)
                        v
        IOSurface  ==  the decode target's storage
             |                    |                         |
     map_ptr -> guest BO    GPU binds it            iosurface_id
     (mmap, dmabuf export)  (sampling, zero copy)   (scanout, zero copy)
```

The guest side is `virgl_video_create_buffer` (`virgl_video.c:1242`), which today defers to
`vl_video_buffer_create` and so gets ordinary per-plane resources. It allocates from a blob
instead, and must still furnish `get_sampler_view_planes`, which the same function consumes at
line 1262.

NV12 becomes **one object with two layers** at distinct offsets — what real drivers report and
what `VADRMPRIMESurfaceDescriptor` is shaped for. Today's two-objects-of-4096 is an artefact of
per-plane resources, not a format requirement.

## The copy, and whether it is needed at all

VideoToolbox owns its output pool and hands back a different `CVPixelBuffer` per frame, while
the guest's decode target is a fixed surface it maps once. Re-pointing a guest mapping every
frame is neither cheap nor race-free, so by default something lands the frame in the target's
own storage.

That copy is **not new cost**: today the host already CPU-maps every decoded plane and uploads
it with `glTexSubImage2D` (`upload_mapped_plane`, `vrend_video.c:156`), and the guest pays a
host→guest transfer on top to get pixels into its own memory. One copy into memory the guest
already maps is strictly less work than that, and the copy engine is a free choice — CPU
`memcpy` or a Metal blit — because coherency constrains neither.

It may also be avoidable outright: `VTDecompressionSessionCreate` takes
`destinationImageBufferAttributes`, so a session may accept IOSurface-backed buffers we supply.
If it does, there is no copy at all. That is the first spike, because it is the difference
between one copy per frame and none, and it changes nothing else in the design.

## Who dictates layout

**The guest computes the layout; the host allocates to match, or refuses.** The guest must
report offsets, pitches and sizes in the export descriptor and cannot report what it did not
choose. IOSurface takes explicit per-plane `bytesPerRow` at creation, subject to
`IOSurfaceGetPropertyAlignment`; where it cannot honour the guest's layout it fails the buffer
creation and the guest falls back, rather than two ends proceeding with disagreeing pictures of
one allocation.

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

**Phase 1 — correct, guest-visible frames.** Allocation, mapping, layout contract, capset bit,
and the frame landing in the target's storage. At the end of it the export is honest,
GStreamer's mmap path reads correct pixels instead of crashing, and **Firefox has its hardware
decoder back** — it needs only that the export succeed and the EGL import resolve, both of
which it had before the refusal existed.

**Phase 2 — the remaining copies.** Bind the IOSurface as the GL texture's storage so host-side
sampling needs no upload, and fix glupload's direct importers, which refuse everything today
with `cannot produce texture-target 2D` and fall back to the copy uploader however well-formed
the buffer is.

Phase 1 is the correctness win and it stands alone. Do not gate the Firefox recovery on the
importer work.

## Spikes

1. **Can VideoToolbox decode into surfaces we supply?** Decides whether this copies per frame or
   not at all. Create a session with `destinationImageBufferAttributes` naming IOSurface-backed
   buffers of our layout and see whether VT honours them or silently uses its own pool.
2. **Layout agreement.** Allocate at a guest-chosen `bytesPerRow` and confirm
   `IOSurfaceGetPropertyAlignment` accepts it at the sizes we care about — odd widths, 4K.
3. **Can an IOSurface's base address be mapped into a guest at all?** This one branches the
   design, and nothing in the stack answers it today: the venus precedent maps shm-backed
   `vkMapMemory` pointers, and zero-copy scanout hands surfaces over by id and never maps
   them. IOKit-owned pages may not survive `hv_vm_map` the way anonymous ones do, and the
   guest can never take part in the `IOSurfaceLock` protocol. Map a real surface's base into a
   guest and checksum what both sides see — at decode-target size, since these are
   multi-megabyte and written whole rather than small and hot. If it fails, phase 1 falls back
   to an shm-backed blob plus a copy into the surface, and phase 2's zero-copy present story
   changes with it.

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
