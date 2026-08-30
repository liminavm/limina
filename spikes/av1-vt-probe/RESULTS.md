# VideoToolbox will give an AV1 backend the same 1:1 contract VP9 got

Measured 2026-08-30 on macOS 26.5.2 / **M4 Pro** (the M1 Max has no AV1 silicon, so
this spike only runs on an M3-or-later Mac):

```
VTIsHardwareDecodeSupported(AV1) = YES
av1C record                      : 17 bytes

               natural framing      per-frame framing
temporal units      90                    133
frame OBUs         133                    133
pictures out        90                    133
  real pixels       90                    133
  errors             0                      0
pixel format      420v 640x360 (2 planes), callback on VT's own thread
```

The clip carries **43 no-show frames** (133 frame OBUs, 90 displayed) — verified,
not assumed, because the VP9 spike learned that `-auto-alt-ref` can silently
produce none and leave you testing the easy path.

## What this settles

- **A picture per frame, if you frame per frame.** In its natural framing a
  temporal unit bundles a no-show frame with the frame that displays it, and
  VideoToolbox returns one picture for the pair — TU-level 1:1, which the virgl
  protocol cannot use, since it submits one *frame* at a time. Wrap each frame in
  its own temporal delimiter and all 133 come back with real pixels. That is the
  VP9 property, and it is the one the backend design rests on.
- **VideoToolbox owns the DPB.** Nothing here tells it about reference frames, and
  the no-show frames are stored and re-displayed correctly regardless. `ref[16]`
  in the picture descriptor stays dead weight on this path, exactly as in VP9.
- **A sequence header in every temporal unit is accepted.** All 133 TUs repeat it;
  no errors. A backend synthesizing headers can therefore emit one unconditionally
  instead of tracking when a new one is owed.
- **Same threading trap as VP9.** Synchronous in order (`arrived after return: 0`)
  but on VideoToolbox's own thread. Park the CVPixelBuffer, deliver after
  `DecodeFrame` returns.
- **`av1C` is required**, and is 4 bytes of configuration record plus the sequence
  header OBU — unlike VP9's `vpcC`, which is six scalars. `make-sample.sh` lifts a
  real one out of an MP4; synthesizing it from virgl's parsed fields is the
  backend's job.

## Film grain: VideoToolbox applies it, and gives you no grain-free picture

Measured on a clip encoded with `film-grain=30`, comparing VideoToolbox's luma against
dav1d's with grain applied and with `-filmgrain 0`:

```
vt       33302bb58b7aea373dc1cc1e7f8e7339
grain on 33302bb58b7aea373dc1cc1e7f8e7339   <- bit-identical
grainoff 246a5357e2919c9278c63aa76281680c
```

This matters because the protocol carries **two** surfaces — `target` and
`film_grain_target` (`virgl_video_hw.h:772`, with `apply_grain` at :818) — and expects a
grain-free reference alongside the grain-applied display picture. VideoToolbox hands back
only the grain-applied one.

It is not a blocker, because VideoToolbox owns the DPB: the reference chain never leaves
its side, so a grain-free picture is never needed for decode correctness. What it is, is a
**surface-mapping decision the backend must make deliberately** rather than discover — fill
the grain-applied picture into whichever surface the guest actually displays, and do not
assume `target` is the display one. Whether VideoToolbox can be asked for a grain-free
picture at all is untested.

## What this does NOT settle

Every frame here carried its **own original, encoder-produced frame header**. The
backend will have to *synthesize* that header from virgl's parsed
`virgl_av1_picture_desc`, and nothing in this spike exercises that. What the spike
removes is the risk that the serializer would be wasted work because VideoToolbox
refuses the framing or drops the no-show frames — it does neither.

The serializer's own cost is unchanged and lives elsewhere: the delta-coded fields
(`primary_ref_frame` inheritance, `global_motion_params()`'s `wm[7]`) need a shadow
DPB kept purely for serialization, in sync with the guest while VideoToolbox
separately owns the real one.

## Reproducing

```sh
./make-sample.sh                                  # encodes, and checks the clip has no-show frames
cc -O2 -o av1-vt-probe av1-vt-probe.c \
   -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework CoreFoundation
./av1-vt-probe sample.obu av1C.bin                # natural framing
./av1-vt-probe sample-perframe.obu av1C.bin       # per-frame — the unit virgl uses
./av1-vt-probe s.obu c.bin luma.gray              # 3rd arg dumps luma, for the grain diff
```
