# Radar: VideoToolbox returns the wrong pixels for AV1 super-resolution frames

Ready to file. Reproduces with stock ffmpeg on a stock clip and mentions no part
of this project, which is the point — the report should stand on its own.

- **Component:** VideoToolbox / AV1 hardware decode
- **Measured on:** macOS 26.5.2, Apple M4 Pro (also expected on any AV1-capable
  Apple silicon; not reproducible on M1/M2, which have no AV1 decoder)
- **Type:** Incorrect behaviour

## Summary

When decoding an AV1 stream that uses super-resolution (`use_superres = 1`),
VideoToolbox returns a picture that is not the decoded frame. The buffer comes
back at the frame's **coded** width rather than its upscaled width, and its
contents are approximately the rightmost `coded_width` columns of the correctly
upscaled picture — not the pre-upscale picture, which a client could have
upscaled itself.

The decoder's internal reconstruction is correct: frames that predict from
super-resolution reference frames come back bit-exact. Only the picture handed
to the client is wrong.

## Steps to reproduce

Any AV1 clip encoded with super-resolution enabled. With `ffmpeg`, decode the
same file twice — once through VideoToolbox, once through libdav1d — and compare
the per-frame mean luma:

```
ffmpeg -hwaccel videotoolbox -i superres.mp4 -pix_fmt gray -f image2 vt/%03d.pgm
ffmpeg -c:v libdav1d          -i superres.mp4 -pix_fmt gray -f image2 sw/%03d.pgm
```

To produce such a clip with SVT-AV1:

```
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30:duration=2 \
       -c:v libsvtav1 -svtav1-params superres-mode=2 superres.mp4
```

## Expected

Both decoders return every frame at the stream's upscaled width (640) with
matching content. AV1's super-resolution upscale is part of the **decoding**
process (§7.16 of the AV1 spec), not a display-time operation, so a conformant
decoder must apply it before output.

## Actual

libdav1d's per-frame mean luma is flat across the clip — **127.00 to 127.43**.
VideoToolbox's swings **110.44 to 142.76**, deviating exactly on the frames that
use super-resolution, and more the more aggressive the scaling.

Decoding the same stream frame-by-frame through `VTDecompressionSession`
directly shows the geometry:

- picture sizes returned are `640 * 8 / superres_denom` — the coded width, e.g.
  `465x360`, `320x360` — and only frames with `use_superres = 0` come back at 640
- against the correctly decoded frame, the returned pixels match:
  - the pre-upscale picture on **6.9%** of pixels (mean abs diff 65)
  - the rightmost `coded_width` columns of the upscaled picture on **76.3%**
    (mean abs diff 5.6)
- a stride/base search over the correct frame finds stride 640 / base 320 — the
  plain right-hand crop — as the unique best fit
- the row pitch leaves room past the declared width, and that region is a flat
  fill of the edge byte, so no wider content is being hidden by a short width

Passing `kCVPixelBufferWidthKey`/`kCVPixelBufferHeightKey` at the stream's own
size to `VTDecompressionSessionCreate` returns 640-wide buffers holding the same
wrong pixels, stretched: the mean is unchanged to four decimals (111.1663 vs
111.1662), while non-super-resolution frames stay bit-exact.

## Impact

Any client decoding super-resolution AV1 through VideoToolbox displays visibly
wrong pixels, with no error reported anywhere. Super-resolution is rare in
practice — it is an encoder opt-in, off by default in aom and SVT-AV1 — which is
likely why this has gone unreported, but it also means a client meeting it has no
signal that anything went wrong.
