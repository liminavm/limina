# Blob-backed decode targets

VA decode surfaces have no guest-visible storage. Their planes are allocated as ordinary
virgl resources whose pixels live on the host, so `drmPrimeHandleToFD` yields a one-page stub
at every resolution and an exported dmabuf names no frame memory at all. Measured, with a
reproducer, in `spikes/va-dmabuf-size`.

The stopgap in place today refuses that export. It stops the SIGBUS in consumers that mmap the
fd, and it takes Firefox's hardware decoder away outright — Firefox imports the fd on the GPU,
never maps it, and has no fallback but software. Neither half of that trade is worth keeping.
This is the fix that removes the trade instead of choosing a side: give the decode target
storage the guest can actually see.

## The constraint that shapes the whole design

**GPU writes are not visible to a guest CPU read.** `spikes/mtl-shm-coherency` measured
exactly that pairing: the same physical pages read fresh from the host CPU and stale from the
guest, across every cache attribute that is mechanically usable (CACHED reads stale; UNCACHED
SIGBUSes on zink's unaligned reads; WC crashes), with no working invalidate either — venus's
`virtgpu_bo_invalidate` is a documented no-op and `dc civac` SIGILLs at EL0. It is an old
measurement (2026-06-07, on the since-retired MoltenVK backend), but it has not been
overturned: the #28 backlog closure keeps venus feedback disabled precisely to avoid the path,
and `glReadPixels` is still called out as unreliable in the project guide.

**Host CPU writes, by contrast, reach the guest — and we rely on it already.** venus's shmem is
`VIRTGPU_BLOB_MEM_HOST3D` (`vn_renderer_virtgpu.c:1561`): host-allocated memory mapped into
the guest, carrying the ring the host writes completions into and the guest polls. When that
mapping is refused the ring never exists and venus is dead, which is exactly what
`spikes/hv-ipa-granule` measured from both sides. So the pairing this design rests on is
exercised every venus frame in production; the one that is unsafe is the one we are avoiding.

So a decoded frame must reach guest-visible memory **by a host CPU write**, never by a GPU
blit. That single fact settles what would otherwise be the design's open question:

- **The copy engine is `memcpy` from VideoToolbox's locked `CVPixelBuffer`.** Not a Metal
  blit, not a `glTexSubImage2D` into a shared texture. A GPU-written frame in guest-visible
  memory would be stale on arrival, and it would be stale *silently* — a plausible-looking
  frame one decode behind, which is the failure this stack is worst at diagnosing. The host CPU
  *is* in the GPU's coherency domain (same spike, Finding 1), so it may read VT's output
  freely; only the guest's view of GPU writes is unsafe.
- **This is not a cost we are adding.** Today the host already CPU-maps every decoded plane and
  uploads it (`upload_mapped_plane`, `vrend_video.c:156`), and the guest then pays a
  host→guest transfer on top to get pixels into its own memory. Replacing both with one host
  `memcpy` into memory the guest already maps is strictly less work, not more.

The host CPU's own view *is* coherent with the GPU (same spike, Finding 1), so the host stays
free to sample the same allocation on the GPU. Only the guest's CPU view is constrained.

## Shape

One stable host allocation per decode target, mapped into the guest for the target's lifetime:

```
VideoToolbox  ->  its own CVPixelBuffer pool   (VT keeps its pool; we do not fight it)
                        |
                        |  host CPU memcpy, per plane, honouring VT's row padding
                        v
              host allocation  == the decode target's storage
                        |
                        |  mapped once, at buffer creation, as a HOST3D mappable blob
                        v
                    guest BO  ->  drmPrimeHandleToFD  ->  a dmabuf that names the frame
```

VT hands back a different buffer from its pool each frame while the guest's decode target is a
fixed surface it reuses, so the guest-visible allocation must be ours and stable; per-frame
remapping is neither cheap nor race-free. That is what makes the copy structural rather than
an implementation shortcut.

The machinery this needs mostly exists. The guest winsys already creates
`VIRTGPU_BLOB_MEM_HOST3D` blobs with `VIRTGPU_BLOB_FLAG_USE_MAPPABLE`
(`virgl_drm_winsys.c:224`); libkrun's `resource_map_blob` maps a host pointer obtained from
`virgl_renderer_resource_get_map_ptr` into the guest's shm region; virglrenderer already
allocates IOSurfaces and reports their CPU base and stride
(`vkr_mtl_iosurface_get_layout`). What is missing is a video resource that opts into that
path, and a layout contract between the two ends.

## Who dictates layout

**The guest computes the layout; the host allocates to match, or refuses.** The guest must
report offsets, pitches and sizes in the export descriptor, and it cannot report what it does
not choose. IOSurface accepts explicit per-plane `bytesPerRow` at creation, subject to
`IOSurfaceGetPropertyAlignment`, so the host can usually honour a guest layout exactly; where
it cannot, it fails the buffer creation and the guest falls back to today's path rather than
proceeding with two disagreeing pictures of one allocation.

The alternative — host allocates, guest queries — costs a wire round-trip at every buffer
creation and buys nothing, because the guest still has to be told before it can export.

NV12 becomes **one object with two layers** at distinct offsets, which is what real drivers
report and what `VADRMPRIMESurfaceDescriptor` is shaped for. Today's two-objects-of-4096 is an
artefact of per-plane resources, not a format requirement.

## Capability negotiation, and the order this ships in

A guest mesa that allocates blob decode targets against a host that cannot back them must fall
back, not fail. So the host advertises a **capset bit** and
`virgl_video_create_buffer` checks it before choosing the allocation path; without the bit it
calls `vl_video_buffer_create` exactly as it does today.

That makes the two sides independently shippable, and fixes the order:

1. **virglrenderer first.** A host that can back blob decode targets, advertising the bit. No
   guest change, so no behaviour change — nothing asks for it yet.
2. **Guest mesa second.** Now the enhanced tier lights up. The delivery chain is the usual one:
   fork commit → `scripts/export-mesa-guest-patches.sh` → mesa RPM →
   `scripts/provision/deliver-payload.sh` over the enhanced images → `docs/images.md`.

Never the other way round: a guest-enabling change ahead of its host fix is the mistake
`limina-enh-delivery` records. It also keeps the capability granular in the sense the two-tier
guarantee asks for — a partially upgraded guest gets the old path for video and keeps
everything else.

## The export refusal is not removed — it lifts itself

The guard refuses an FD export whose laid-out size exceeds the guest storage behind it. A
blob-backed target has storage at least as large as its layout, so the export simply passes.
Nothing needs deleting, and the guard goes on protecting every resource that still arrives
unbacked — including the stock tier, which keeps today's behaviour until the refusal is
upstreamed.

That property is worth preserving deliberately: the phase that fixes video must not be the
phase that reopens the SIGBUS for everything else.

## Phases

**Phase 1 — correct, guest-visible frames.** The allocation, the mapping, the layout contract,
the capset bit, the host CPU copy. At the end of it the export is honest, GStreamer's mmap
path reads correct pixels instead of crashing, and **Firefox has its hardware decoder back** —
Firefox needs only that the export succeed and the EGL import resolve, both of which it did
before the refusal existed.

**Phase 2 — actual zero copy.** Make the same allocation the GL texture's storage so a guest
GPU import samples the frame without any copy, and fix glupload's direct importers, which
refuse everything today with `cannot produce texture-target 2D` and fall back to the copy
uploader regardless of how well-formed the buffer is.

Phase 2 is where the performance win lives; **phase 1 is where the correctness win lives**, and
phase 1 stands alone. Do not gate the Firefox recovery on the importer work.

## Spikes, before any wiring

1. **Host-CPU-write → guest-CPU-read across a mapped blob, at video sizes.** Not a gate — the
   venus ring depends on this pairing every frame — so the question is not whether it works but
   whether it still holds for a multi-megabyte mapping written in bulk rather than a ring
   written in small records. Cheap to answer: host writes a known pattern through the mapping,
   guest mmaps the blob and checksums, with no synchronisation of any kind. Worth doing early
   because everything downstream assumes it.
2. **Can an IOSurface's base address be mapped into the guest at all?** `get_map_ptr` returns a
   host VA and the macOS path maps host VAs, so this should reduce to reporting the right
   pointer — but if IOSurface memory cannot be mapped, phase 1 falls back to our own shm
   allocation (which costs phase 2 its zero-copy story, so this is the fact that decides
   whether the two phases share one allocation).
3. **Layout agreement.** Allocate at a guest-chosen `bytesPerRow` and confirm
   `IOSurfaceGetPropertyAlignment` accepts it at the sizes we care about, including odd widths
   and 4K.

Spike 2 is the one that branches the design. If IOSurface memory cannot be mapped into the
guest, phase 1 falls back to a plain host allocation and phase 2 loses its zero-copy story,
because the frame the guest maps and the frame the host samples would stop being one
allocation.

## Verifying

Two consumers, because they fail differently, and neither failure is a crash:

- **GStreamer (mmap path).** Frames checksummed against the software decoder, not merely
  "no SIGBUS". A coherency fault produces a frame one decode behind — it plays, it looks like
  video, and byte-equality is the only thing that catches it.
- **Firefox.** `spikes/vt-vp9-decode/guest-ff-vaapi-check.sh` and its three verdict lines, with
  hardware **retained across a full session** rather than for one frame. The current regression
  is precisely a decoder that reports `IsHardwareAccelerated=true` and then falls back, so a
  single-frame check would pass against the bug it is meant to catch.

Plus the `l2_video_vaapi` extension, and one suspend/resume cycle mid-playback: a mapped video
blob must survive replay, and it should ride the same machinery as the unmappable venus
memory the m9 restore arc already handles (`limina-m9-suspend-resume`). The parked
suspend/resume hardware-decode bounce is a candidate to be explained by this, and should not be
left to discover the restore path for us.

## What this does not fix

- **The stock tier.** Stock guests run vanilla mesa and keep the one-page stub. The route there
  is upstreaming — both the refusal and, eventually, this — not shipping our mesa to stock
  images.
- **AV1.** Unrelated and independent: YouTube negotiates `av01` with Firefox, and there is no
  AV1 hardware decode on any tier yet. Blob-backed targets change nothing about that; see
  `docs/design/av1-decode.md`.
