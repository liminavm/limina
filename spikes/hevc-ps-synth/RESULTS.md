# Synthesized HEVC parameter sets decode bit-exactly

The VideoToolbox backend must write a real VPS, SPS and PPS from the picture parameters the
guest sends (`docs/design/h264-hevc-decode.md`). This spike is the oracle for that
serializer — `third_party/virglrenderer/src/vrend/virgl_video_h265_ps.c` — and is the direct
sibling of `spikes/h264-ps-synth`. Read `spikes/hevc-vt-probe` first: it establishes *why*
the reference picture sets can be placeholders, which is the premise everything here rests on.

## The method

`./verify.sh <clip.265> <width> <height>` reads the stream's **own** VPS/SPS/PPS field values
with `ffmpeg -bsf:v trace_headers`, feeds only those to the serializer, splices the
synthesized sets in place of the stream's real ones, and decodes both:

```
ffmpeg -i clip.265 -c copy -bsf:v "filter_units=remove_types=32|33|34" slices.265
cat ps.bin slices.265 > ours.265
```

**Deliberately not handed over**, because they are exactly what the backend has to invent:
the entire VPS, `general_level_idc`, the `conf_win_*` offsets, and the contents of every
`short_term_ref_pic_set`.

## Results — all bit-exact

| stream | encoder | exercises | frames |
| --- | --- | --- | --- |
| `x265.265` 640×480 | x265 | `num_short_term_ref_pic_sets = 0`, no conformance window | 60 ✓ |
| `x265-1080.265` 1920×1080 | x265 | a second size through the same shape | 60 ✓ |
| `vt1080.265` 1920×1080 | **VideoToolbox** | **4 placeholder ref pic sets**, a **conformance window** (coded 1088 → displayed 1080), **default scaling lists**, an independent parameter-set writer | 60 ✓ |
| `odd854.265` 854×482 | x265 | cropping on **both** axes (coded 856×488), 4 references, 4 B-frames | 60 ✓ |
| `vtodd.265` 1278×718 | VideoToolbox | cropping on both axes **with** placeholder ref pic sets | 60 ✓ |

The clips live in `../hevc-vt-probe/`, where they are also the probe's fixtures.

## Two harness bugs, both of the same family

Neither was a serializer fault, and both presented as one.

**Subscripted names were silently dropped.** `trace_headers` prints
`sps_max_dec_pic_buffering_minus1[0]`, and the extractor's name pattern rejected the
brackets, so the DPB size never reached the serializer and `get()` returned its default of 1.
`x265.265` then matched on frames 0–3 and diverged from frame 4 — precisely where a
one-picture buffer runs out. A dropped field does not look like a dropped field; it looks
like a plausible value, because the default *is* plausible. The extractor now keeps index 0
explicitly and skips higher indices rather than letting the pattern reject the line.

**The harness left the scaling lists zeroed.** The serializer refused `vt1080.265` for
carrying "custom" lists. It was right to, given what it was handed: mesa delivers the
*effective* lists whether or not the stream carried any, so a stream that merely enables the
defaults still arrives with all 6×16 4×4 entries at 16 — and the harness was sending zeroes.
Modelling the wire means filling in the defaults, exactly as
`spikes/h264-ps-synth` had to be corrected to feed the picture descriptor rather than the
SPS/PPS members.

Both are the same lesson in different clothes: **when a spike and the code disagree, suspect
the spike's model of the wire first.** In this spike's short life it has produced two
confident false failures and zero true ones.

## What this does not cover

- **Custom scaling lists.** The serializer emits `sps_scaling_list_data_present_flag = 0`
  and so selects the defaults, which is exact when the stream did the same and silently wrong
  otherwise. It distinguishes the two by comparing the wire's lists against the defaults **as
  sorted multisets**, because the scan order VA-API delivers them in is not established — a
  multiset comparison does not depend on it. A custom list that is a permutation of a default
  would pass; nothing plausible produces one. No clip here carries custom lists, so the
  refusal path is unverified.
- **Streams whose slices index an SPS ref pic set**, or inline one predicted from an SPS set.
  Both are refused by `virgl_h265_slice_inspect`; no encoder available here produces either,
  so the detector is unexercised. A JCT-VC `RPS_*` conformance vector would prove it fires.
- **Main 10, 4:2:2/4:4:4, separate colour planes, tiles, PCM** — refused or untested.
- **The slice inspector's parse** beyond the first independent slice segment: later segments
  need the CTB address width, and are skipped because the picture's first slice has already
  answered every question.

## Reproducing

```
cc -O1 -Wall -Wextra -I shim -I <virgl>/src/vrend -I <virgl>/src -I <virgl>/src/gallium/include \
   synth.c <virgl>/src/vrend/virgl_video_h265_ps.c -o synth
./verify.sh ../hevc-vt-probe/x265.265      640  480
./verify.sh ../hevc-vt-probe/x265-1080.265 1920 1080
./verify.sh ../hevc-vt-probe/vt1080.265    1920 1080
./verify.sh ../hevc-vt-probe/odd854.265     854  482
./verify.sh ../hevc-vt-probe/vtodd.265     1278  718
```

## End-to-end verdict

Decoded in the guest through VA-API against the software decoder, `framemd5` per frame:

```
x265       PASS - 60 frames, hardware == software, bit-exact
x265-1080  PASS - 60 frames, bit-exact
vt1080     PASS - 60 frames, bit-exact
odd854     PASS - 60 frames, bit-exact
vtodd      PASS - 60 frames, bit-exact
```

Every session reported `hardware accelerated: yes`; no refusal fired, no decode failed, and
the synthesized sets stayed byte-identical for the life of each stream. Unlike H.264, whose
PPS changes mid-GOP because its reference counts arrive per-slice, HEVC's
`num_ref_idx_l*_default_active_minus1` really are the PPS defaults, so nothing churns.
