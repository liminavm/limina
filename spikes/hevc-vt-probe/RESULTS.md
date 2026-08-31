# What VideoToolbox needs from a synthesized HEVC SPS

HEVC decode on the VideoToolbox backend has to synthesize a VPS, SPS and PPS from the
VA-API wire (`docs/design/h264-hevc-decode.md`), and one part of the SPS has no source at
all: the `short_term_ref_pic_set()` structures. `num_short_term_ref_pic_sets` is on the wire
(`picture_hevc.c:110`); the sets themselves are not, and cannot be — VA-API hands drivers
resolved reference lists instead, which is why mesa's own slice parser *skips* the RPS
(`picture_hevc.c:374-378`) rather than reading it.

Slice headers index into those sets. So the question this spike exists to answer is whether
a backend that cannot reproduce them can decode anything at all — asked against real
VideoToolbox on the real hardware decoder, with no guest, no serializer, and no
virglrenderer code.

## The answer: the sets are never read, because slices carry their own

| stream | encoder | `num_short_term_ref_pic_sets` | slices with `short_term_ref_pic_set_sps_flag = 1` |
| --- | --- | --- | --- |
| `x265.265`, `x265-1080.265` | x265 | 0 | 0 of 59 |
| `vt1080.265` | VideoToolbox | 4 | 0 of 55 |

Both encoders emit the reference picture set **inline in every slice header**, with
`inter_ref_pic_set_prediction_flag = 0`. An inline set is self-contained, and slice bytes
pass through the backend untouched, so the SPS sets are decorative for these streams.

**Test H — the one that matters.** Rewrite each SPS to keep its set *count* but make every
set empty, and over-declare the level, then build the format description from those bytes at
session creation. This is exactly what the serializer will emit:

```
st_ref_pic_set(0)   = ue(0) ue(0)          2 bits
st_ref_pic_set(i>0) = 0, ue(0) ue(0)       3 bits
```

```
vt1080     BIT-EXACT vs the real parameter sets, 60 frames, hardware accelerated
x265       BIT-EXACT, 60 frames
x265-1080  BIT-EXACT, 60 frames
```

The leading `0` for `i > 0` is `inter_ref_pic_set_prediction_flag`, which is present for
every set except the first. Omitting it desyncs the parse of the SPS itself.

## Which format-description changes a live session survives

Measured with `VTDecompressionSessionCanAcceptFormatDescription` plus a pixel comparison
across the swap — the call's answer alone is not evidence the reference buffer survived.

| SPS change, mid-stream | CanAccept | outcome |
| --- | --- | --- |
| `sps_max_dec_pic_buffering` and CTB sizes (a different encode) | YES | decodes on, pixels unchanged |
| one `used_by_curr_pic` flag, then all ten | YES | decodes on, pixels unchanged |
| `general_level_idc` 90 → 120 (both defined levels) | **NO** | every later frame fails |
| `num_short_term_ref_pic_sets` 4 → 0 | **NO** | `-12916`, every later frame fails |

`-12916` is `kVTFormatDescriptionChangeNotSupportedErr`. The rule to carry into
`ensure_session`: **a level change is refused**, so the backend must pick one generous level
and never vary it — the level is not on the wire anyway, so there is nothing to vary it with.

## Two results that look like evidence and are not

- **The used_by_curr flips prove nothing about whether VideoToolbox consults set contents.**
  Every slice in those clips inlines its own set, so nothing ever read the bytes that changed.
  Invariance under a differential that never reaches the system under test is not an
  exoneration. The question stays open, and stays moot: the streams where it would matter are
  the ones the backend refuses.
- **The first `general_level_idc` probe edited the wrong byte** — `rbsp[12]` rather than
  `rbsp[14]`, landing inside the 44 reserved bits — and reported `level_idc 0 -> 3` while the
  SPS *shrank* by a byte, because the edit removed an emulation-prevention byte. It returned
  a confident YES. The tell was in the output the whole time: a plausible field value and a
  length that did not change are both worth checking before believing the verdict.

## What the backend must refuse, and why it cannot refuse early

The refusals are not hardening; they are the conditions under which the placeholder sets are
sound. Each needs the slice header parsed to the flag, which is why the parse is part of the
first cut rather than a later addition:

- `short_term_ref_pic_set_sps_flag = 1` — the slice would read an empty placeholder and
  decode silently wrong.
- inline RPS with `inter_ref_pic_set_prediction_flag = 1` — the *bit layout* of the inline set
  depends on the referenced set's delta count, so a placeholder desyncs the parse.
- `num_long_term_ref_pics_sps > 0` — SPS long-term entries are missing from the wire exactly
  as the short-term sets are. Slice-carried long-term references are self-contained and fine.

None of these are visible at codec-creation time: RPS policy first shows up in the first
inter-predicted slice. The refusal therefore lands one frame into playback, and that is not a
defect to be "fixed" into a create-time check, which cannot exist.

## Reproducing

```
clang -O1 -Wall -Wno-deprecated-declarations -o probe probe.c \
  -framework CoreFoundation -framework CoreMedia -framework VideoToolbox -framework CoreVideo
./probe <clip.265>                        # baseline digests
./probe <clip.265> <n> [alt.265|alt.bin]  # swap a format description in before frame n
./probe <clip.265> create <alt.bin>       # build the session from alt's SPS instead

python3 sps_placeholder.py <clip.265> ph.bin   # the SPS the serializer will emit
python3 rps_edit.py <clip.265> edited.bin      # flip every used_by_curr flag, same bit length
```

Clips regenerate with `ffmpeg -f lavfi -i testsrc2` and `-c:v libx265` / `-c:v
hevc_videotoolbox`. `probe` groups slice NALs into access units on
`first_slice_segment_in_pic_flag` and digests every returned plane.
