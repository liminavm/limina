#!/usr/bin/env python3
"""
Compare what each reference RESOLVES to, in the original stream and in the rebuilt one.

This is the diagnostic that works on a slot bug, and the reason it exists is that nothing
cheaper does. A lost reference does not fail at the frame that lost it: every frame header
still parses field-for-field identical to its descriptor, cbs_av1 accepts the whole stream,
and the damage surfaces many frames later inside dav1d_decode_tile_sbrow as a bare EINVAL
with no message. "The header parses" says nothing about slot state.

refresh_frame_flags and ref_frame_idx are ours to choose -- VA-API does not carry the
former -- so they are EXPECTED to differ from the encoder's. What may not differ is the
picture each reference lands on. Both DPB state machines are simulated here and their
resolutions compared; the first divergence is the bug.

  ./dpb-check.py resolve <original.obu> <rebuilt.obu>
        Per frame, the seven references and the CDF source, both sides.

  ./dpb-check.py exposure <clip.obu> [...]
        How many decode calls separate a hidden frame from the show_existing_frame that
        displays it. The serializer holds a hidden frame until the next decode call, so a
        gap of zero would mean a guest displaying a picture the host has not delivered yet
        -- a stale surface, silently, since the guest's fence is a command-stream fence and
        never waits on delivery (mesa virgl_video.c: virgl_video_end_frame flushes the
        queue; get_decoder_fence waits on that fence, not on the picture).
"""
import sys
sys.path.insert(0, __file__.rsplit('/', 1)[0])
from fhparse import walk, seq, fh, decoded_frames


def resolutions(frames):
    """Per frame: the picture each of the seven references lands on, and the one the CDF
    is inherited from, naming pictures by order hint."""
    slots, out = [None] * 8, []
    for f in frames:
        if f['frame_type'] == 0:
            refs, cdf = [None] * 7, None
        else:
            refs = [slots[f['ref_idx'][j]] for j in range(7)]
            pr = f['primary_ref']
            cdf = None if pr == 7 else slots[f['ref_idx'][pr]]
        out.append((refs, cdf))
        for b in range(8):
            if f['refresh'] >> b & 1:
                slots[b] = f['order_hint']
    return out


def cmd_resolve(original, rebuilt):
    _, O = decoded_frames(original)
    _, R = decoded_frames(rebuilt)
    if len(O) != len(R):
        print(f"decode counts differ: {len(O)} vs {len(R)} -- not comparable")
        return 1
    bad = 0
    for i, (a, b) in enumerate(zip(resolutions(O), resolutions(R))):
        if a != b:
            bad += 1
            if bad <= 8:
                print(f"frame {i:3d} oh={O[i]['order_hint']:3d} primary_ref={O[i]['primary_ref']}")
                if a[0] != b[0]:
                    print(f"    refs original {a[0]}\n         rebuilt  {b[0]}")
                if a[1] != b[1]:
                    print(f"    CDF inherited from {a[1]} originally, {b[1]} rebuilt")
    print(f"\n{bad} of {len(O)} frames resolve a reference differently")
    return 1 if bad else 0


def cmd_exposure(paths):
    worst = 0
    for path in paths:
        obus = walk(open(path, 'rb').read(), limit=20000)
        S = seq(next(p for t, p in obus if t == 1))
        slot_decode = [None] * 8      # decode index that wrote each slot
        hidden = set()                # decode indices of hidden frames
        decodes, risky, gaps = 0, 0, []
        for t, p in obus:
            if t not in (3, 6):
                continue
            F = fh(p, S)
            if F.get('show_existing'):
                src = slot_decode[(p[0] >> 4) & 7]
                if src in hidden:
                    gap = decodes - src
                    gaps.append(gap)
                    if gap < 1:
                        risky += 1
                continue
            decodes += 1
            if not F['show_frame']:
                hidden.add(decodes)
            for b in range(8):
                if F['refresh'] >> b & 1:
                    slot_decode[b] = decodes
        worst = max(worst, risky)
        print(f"{path.rsplit('/', 1)[-1]:16s} decodes={decodes:3d}  hidden frames displayed "
              f"later={len(gaps):3d}  smallest gap={min(gaps) if gaps else '-'}  "
              f"displayed with NO intervening decode={risky}")
    return 1 if worst else 0


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    sys.exit(cmd_resolve(sys.argv[2], sys.argv[3]) if sys.argv[1] == "resolve"
             else cmd_exposure(sys.argv[2:]))
