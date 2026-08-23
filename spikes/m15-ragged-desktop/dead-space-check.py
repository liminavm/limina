#!/usr/bin/env python3
"""Did the absolute pointer we sent ever land on no monitor at all?

The guest spreads its one absolute device over the BOUNDING BOX of its desktop, and a
desktop is a union of rectangles: any vertical offset or height mismatch leaves corners of
that box belonging to no monitor. A position there puts the guest's pointer over no output,
and since the cursor plane is per-scanout, nothing draws it — the cursor is simply gone.

Feed this a worker log taken with LIMINA_POINTER_WIRE_TRACE=1 (which logs every value we put
on the wire) plus the guest's own reported rects, and it replays every absolute position
against the desktop. The answer that matters is the second line: it must be 0.

    ./dead-space-check.py <worker.log> <x,y,w,h> <x,y,w,h> ...

The rects are the guest's LOGICAL monitor rects, exactly as its compositor reports them —
read them from `busctl --user call org.gnome.Mutter.DisplayConfig … GetCurrentState` in the
guest, or from the `display: the guest reports its monitors as …` line in the log.

    ./dead-space-check.py /tmp/limina-worker-ragged.log 1512,0,2048,1152 0,747,1512,948

Boundaries are inclusive to half a unit: monitors abut at an exact coordinate, so a point on
a seam is on both of them and is not dead.
"""
import re
import sys

ABS_MAX = 32767.0
TOL = 0.5


def inside(rect, gx, gy):
    x, y, w, h = rect
    return x - TOL <= gx <= x + w + TOL and y - TOL <= gy <= y + h + TOL


def main(argv):
    if len(argv) < 3:
        sys.exit(__doc__)
    log, rects = argv[1], [tuple(float(n) for n in a.split(",")) for a in argv[2:]]
    dw = max(r[0] + r[2] for r in rects)
    dh = max(r[1] + r[3] for r in rects)

    x = None
    points = []
    for line in open(log, errors="replace"):
        m = re.search(r"dev=abs type=3 code=(\d) value=(-?\d+)", line)
        if not m:
            continue
        if m.group(1) == "0":
            x = int(m.group(2))
        elif x is not None:
            points.append((x / ABS_MAX * dw, int(m.group(2)) / ABS_MAX * dh))

    dead = [p for p in points if not any(inside(r, *p) for r in rects)]
    print(f"absolute positions sent      : {len(points)}")
    print(f"landing on no monitor at all : {len(dead)}")
    for p in dead[:10]:
        print(f"    guest ({p[0]:.0f},{p[1]:.0f})")

    # How far into each wall the pointer was actually driven — a run that never reached an
    # edge proves nothing, so print the extremes and read them before believing the zero.
    for i, r in enumerate(rects):
        own = [p for p in points if inside(r, *p) and not any(
            inside(o, *p) for j, o in enumerate(rects) if j != i)]
        if own:
            print(f"rect {i} {r}: y reached {min(q[1] for q in own):.1f} .. "
                  f"{max(q[1] for q in own):.1f}, x {min(q[0] for q in own):.1f} .. "
                  f"{max(q[0] for q in own):.1f}")
    return 1 if dead else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
