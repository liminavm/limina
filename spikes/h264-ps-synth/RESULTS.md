# Synthesized H.264 parameter sets decode bit-exactly

The VideoToolbox backend has to write real SPS/PPS bytes from the parsed picture parameters
the guest sends (`docs/design/h264-hevc-decode.md`). This spike is the oracle for that
serializer — `third_party/virglrenderer/src/vrend/virgl_video_h264_ps.c` — and it exists
because a malformed parameter set does not announce itself: VideoToolbox either rejects the
format description outright, or accepts it and decodes subtly wrong.

## The method

`./verify.sh <clip.264> <width> <height>` reads a stream's **own** SPS/PPS field values with
`ffmpeg -bsf:v trace_headers`, feeds only those to the serializer, splices the synthesized
sets in place of the stream's real ones, and decodes both:

```
ffmpeg -i clip.264 -c copy -bsf:v "filter_units=remove_types=7|8" slices.264   # strip SPS/PPS
cat ps.bin slices.264 > ours.264                                              # ours instead
```

H.264 is normatively exact, so `framemd5` agreement is a verdict rather than a smell test.

**What is deliberately not handed over** is the point of the whole exercise:
`pic_width_in_mbs_minus1`, `pic_height_in_map_units_minus1` and every `frame_crop_*` value.
The serializer derives those from the display size; passing them in would test nothing.

## Results — all bit-exact

| stream | encoder | exercises | frames |
| --- | --- | --- | --- |
| `ref.264` 640×480 | x264 High | CABAC, B-frames, weighted prediction, 8×8 transform | 60 ✓ |
| `vt1080.264` 1920×1080 | **VideoToolbox hardware** | **cropping** (1080 → 68 MB rows, crop 4), a second encoder | 30 ✓ |
| `base480.264` 640×480 | x264 Baseline | **`profile_idc` 66 → the chroma block must be absent**, `pic_order_cnt_type` 2, CABAC off | 30 ✓ |
| `main854.264` 854×482 | x264 Main | cropping on **both** axes (crop right 5, bottom 7) | 30 ✓ |

The last three are what a single x264 High clip could never have covered. Using the M1's own
hardware encoder for one of them matters beyond convenience: it is a genuinely independent
SPS writer, so agreeing with it is not agreeing with x264's conventions.

`check.c` covers the other two entry points:

```
slice pic_parameter_set_id = 0        # parsed back out of the first slice header
annexb 22882 -> avcc 22885, 65 NALs, exact fit
undersized output refused
```

## The harness bug worth remembering

The first matrix run reported two of four streams failing, with `ref.264` — which had passed
minutes earlier with hand-entered values — among them. The serializer was fine. `synth.c`
had `MAX_KEYS 64` and **silently skipped** every key past the limit; the SPS and its VUI use
~45, so the PPS keys were dropped and `get()` returned defaults. The synthesized PPS shrank
from 6 bytes to 4 and decoding diverged.

A truncating loader in the *test* is indistinguishable from a broken serializer, and the
instinct was to go looking in the serializer. The tell was that a previously passing case
regressed when only the harness had changed. The limit is now loud (`exit(1)`), which is what
it should have been from the start: a test fixture that silently discards its input can only
ever produce false verdicts.

## What this does not cover

- **Custom scaling matrices.** The serializer refuses them. The wire lists come from VA-API's
  `VAIQMatrixBufferH264` and this spike has not established which scan order those are in;
  emitting them wrongly decodes with subtly wrong dequantization and looks like a driver bug
  elsewhere. Refusing is loud, guessing is not.
- **Interlaced, 4:2:2/4:4:4, >8-bit** — all refused by the serializer, none tested.
- **`pic_order_cnt_type == 1`**, which no encoder here emits by default. Types 0 and 2 are
  covered.
- **HEVC**, which additionally needs a VPS synthesized from nothing.

## Reproducing

```
cc -O1 -Wall -Wextra -I shim -I <virgl>/src/vrend -I <virgl>/src -I <virgl>/src/gallium/include \
   synth.c <virgl>/src/vrend/virgl_video_h264_ps.c -o synth
./verify.sh ref.264 640 480
./verify.sh vt1080.264 1920 1080
./verify.sh base480.264 640 480
./verify.sh main854.264 854 482
```

`shim/virgl_util.h` stands in for the real header, which pulls in meson-generated config the
serializer does not need. The clips regenerate with the `ffmpeg -f lavfi -i testsrc` lines in
this file's history; `vt1080.264` needs `-c:v h264_videotoolbox`.
