# Synthesized H.264 parameter sets decode bit-exactly

The VideoToolbox backend has to write real SPS/PPS bytes from the parsed picture parameters
the guest sends (`docs/design/h264-hevc-decode.md`). This spike is the oracle for that
serializer — `third_party/virglrenderer/src/vrend/virgl_video_h264_ps.c` — and it exists
because a malformed parameter set does not announce itself: VideoToolbox either rejects the
format description outright, or accepts it and decodes subtly wrong.

## The test

`ref.264` is a 640×480 60-frame **High profile** clip (x264, `-bf 2`), so it exercises CABAC,
B-frames, weighted prediction and the 8×8 transform rather than a trivial baseline path.
Its own SPS/PPS field values were read with `ffmpeg -bsf:v trace_headers` and hand-entered
into `synth.c` as the values mesa would put on the wire — the serializer never sees the
original bytes.

Then, the substitution:

```
ffmpeg -i ref.264 -c copy -bsf:v "filter_units=remove_types=7|8" slices.264   # strip SPS/PPS
cat ps.bin slices.264 > ours.264                                             # ours instead
```

## Result

```
sps 10 bytes, pps 6 bytes
ref frames: 60  ours frames: 60
IDENTICAL — every frame bit-exact
```

`framemd5` agrees on all 60 frames. Since H.264 is normatively exact, that is a real verdict
and not a smell test: our synthesized parameter sets configure the decoder identically to the
encoder's own.

`check.c` covers the other two entry points against the same stream:

```
slice pic_parameter_set_id = 0        # parsed back out of the first slice header
annexb 22882 -> avcc 22885, 65 NALs, exact fit
undersized output refused
```

The Annex-B → AVCC walk verifies every 4-byte length lands exactly on the next NAL, and that
a one-byte-short output buffer is refused rather than truncated.

## What this does not cover

- **Custom scaling matrices.** The serializer refuses them today. The wire lists come from
  VA-API's `VAIQMatrixBufferH264` and this spike has not established which scan order those
  are in; emitting them wrongly decodes with subtly wrong dequantization and looks like a
  driver bug elsewhere. Refusing is loud, guessing is not. Extending the spike with a
  `--tune` encode that produces a non-flat matrix is what would settle it.
- **Interlaced, 4:2:2/4:4:4, >8-bit** — all refused by the serializer, none tested.
- **HEVC**, which additionally needs a VPS synthesized from nothing.
- The clip is one encoder's output. x264 is representative but not exhaustive; a hardware
  encoder's SPS (different `pic_order_cnt_type`, cropping, `pic_order_cnt_type == 1`) would
  widen the coverage cheaply.

## Reproducing

```
cc -O1 -Wall -Wextra -I shim -I <virgl>/src/vrend -I <virgl>/src -I <virgl>/src/gallium/include \
   synth.c <virgl>/src/vrend/virgl_video_h264_ps.c -o synth
./synth ps.bin && ffmpeg -v error -i ref.264 -c copy -bsf:v "filter_units=remove_types=7|8" -f h264 -y slices.264
cat ps.bin slices.264 > ours.264
ffmpeg -v error -i ref.264 -f framemd5 -y ref.md5 && ffmpeg -v error -i ours.264 -f framemd5 -y ours.md5
diff ref.md5 ours.md5
```

`shim/virgl_util.h` stands in for the real header, which pulls in meson-generated config the
serializer does not need.
