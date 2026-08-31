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

## What the spike could not catch, and what did

Both real bugs in the serializer survived a green matrix here, because a spike that feeds
fields from `trace_headers` chooses which struct member to write and can pick the wrong one:

- **The dead wire fields.** `sps.max_num_ref_frames` and
  `pps.num_ref_idx_l*_default_active_minus1` are **never written on mesa's decode path** --
  only `picture_h264_enc.c` sets them. The live values arrive at the top level of the picture
  descriptor: `desc->num_ref_frames` (from the VA picture parameter buffer) and
  `desc->num_ref_idx_l*_active_minus1` (from the slice parameter buffer). Reading the SPS/PPS
  members got zeros, so the SPS declared a 0-frame DPB and the reference lists held one entry;
  VideoToolbox answered `kVTVideoDecoderBadDataErr` (-12909) from the third frame on. `synth.c`
  now feeds the descriptor fields, so the harness models the wire rather than the spec layout.
- **The parameter sets are not constant.** `num_ref_idx_l*_active_minus1` is the *effective*
  per-slice count, so a slice that overrides the PPS default changes the PPS we synthesize by
  a byte mid-GOP. That is correct -- and it is invisible to a spike that synthesizes one set
  per stream. It broke decoding anyway, in the backend: the decompression session was keyed on
  the parameter-set bytes, so the change rebuilt the session and took the reference picture
  buffer with it. Every frame after the first override referenced an empty DPB. Decoding
  reported success and the pixels were wrong. The fix keys the session on frame shape alone and
  lets the bytes drive only the format description, swapped into the live session through
  `VTDecompressionSessionCanAcceptFormatDescription`.

The rule both bugs teach: **a serializer spike verifies the bytes, never the plumbing that
fills them.** Nothing here can pass or fail on which struct member mesa populates, or on how
the backend reacts to a value legitimately changing. Only a guest can.

## End-to-end verdict

Decoded in the guest through VA-API against the software decoder, `framemd5` per frame:

```
ref     PASS - 60 frames, hardware == software, bit-exact
vt1080  PASS - 30 frames, hardware == software, bit-exact
base480 PASS - 30 frames, hardware == software, bit-exact
main854 PASS - 30 frames, hardware == software, bit-exact
```

One decompression session per clip, 84 parameter-set changes absorbed without a rebuild, zero
fallbacks to the session-rebuild path.

## What this does not cover

- **Custom scaling matrices.** The serializer refuses them. The wire lists come from VA-API's
  `VAIQMatrixBufferH264` and this spike has not established which scan order those are in;
  emitting them wrongly decodes with subtly wrong dequantization and looks like a driver bug
  elsewhere. Refusing is loud, guessing is not.
- **Interlaced, 4:2:2/4:4:4, >8-bit** — all refused by the serializer, none tested.
- **`pic_order_cnt_type == 1`**, which no encoder here emits by default. Types 0 and 2 are
  covered.
- **HEVC**, which additionally needs a VPS synthesized from nothing.
- **A multi-slice frame whose first slice overrides the reference counts and whose later
  slices do not.** The PPS default we emit comes from the effective count of whichever slice
  we parsed, which is then not the true default for the non-overriding ones. No stream here
  does this; parsing `num_ref_idx_active_override_flag` out of the slice header is the fix if
  one ever does.

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
