# AV1 hardware decode: synthesizing the frame header the guest threw away

Follows VP9 (`docs/graphics.md` §4.5), which is shipped. AV1 is the only other codec a
**stock** guest can ask for — Fedora builds mesa `-Dvideo-codecs=all_free`, enforced
driver-independently in the VA frontend, so H.264/HEVC are absent from the guest driver
whatever the host offers. AV1 is free and already advertised.

## Why this one is not plumbing

VP9 was cheap because the guest's slice-data buffers already held the complete compressed
frame: concatenate, wrap, hand to VideoToolbox. AV1 is not, and the reason is a hard API
boundary rather than anything we control.

ffmpeg's AV1 decoder calls its hwaccel with `raw_tile_group->tile_data.data`
(`libavcodec/av1dec.c`, the `AV1_OBU_TILE_GROUP` case) — the tile payload **after** the tile
group header, and therefore long after the frame header. `libavcodec/vaapi_av1.c` passes
exactly that buffer to libva with per-tile offsets into it. The frame header survives only as
the parsed `VADecPictureParameterBufferAV1`, which mesa forwards to us as
`virgl_av1_picture_desc`. So the header bytes are destroyed at the ffmpeg→VA-API boundary,
which is a standard interface a stock guest is entitled to use.

VideoToolbox wants real OBUs. Therefore the backend must **re-serialize a conformant
`OBU_FRAME_HEADER` from parsed parameters**, plus a sequence header for the `av1C` box.
There is no way around this that keeps the stock tier.

**No prior art.** ffmpeg's own `libavcodec/videotoolbox_av1.c` is not a precedent: its
`start_frame`/`decode_slice` are stubs and `end_frame` appends the original OBUs verbatim,
because ffmpeg still holds the packet. Nothing in the ecosystem reconstructs an AV1 frame
header from parsed parameters — dav1d, libgav1, obuparse and gstreamer parse only, libaom
writes from encoder internals, rav1e is Rust.

## What VideoToolbox already guarantees

Measured on an M4 Pro, `spikes/av1-vt-probe/RESULTS.md`. It decodes AV1 in hardware, owns its
own DPB (so `ref[16]` stays dead weight as in VP9), accepts a repeated sequence header in
every temporal unit, and returns **a picture per frame including no-show frames** — but only
when each frame is wrapped in its own temporal delimiter. That last point is the one the
design rests on, and it matches the unit virgl submits.

**AV1 silicon is M3-or-later.** The dev Mac (M1 Max) cannot decode AV1 at all, so the
*VideoToolbox* end-to-end check has to run on an M3+ host and the L2 test must SKIP cleanly
elsewhere. Everything up to that point — fixture capture, the serializer, and the dav1d
oracle — runs locally; see the phases below.

## Licence: the serializer is ours, and the oracle is dav1d

virglrenderer is **MIT**. Vendoring ffmpeg's `cbs_av1` (LGPL-2.1+) into it would relicense the
file and make the fork's delta unupstreamable, which is the opposite of what the fork is for.
So the serializer is **written from the AV1 spec, inside virglrenderer, MIT**. The syntax is
fully specified; this is bounded work.

**The oracle is `dav1d`** — BSD-2, already on the dev Mac (1.5.4 via Homebrew), and a far
better instrument than the bitstream rewriter originally planned:

- It is a **real decoder**, so it validates *conformance*, not merely that fields survive a
  round-trip. If dav1d decodes our synthesized frame to the right pixels, the header is right
  in every way that matters.
- `Dav1dPicture.frame_hdr` is **public** (`include/dav1d/picture.h`), so the same run also
  yields a parsed `Dav1dFrameHeader` for field-by-field diffing when pixels disagree — which
  is what keeps a wrong field from presenting only as wrong pixels far from its cause.
- It carries **no GPL entanglement**, so nothing constrains where the oracle may live.
- It is **pure software**, so the entire serializer can be developed and tested on a machine
  with no AV1 silicon. VideoToolbox and M3+ hardware enter only at the end.

`Dav1dFrameHeader` exposes `primary_ref_frame`, `refresh_frame_flags`, `segmentation`, and
`gmv[]` as `Dav1dWarpedMotionParams` — *reconstructed* warp parameters, the same representation
virgl hands us. So a descriptor → our OBU → dav1d round-trip compares `wmmat` directly, and a
wrong `PrevGmParams` base shows up as a wrong reconstructed value rather than staying hidden
inside a subexp symbol.

## Shape of the work

**Phase 0 — capture fixtures. This needs no AV1 silicon.** An AV1 path that advertises caps,
records the `virgl_av1_picture_desc` and tile bytes for every frame, and decodes nothing,
returning success. The descriptors are a pure function of the *guest's own* bitstream parse —
`av1dec.c` parses every OBU itself and `vaapi_av1.c` fills the VA parameters from that parse,
with no feedback from decoded output — so a local poke VM on the M1 produces exactly the
fixtures an M3 would. Caps advertisement is env-forced past `vt_can_decode` for the capture
build only. (The assumption to confirm in the first run: ffmpeg keeps submitting frames when
the stub reports success and returns no picture.)

This is what makes everything after it offline work: with real descriptors plus the matching
real stream, the serializer and its dav1d oracle iterate at unit-test speed, on this machine,
with no VM in the loop. It is also the first chance to confirm on real hardware what this
document asserts from source — that the buffer holds tile data only.

Keep the probe as a patch under `spikes/av1-obu-serializer/`, since the source it instruments
lives in gitignored `third_party/`.

Encode the capture clips **deliberately and verify each property is present**, per the VP9
`-auto-alt-ref` lesson: film grain, global motion, segmentation, multiple tiles, superres, and
no-show frames. Fixture breadth is oracle coverage — the serializer is only tested on the
syntax the fixtures actually exercise.

**Phase 1 — the serializer** (`src/vrend/virgl_video_av1_obu.c`): sequence header OBU, frame
header OBU, tile group OBU re-emitting the tile entries with `tile_size_minus_1`, and the
`av1C` record (4 bytes plus the sequence header OBU, unlike VP9's six-scalar `vpcC`).

### How much shadow state the writer actually needs: one array, not an apparatus

`primary_ref_frame` makes several things *inherit* from a reference slot, which looks like it
forces us to track everything the decoder will inherit. Enumerated against
`cbs_av1_syntax_template.c` — the executable spec — most of it does not, and the distinction
is sharp:

> A field coded **relative to reference state** needs shadow state, because the writer must
> know the base to compute the coded delta. A field with an **inherit flag** does not: set the
> flag and write the resolved value the descriptor already carries, and the decoder — which
> has the real reference history — never consults its base at all.

- **Segmentation is shadow-free.** `feature_value[i][j]` is written absolutely (`sus`/`fbs`)
  whenever `segmentation_update_data` is set, and *inferred* from the reference otherwise.
  Force `update_data = 1` and emit the resolved `seg_info.feature_data` / `feature_mask`.
- **Loop-filter deltas are shadow-free.** `loop_filter_ref_deltas[i]` is `sus(1+6, …)`, again
  absolute, behind `loop_filter_delta_update` and a per-index `update_ref_delta[i]`. The
  descriptor carries resolved `ref_deltas[8]`/`mode_deltas[2]` but no per-index update flags,
  so force them all to 1 and write the values.
- **Film grain is shadow-free.** `update_grain = 0` means "load the params from
  `film_grain_params_ref_idx`", and the descriptor carries neither field — but it does carry
  every resolved grain parameter. `update_grain` is a plain flag on INTER frames and inferred
  1 elsewhere, so always writing 1 with the resolved params is conformant and closes the hole.
- **Global motion is the one exception, and it is real.** `global_motion_param()` codes each
  value as `subexp` against `PrevGmParams` — the primary reference's saved warp parameters, or
  the defaults when `primary_ref_frame` is `NONE`. We hold *reconstructed* `wm[7].wmmat[]`, so
  producing the coded symbol requires that base.

  Note ffmpeg cannot be borrowed from here even conceptually: `cbs_av1` stores `gm_params` as
  the **raw coded symbol** and explicitly does not reconstruct the warp value
  (`"Actual gm_params value is not reconstructed here"`). It rewrites bitstreams without ever
  converting between the two representations; we have only the far side.

So the shadow state is one `int32_t saved_gm[8][7][6]`, written for each slot in
`refresh_frame_flags` at the end of a frame and read through
`ref_frame_idx[primary_ref_frame]`. It is also **inert for most content**: global motion is
skipped entirely on KEY and INTRA_ONLY frames, and `is_global[ref] = 0` codes as a single zero
bit with no parameters at all.

That `cbs_av1` keeps coded symbols is a bonus for the oracle rather than a loss — the diff
then compares what actually goes on the wire.

**Phase 2 — the offline oracle**: a spike under `spikes/av1-obu-serializer/` that compiles the
serializer straight from the virglrenderer source, runs it over the captured fixtures, and hands
the result to dav1d — comparing pixels against dav1d's decode of the original stream, and
falling back to a field-by-field header diff to localise a failure.

**Phase 3 — wire into the backend**: per-frame temporal delimiters, and the film-grain surface
decision. VideoToolbox applies grain and returns no grain-free picture (measured bit-identical
to dav1d with grain on). Not a blocker, since VT owns the DPB and a grain-free picture is never
needed for decode correctness — but the protocol carries two surfaces, `target` and
`film_grain_target` (`virgl_video_hw.h:772`, `apply_grain` at :818), and the backend must
**decide the mapping deliberately**: fill the grain-applied picture into whichever surface the
guest displays, and do not assume that is `target`.

**Phase 4 — an L2 test**, which can only assert end-to-end on M3+ hardware.

## Traps carried forward

- **Name the unit when counting.** In a stream's natural framing a temporal unit bundles a
  no-show frame with the frame that displays it and one picture comes back for the pair.
  Measuring submissions-in against pictures-out without saying which unit is being counted
  reads as a clean pass while a third of the frames are dropped.
- **A clip must actually contain no-show frames**, verified rather than assumed —
  `-auto-alt-ref` can silently produce none and leave only the easy path tested.
- The same threading rule as VP9: VideoToolbox's callback is ordered-synchronous but on its
  own thread, which holds no GL context. Park the picture, deliver after `DecodeFrame` returns.
