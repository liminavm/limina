# Spike 1: can VideoToolbox decode into storage we choose?

Spike 1 of `docs/design/blob-decode-targets.md`. The design is the same shape either
way; what turns on this is whether the host copies each decoded frame into the decode
target's IOSurface, or hands the guest the decoder's own output untouched.

**Verdict: one copy per frame, and it is cheap enough not to constrain the design.**
VideoToolbox will not decode into a buffer we supply — no API accepts one — but it
*will* produce IOSurfaces in a layout we dictate exactly, on the hardware decoder, at
no measurable cost. So the host allocates the target, tells VT to match its layout,
and copies: ~0.10 ms at 1080p, ~0.42 ms at 4K, against 16.7 ms of frame budget at
60 fps.

Measured 2026-09-01 on macOS 26.5, M1 Max, VP9 hardware decode, fixture
`../vt-vp9-decode/hidden-frames.ivf` (352x240, 107 decoded frames).

```
cc -O2 -o probe probe.c -framework VideoToolbox -framework CoreMedia \
   -framework CoreVideo -framework IOSurface -framework CoreFoundation
./probe layout ../vt-vp9-decode/hidden-frames.ivf
./probe pool   ../vt-vp9-decode/hidden-frames.ivf
./probe copy
./probe alloc
```

## There is no destination-buffer API — settled from the headers

All six decode entry points take a `CMSampleBufferRef` in and deliver a
`CVImageBufferRef` out; none accepts a destination buffer
(`VTDecompressionSession.h:185, 256, 475, 517, 561`, plus the multi-image handler at
`444`). `VTDecompressionSessionDecodeFrameWithOptions` (macOS 15) looks like the place
such a thing would live, but its `frameOptions` dictionary admits only keys with the
`kVTDecodeFrameOptionKey_` prefix, and the SDK defines exactly two —
`ContentAnalyzerRotation` and `ContentAnalyzerCropRectangle`
(`VTDecompressionProperties.h:525, 537`).

`destinationImageBufferAttributes` (`VTDecompressionSession.h:113`) constrains the
attributes of the pool VideoToolbox creates for itself. It never lets us name a buffer.
That is the whole of the negative, and it needs no probe.

## What the session *will* honour

Every requested row alignment was applied exactly, and the decoder stayed on hardware
in all nine configurations:

| destinationImageBufferAttributes | hw | luma bytesPerRow | chroma offset | allocSize |
|---|---|---|---|---|
| baseline, IOSurface properties `{}` | yes | 384 | 92672 | 139264 |
| **no IOSurface properties at all** | yes | 384 | 92672 | 139264 |
| `BytesPerRowAlignment` 16 | yes | 384 | 92672 | 139264 |
| `BytesPerRowAlignment` 64 | yes | 384 | 92672 | 139264 |
| `BytesPerRowAlignment` 256 | yes | 512 | 123392 | 185536 |
| `BytesPerRowAlignment` 4096 | yes | 4096 | 987136 | 1483456 |
| `BytesPerRowAlignment` 16384 | yes | 16384 | 3948544 | 5931904 |
| `PlaneAlignment` 16384 | yes | 384 | 114688 | 161344 |
| `PoolMinimumBufferCount` 32 | yes | 384 | 92672 | 139264 |

Three things fall out of that table:

- **VideoToolbox's output is always IOSurface-backed**, whether or not
  `kCVPixelBufferIOSurfacePropertiesKey` is present. The row that omits it is
  byte-identical to the baseline. We do not have to ask.
- **NV12 comes back as one IOSurface with two planes at distinct offsets** — exactly
  the "one object, two layers" shape the design wants for the export descriptor. The
  two-objects-of-4096 the guest reports today really is an artefact of per-plane
  resources.
- **Row and plane alignment are both dictatable, and neither costs anything.** Wall
  time across all nine runs sat in a 41.5–46.4 ms band with no ordering by
  configuration, so nothing is being converted behind our backs, and
  `UsingHardwareAcceleratedVideoDecoder` stayed true throughout. (Both checks are here
  because either failure mode would have read as a pass. The hardware check is the
  lesson from virglrenderer `d7dd10aa`.)

**Alignment is not free in memory, though, and the cost is per row.** Asking for a
16384-byte row on a 352x240 frame took the surface from 136 KiB to 5.8 MiB. At 4K the
same request would mean ~53 MB per surface against ~12 MB tight. Ask for the alignment
the layout actually needs, never for the host page size by reflex.

## The pool is not a fixed set — the "map the pool once" alternative is dead

If VideoToolbox recycled a bounded set of surfaces, we could map all of them into the
guest once and never copy. It does not:

```
release in callback:   107 outputs, 5 distinct IOSurface ids     (31 45 57 59 57 59 57 59 ...)
hold every output:     107 outputs, 107 distinct IOSurface ids   (31 45 57 59 78 79 163 165 ...)
```

Five surfaces is a steady state that exists only because the consumer hands each buffer
straight back. Hold them and VideoToolbox mints a fresh one every frame; the pool grows
on demand rather than blocking or dropping. `kCVPixelBufferPoolMinimumBufferCountKey`
changed nothing — it is a pool-level key being passed in a buffer-attributes dictionary,
so being ignored is unsurprising.

This matters because a decode target is *exactly* the held case: the guest keeps
reference frames alive in its DPB for as long as later frames predict from them. Mapping
"the pool" would mean mapping an unbounded, ever-changing set.

The other half of the same problem is ordering, and no measurement can fix it: VA-API
names the render target in `vaBeginPicture` *before* the frame is decoded, while
VideoToolbox chooses its output surface itself and only reveals it afterwards. Even a
bounded pool could not be bound to guest targets ahead of time.

## What the copy costs

NV12, two planes, IOSurface to IOSurface, 50 repetitions:

| | best | mean |
|---|---|---|
| 1080p, matching pitch (one memcpy per plane) | 0.088 ms | 0.096 ms |
| 1080p, mismatched pitch (row by row) | 0.071 ms | 0.077 ms |
| 4K, matching pitch | 0.399 ms | 0.420 ms |
| 4K, mismatched pitch (row by row) | 0.366 ms | 0.402 ms |

Around 32–44 GB/s, i.e. 0.6% of a 60 fps frame at 1080p and 2.5% at 4K.

**Matching the pitches is not worth engineering for.** The row-by-row copy measured
*faster* than the single whole-plane memcpy here, and on an earlier run it measured
slower; the two shapes are indistinguishable at this scale and the ordering is noise.
Both copy the same bytes, and bandwidth is the only thing that matters. So the design
should let the guest choose whatever layout suits it and not contort the host's request
to make the pitches agree.

These numbers reuse one warm pair of surfaces, so treat them as a floor. They are also
not new cost: today the host CPU-maps every decoded plane and uploads it with
`glTexSubImage2D` (`vrend_video.c:156`, `upload_mapped_plane`), and the guest then pays
a host-to-guest transfer to get pixels into its own memory. One copy into memory the
guest already maps replaces both.

## IOSurface takes the guest's layout as given

The design has the guest compute the layout and the host allocate to match or refuse.
IOSurface allocates exactly what it is asked for, with **no rounding in any case
tested** — including tight pitches at odd widths and odd heights:

```
IOSurfaceGetPropertyAlignment(kIOSurfaceBytesPerRow)      = 128
IOSurfaceGetPropertyAlignment(kIOSurfacePlaneBytesPerRow) = 128
IOSurfaceGetPropertyAlignment(kIOSurfacePlaneOffset)      = 16384
IOSurfaceGetPropertyAlignment(kIOSurfaceAllocSize)        = 1

854x480   pitch 854   -> luma 854   (exact), chroma 854,   offset 409920
1920x1080 pitch 1920  -> luma 1920  (exact), chroma 1920,  offset 2073600
1921x1080 pitch 1921  -> luma 1921  (exact), chroma 1922,  offset 2074680
1920x1081 pitch 1920  -> luma 1920  (exact), chroma 1920,  offset 2075520
1921x1081 pitch 1921  -> luma 1921  (exact), chroma 1922,  offset 2076601
3840x2160 pitch 3840  -> luma 3840  (exact), chroma 3840,  offset 8294400
3840x2160 pitch 16384 -> luma 16384 (exact), chroma 16384, offset 35389440
```

Those `GetPropertyAlignment` values are advisory when explicit `kIOSurfacePlaneInfo` is
supplied: a 128-byte row alignment is reported and a 854-byte pitch is accepted; a
16384-byte plane-offset alignment is reported and an offset of 2073600 is accepted. Read
them as a hint about what the hardware prefers, not as a constraint the allocator
enforces. This folds in spike 2 of the design.

**A rule the arithmetic here has to obey:** chroma needs `ceil(w/2)*2` bytes per row, so
for an odd width it is *wider* than a luma pitch of exactly `w`. Sizing the chroma plane
by inheriting luma's pitch makes `IOSurfaceCreate` refuse every odd width — which is what
the first version of this probe did, and it looked like an IOSurface restriction on odd
widths until the chroma plane was sized in its own right. Whatever computes the layout,
guest or host, has to size each plane independently.

## What this settles, and what it does not

Settled: the copy stays, it is cheap, VideoToolbox gives us the layout and the
one-surface-two-planes shape the export descriptor needs, and IOSurface will allocate
whatever the guest asks for.

Not touched here: whether an IOSurface's base address can be mapped into a guest at all.
That is the design's remaining branching spike, it needs the hypervisor entitlement and a
codesigned binary, and nothing in this spike bears on it. If it fails, the target's
storage becomes an ordinary host-visible blob and the copy lands there instead — the
frame-cost numbers above are unchanged, but zero-copy present would need rethinking.
