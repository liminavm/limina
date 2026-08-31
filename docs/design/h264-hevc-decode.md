# H.264 / HEVC hardware decode: synthesizing the parameter sets

Third and fourth codecs after VP9 (`docs/graphics.md` §4.5) and AV1
(`docs/design/av1-decode.md`). Both are decoded in hardware by every Apple silicon Mac we
support, and the whole host-side session machinery in `virgl_video_vt.c` already works. The
work is neither the session nor the transport: it is **rebuilding the parameter sets the
guest never sends**, and deciding who is allowed to ask.

## Unlike VP9 and AV1, this one is enhanced-tier by construction

VP9 and AV1 are reachable from a stock Fedora guest. H.264 and HEVC are not, and the gate is
not ours:

```c
/* src/gallium/auxiliary/vl/vl_codec.c:65-81 */
if (profile >= PIPE_VIDEO_PROFILE_MPEG4_AVC_BASELINE &&
    profile <= PIPE_VIDEO_PROFILE_MPEG4_AVC_HIGH444) {
   ...  } else if (!VIDEO_CODEC_H264DEC) { return false; }
```

`VIDEO_CODEC_H264DEC` / `H265DEC` are compile-time constants from
`-Dvideo-codecs` (`meson.options:718`, default `['all_free']`), and the check runs **before**
`screen->get_video_param` is consulted. So the driver never gets asked. A host that advertises
H.264 to a stock guest is advertising into a void — no guest-visible change, no regression,
just nothing.

Consequences to accept up front:

- **The two-tier guarantee holds trivially, but the feature is one-sided.** A stock guest
  keeps software decode; an enhanced guest gets hardware. Nothing degrades.
- **We already own this dial.** Our enhanced mesa is built from Fedora's SRPM, so enabling
  `h264dec,h265dec` is a spec change in `scripts/provision/f44/build-mesa-rpm.sh`, not a patch.
- **Stock users have their own route** — RPM Fusion's `mesa-va-drivers-freeworld`, which libva
  already probes ahead of ours (`/usr/lib64/dri-freeworld/` appears in every guest's driver
  search). A guest with freeworld installed reaches our host backend with no help from us,
  which makes it a useful second test vehicle.

**Our position on the patent question.** Fedora excludes these codecs for patent reasons; we
enable them, in both tiers, deliberately. What is enabled is *plumbing*, not an
implementation: the mesa side is a frontend that forwards parsed parameters, and the decode is
performed by Apple's already-licensed VideoToolbox on Apple's silicon. Nothing we ship
contains a codec.

Concretely:

- **Enhanced tier**: `build-mesa-rpm.sh` injects `-Dvideo-codecs=all_free,h264dec,h265dec`
  into the spec's `%meson` block. The value is additive by design — meson's `all_free` branch
  does `_codecs += free_codecs`, so naming the two decoders keeps every free codec and pulls in
  **no encoders**. Fedora passes no `-Dvideo-codecs` at all, so the injection has exactly one
  site to find, and the script fails loudly if it misses.
- **Stock tier**: `mesa-va-drivers-freeworld` goes onto the F44 `accessible` and `stock.test`
  images via `scripts/provision/install-freeworld-va.sh`. `vanilla.raw` is left pristine on
  purpose, so there is still one untouched Fedora reference to compare against. **This is what
  "stock" means for us from here on** — including for the L2 stock-tier tests, which will boot
  an image that advertises the codecs.

### On the stock tier, validate with Chrome or GStreamer — not Firefox

**We do not ship patched mesa to stock images**; that is what makes them stock. So a stock
guest runs vanilla mesa — RPM Fusion's freeworld build included — and therefore does *not*
carry the one-line fix that stopped virgl offering YV12/IYUV as decode targets. libva also
probes `/usr/lib64/dri-freeworld/` **ahead of** `/usr/lib64/dri/` (measured on a live guest),
so on a freeworld image the vanilla driver is the one that loads even where ours is present.

That is a constraint on the *validation vehicle*, not on the feature:

- **Firefox is the one that trips**, because it decodes through ffmpeg, whose
  `vaapi_decode_find_best_format` resolves the 8-bit 4:2:0 tie to the last exact pix-fmt match
  and lands on IYUV. Its rejection (`Unsupported VA-API surface format`) is specific to that
  path, and so is our fix.
- **Chrome and GStreamer select their own decode-target format** and pick NV12 without help.
  Chrome was observed taking the hardware path on a stock guest with unpatched mesa. They are
  the stock-tier oracles.

So do not read a red Firefox result on a stock image as a broken backend — check what the
driver offered before concluding anything. Upstreaming the virgl change
(`docs/hardening-backlog.md`) is still owed and is what would eventually give stock-tier
Firefox the hardware path, but it gates one browser, not the tier.

## What already exists, and it is more than for AV1

- **The wire carries the parameters.** `virgl_h264_picture_desc` and
  `virgl_h265_picture_desc` are already defined in `src/virgl_video_hw.h` (lines 123 and 364)
  with SPS and PPS embedded. No protocol change, no manifest-visible wire bump.
- **The slice data arrives as real bitstream, in Annex-B.** mesa's VA frontend prepends a
  three-byte start code before every H.264/H.265 slice NAL
  (`src/gallium/frontends/va/decode.c:204-227`). So unlike AV1 — where ffmpeg destroys the
  frame header at the VA boundary — the *slice* layer survives intact. We never synthesize a
  slice header.
- **Decode-order delivery is already the backend's shape.** `DecodeFrame` is called with flags
  `0` (`virgl_video_vt.c:969`), so no temporal processing and no asynchrony: the callback fires
  before the call returns, in decode order. That is exactly what VA-API semantics need, and it
  is what makes B-frames a non-issue — the guest owns its DPB and its reordering, we hand back
  each picture as it is decoded. **Do not add `kVTDecodeFrame_EnableTemporalProcessing`** to
  "fix" ordering; it would introduce the reordering delay the guest is not expecting.

So the job reduces to one function per codec: parsed parameters in, a conformant parameter-set
NAL out.

## The real work: parameter-set synthesis

VideoToolbox builds its format description from parameter sets —
`CMVideoFormatDescriptionCreateFromH264ParameterSets(sps, pps)` and
`...FromHEVCParameterSets(vps, sps, pps)`. The guest sends us the *semantic content* of those
sets, never their bytes. We must write the bytes.

### The id-matching problem, and why it is not one

The synthesized PPS must carry the `pic_parameter_set_id` that the guest's **real slice
headers** reference — and the wire struct carries no ids at all. This looks like a blocker and
is not:

- `pic_parameter_set_id` is the second `ue(v)` in an H.264 slice header (after
  `first_mb_in_slice` and `slice_type`), and `slice_pic_parameter_set_id` is likewise near the
  front of an HEVC slice segment header. We hold the slice bytes. **Parse the id out of the
  first slice and emit a PPS bearing it.**
- The PPS→SPS link (`seq_parameter_set_id`) and, for HEVC, the SPS→VPS link are *internal to
  the sets we are writing*. Nobody else references them. Pick `0` and be consistent.

That is the whole of it: one id is observed, the rest are ours to choose.

### H.264: what the wire omits, and where each missing field comes from

`struct virgl_h264_sps` is the gallium hardware-decoder shape — everything a fixed-function
decoder needs, nothing a serializer needs. Missing, with its source:

| Missing from the wire | Where it comes from |
| --- | --- |
| `profile_idc` | derived from the codec's `pipe_video_profile` (BASELINE→66, MAIN→77, HIGH→100) |
| `constraint_set*_flags` | zero; they only ever *narrow* a profile |
| `seq_parameter_set_id`, `pic_parameter_set_id` | ours / parsed from the slice (above) |
| `pic_width_in_mbs_minus1`, `pic_height_in_map_units_minus1` | the codec's width/height, rounded up: `(w + 15) / 16 - 1` |
| `frame_cropping_*` | the remainder of that rounding, so VT emits the display size and not the padded one |
| `gaps_in_frame_num_value_allowed_flag` | zero |
| VUI | omitted entirely; `vui_parameters_present_flag = 0` |

Note the asymmetry worth remembering: **H.264 has no dimensions on the wire and HEVC does**
(`pic_width_in_luma_samples` / `pic_height_in_luma_samples`, `virgl_video_hw.h:266-267`). The
H.264 path must therefore take its geometry from the codec object, and cropping is not
optional — any width that is not a multiple of 16 decodes to the wrong visible size without it.

### HEVC: the VPS has no source at all

`CMVideoFormatDescriptionCreateFromHEVCParameterSets` requires a VPS, and nothing on the wire
describes one. It must be synthesized whole. This is less alarming than it sounds — the
load-bearing content of a VPS is `profile_tier_level` plus
`vps_max_dec_pic_buffering_minus1`, and the SPS-side equivalents are on the wire
(`sps_max_dec_pic_buffering_minus1`). The tier/level itself is not, so it is derived from the
profile the same way H.264's `profile_idc` is.

Write the VPS, SPS and PPS as one contiguous set, in that order, with ids 0/0/parsed.

### Sample buffers: Annex-B in, AVCC out

VideoToolbox wants length-prefixed NALs, and the guest gives us start-code-delimited ones. So
each submission is walked once: find the `00 00 01` boundaries, replace each with a four-byte
big-endian length, and set `nal_length_size = 4` in the format description. Emulation-prevention
bytes stay untouched — we are re-framing NALs, not re-writing them.

## Scope for the first cut

Progressive, 8-bit 4:2:0, single-layer:

- **H.264**: Baseline / Main / High, `frame_mbs_only_flag = 1`. Interlaced content
  (`field_pic_flag`, PAFF/MBAFF) is refused at `get_video_param` rather than half-supported.
- **HEVC**: Main and Main Still Picture. Main10 is a natural follow-on — VT decodes it and the
  P010 surface path already exists for VP9 profile 2 — but it is not this cut.
- No encode, no SVC/MVC, no scaling lists beyond what the wire carries (it carries both 4x4 and
  8x8, so this costs nothing).

## Verifying

**Both codecs are normatively exact**, like VP9 — the transforms are integer and specified.
So the oracle is the same and it is a strong one: hardware and software decode of the same
stream must be **byte-identical**, plus a not-uniform check, because every failure mode we have
seen in this stack produces a cleared surface that byte-equality alone would happily accept.

Test vehicles, in the order they should be reached for:

1. `l2_video_vaapi.rs`, extended — the existing harness already knows how to do the
   hardware-vs-software comparison.
2. The **freeworld guest** as an independent check that a non-enhanced path reaches the same
   backend.
3. The browser-level verdict, and **the vehicle differs by tier** (see above):
   - *Enhanced* — `spikes/vt-vp9-decode/guest-ff-vaapi-check.sh`. The FOURCC line it greps for
     is the same one that mattered for VP9, and ffmpeg's decode-target selection is
     codec-independent, so the `26.1.7-4` NV12 fix covers H.264/HEVC the day they are enabled.
   - *Stock* — Chrome (`chrome://gpu`, plus the media-internals decoder name) or a GStreamer
     pipeline (`vah264dec` / `vah265dec` appearing in `gst-inspect-1.0 va`, then a decode run).
     Both pick NV12 on their own, so they exercise the backend without needing our mesa.

A conformance suite is worth using here in a way it was not for VP9: the JCT-VC/JVT bitstreams
exercise parameter-set corners (cropping, scaling lists, long-term references) that a
hand-rolled clip never reaches, and every one of those corners lands in code we are writing
rather than in VideoToolbox.

## The trap this design is shaped around

For VP9 the backend was plumbing and the risk was elsewhere. Here, **we are writing bitstream
that a hardware decoder will parse**, and a malformed parameter set does not announce itself:
VideoToolbox will either reject the format description outright (loud, easy) or accept it and
decode subtly wrong — wrong size, wrong cropping, wrong reference behaviour — which reads
exactly like a driver bug several layers away. So: assert the synthesized sets against a
reference parser (`ffmpeg -v trace`, or `h264bitstream`) as part of the test, not by eye, and
do it before trusting any pixel comparison.
