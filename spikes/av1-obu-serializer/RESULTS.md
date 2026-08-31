# AV1 OBU serializer

Rebuilds an AV1 bitstream from the VA-API descriptors a guest submits, so a
bitstream-in decoder (VideoToolbox) can serve `vaEndPicture`. ffmpeg destroys the
frame header at the ffmpeg→VA-API boundary — `av1dec.c` passes only
`raw_tile_group->tile_data.data` — so the header has to be synthesized from the
descriptor and the tile payload passed through verbatim.

The serializer itself lives in the virglrenderer fork
(`src/vrend/virgl_video_av1_obu.{c,h}`); this directory holds the fixtures and the
oracle that grade it.

## Status

All six fixtures rebuild into a stream dav1d decodes **bit-identically** to the
original clip:

| fixture   | shown pictures compared | hidden frames |
|-----------|------------------------|---------------|
| baseline  | 31                     | 29            |
| filmgrain | 31                     | 29            |
| tiles     | 31                     | 29            |
| superres* | 31                     | 29            |
| pan       | 31                     | 29            |
| lowdelay  | 60                     | 0             |

\* the `superres` capture carries a local repair: two of its tile payloads were
recorded as zeros (see *Known defect* below) and were restored from the clip.

Hidden frames are not compared directly — the rebuilt stream never shows them, so
no decoder emits them. They are covered transitively: every shown picture is
predicted from them, so a hidden frame decoded wrongly appears as a pixel
difference in the frames that reference it.

**Global motion is still unexercised.** No fixture induces it, so
`write_global_motion_params()` is unverified. A clip that actually provokes warped
motion is still owed.

**Every fixture is SVT-AV1.** That is one encoder's idea of a GOP, and it is not the
one the stack meets in a browser: a libaom stream stores shown frames inside a pyramid
GOP, which no fixture does, and which the slot assignment gets wrong (below). Fixture
coverage is a claim about syntax, not about the streams users play.

## Reference slots are ours to assign, and the assignment must be exact

VA-API does not carry `refresh_frame_flags` — mesa writes a constant 1
(`picture_av1.c`) — because a VA driver never needs it: the application hands it
the whole reference map per frame and manages the DPB itself. A bitstream writer
does need it, so slots are assigned here and `ref_frame_idx` remapped onto them.

Assigning them round-robin is not merely inelegant, it is wrong. Once eight
distinct pictures are live — which happens in any pyramid GOP — storing a ninth
evicts one, and evicting one a later frame still references loses it. Which slot
the guest chose is information that exists **only in the next frame's reference
map**, so it cannot be derived when the frame arrives.

Hidden frames are therefore held for one submission and emitted once the next
descriptor settles the question. Only hidden frames are held: nothing waits on
their pixels until a later `show_existing`, whereas holding a shown frame would
stall a caller that reads its surface back before submitting the next one. Shown
frames go out immediately into a slot nothing live occupies — in a pyramid GOP
they are never stored at all, and a low-delay stream, where they are, never fills
eight slots.

What a frame stored is read as a **set difference between consecutive reference
maps**, never against our own slots and never per-slot. Surface ids are recycled
— a capture cycles through a handful of them — so a freshly reused id reads as one
we already hold and teaches nothing; that alone cost seven frames. A per-slot diff
has the mirror-image flaw: it misses a frame landing in a slot whose contents were
seen elsewhere. Pictures the guest stops listing are pruned for the same reason.

## A shown frame is stored too, and then there is no free slot

The slot assignment emits a shown frame immediately, into a slot nothing live occupies,
rather than holding it for the next descriptor the way a hidden frame is held. When all
eight slots are live there is no such slot, and the frame is emitted with
`refresh_frame_flags = 0` — stored nowhere. Every later reference to it then resolves to
whatever the slot still holds, usually the key frame.

This is what a libaom pyramid GOP does and SVT-AV1's does not. Measured over 124 frames
of a 720p YouTube stream against the six fixtures:

| stream    | frames | shown *and* stored |
|-----------|--------|--------------------|
| youtube   | 124    | 64                 |
| lowdelay  | 60     | 29                 |
| baseline, filmgrain, tiles, superres, pan | 60 each | 0 |

`lowdelay` stores shown frames but never fills eight slots, so it never reaches the
case. The five pyramid fixtures fill eight slots but never store a shown frame. The
combination — a pyramid GOP that also stores shown frames — is the one no fixture holds,
and it is the common case in the wild.

dav1d rejects the result at `dav1d_send_data` with `EINVAL` and logs
`Error parsing frame header`, which reads like a syntax bug and is not one: the header
parses field-for-field, and an independent parser (`fhparse.py`) accepts all of it. Only
the *resolution* of the references is wrong, so `dpb-check.py resolve` is the diagnostic
that shows it and a header diff is the one that hides it.

## The failure this produces is not where it happens

A dropped reference does not fail at the frame that dropped it. Every frame header
still parses field-for-field identical to its descriptor, and `cbs_av1` accepts the
whole stream. The loss surfaces many frames later inside
`dav1d_decode_tile_sbrow` as an entropy-decode desync, reported as a bare `EINVAL`
with no message.

So **"the header parses" proves nothing about slot state**, and the diagnostic that
does work is to simulate both DPB state machines — the original clip's and the
rebuilt stream's — and compare what each reference *resolves to*. The first
divergence is the bug; on `baseline` it was a reference resolving to order hint 47
where the encoder meant order hint 0, the key frame, still live and evicted by the
cursor.

## Sequence-header flags may differ from the encoder's

`separate_uv_delta_q`, `initial_display_delay_present_flag` and `enable_superres`
are derived differently from the encoders that produced these clips, so every
rebuilt frame header is one or two bits longer. This is harmless: each stream is
read with its own sequence header, and the fixtures decode bit-exact. A header-bit
count that differs from the original is **not** evidence of a defect.

## Known defect: the capture can record zeroed tile data

Two of the sixty `superres` fixtures were captured with the correct tile *size* but
all-zero *contents*, which reads exactly like a serializer bug — the stream fails
to decode with a header that is provably correct. Restoring those two payloads from
the clip makes `superres` pass bit-exact, so the serializer is not implicated.

The guest's slice buffer was therefore not visible to the host when
`av1_decode_bitstream` read it. This is intermittent (2 of 360 captured frames) and
must be resolved before the decode path is wired up, where it would corrupt frames
silently rather than merely spoiling a fixture.

## Layout

- `make-fixtures.sh` — encodes the six clips with SVT-AV1.
- `capture/<clip>/frameNNNNN.{desc,tile}` — recorded by a `LIMINA_AV1_CAPTURE` run.
- `oracle.c` — rebuilds the stream and double-decodes it against the clip with
  dav1d, matching pictures on `frame_offset`. `AV1_ORACLE_DUMP=<path>` writes the
  rebuilt stream out.
- `coverage.c` — reports which syntax the fixtures exercise.
- `sw-oracle.c` — drives the serializer into dav1d in the backend's own order and
  reports the first unit dav1d refuses. Replays a capture directory offline, with no VM.
- `LIMINA_AV1_SLOT_TRACE=1` traces slot assignment and unresolved references.
- `LIMINA_AV1_DUMP=<path>` makes the worker append every reconstructed temporal unit,
  in submission order, to one file. That file is a stream `fhparse.py` and
  `dpb-check.py` read directly, which is what turns a live playback into the same
  offline diff the fixtures get.
