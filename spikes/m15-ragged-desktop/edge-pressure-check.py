#!/usr/bin/env python3
"""Classify edge-pressure events from a LIMINA_POINTER_WIRE_TRACE run.

The uncaptured path charges pressure only at a slot's OUTER edges, and decides that per
point (`window::arrangement::outer_edges_at`). On a ragged desktop a single edge is a wall
over the span no neighbour covers and a seam over the span one does, so that one edge must
charge on one half and stay silent on the other. That split is the whole test: the wall
half is the differential (the old per-side test called the entire edge a seam and dropped
every push), and the silent half shows the fix does not over-correct.

Three things this has to get right, each learned by getting it wrong:

* **Only uncaptured motion counts.** While captured, pressure comes from the confinement
  clamp instead and never consults `outer_edges_at`; counting it attributes one fix's work
  to the other. Capture state is tracked across the whole file, since the state on entering
  the window under test is set before it.
* **Coverage, not just counts.** A half that never reached the edge proves nothing by being
  silent, so `reach` is what makes a zero meaningful.
* **Reach is measured on the monitor under test.** A position out past the edge is on the
  NEIGHBOUR and would report a reach the slot's own edge never saw.
"""

import argparse
import re
import sys

ABS = re.compile(r"\[WIRE\] t=(\d+) dev=abs type=(\d+) code=(\d+) value=(-?\d+)")
REL = re.compile(r"\[WIRE\] t=(\d+) dev=rel dx=(-?\d+) dy=(-?\d+)")
EV_ABS, ABS_X, ABS_Y = 3, 0, 1

# depth axis, along axis, (x,y,dx,dy) index of the push, and which side of the edge the
# monitor under test lies on.
EDGES = {
    "right":  (0, 1, 2, "below"),
    "bottom": (1, 0, 3, "below"),
    "left":   (0, 1, 2, "above"),
    "top":    (1, 0, 3, "above"),
}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--from-line", type=int, default=0)
    ap.add_argument("--to-line", type=int, default=None)
    ap.add_argument("--abs-max", type=float, default=32767.0)
    ap.add_argument("--bbox", required=True, help="desktop bounding box WxH, guest logical units")
    ap.add_argument("--edge", choices=sorted(EDGES), required=True)
    ap.add_argument("--edge-at", type=float, required=True,
                    help="the edge's own coordinate (guest x for left/right, y for top/bottom)")
    ap.add_argument("--boundary", type=float, required=True,
                    help="coordinate ALONG the edge where wall becomes seam")
    ap.add_argument("--wall", choices=("low", "high"), default="low",
                    help="which side of the boundary the neighbour does NOT cover")
    ap.add_argument("--tol", type=float, default=3.0)
    ap.add_argument("--label", default="run")
    a = ap.parse_args()

    bw, bh = (float(v) for v in a.bbox.lower().split("x"))
    depth, along, push, side = EDGES[a.edge]
    along_max = bw if along == ABS_X else bh

    pos = [None, None]
    press, reach = [], {"wall": [], "seam": []}
    captured, dropped = False, 0

    with open(a.log, "rb") as fh:
        for n, raw in enumerate(fh, 1):
            line = raw.decode("utf-8", "replace")
            if "pointer capture: ON" in line or "promoted to a hard grab" in line:
                captured = True
            elif "pointer capture: OFF" in line:
                captured = False
            if n <= a.from_line or (a.to_line and n > a.to_line):
                continue
            if captured:
                dropped += 1 if REL.search(line) else 0
                continue
            if m := ABS.search(line):
                _, t, code, val = (int(g) for g in m.groups())
                if t != EV_ABS or code not in (ABS_X, ABS_Y):
                    continue
                pos[code] = int(val) / a.abs_max * (bw if code == ABS_X else bh)
                if code == ABS_Y and None not in pos:
                    on_slot = (pos[depth] >= a.edge_at) if side == "above" else (pos[depth] <= a.edge_at)
                    if on_slot:
                        half = "wall" if (pos[along] < a.boundary) == (a.wall == "low") else "seam"
                        reach[half].append(pos[depth])
            elif (m := REL.search(line)) and None not in pos:
                press.append((pos[0], pos[1], int(m.group(2)), int(m.group(3))))

    def half_of(p):
        return "wall" if (p[along] < a.boundary) == (a.wall == "low") else "seam"

    def report(name, want):
        got = [p for p in press if half_of(p) == name and p[push] != 0]
        r = reach[name]
        deep = (min(r) if side == "above" else max(r)) if r else None
        covered = deep is not None and (
            deep <= a.edge_at + a.tol if side == "above" else deep >= a.edge_at - a.tol
        )
        print(f"  {name} half {'(must charge)  ' if want else '(must stay silent)'} "
              f"pressure={len(got):<4} reach={('%.1f' % deep) if deep is not None else 'NEVER':<9} "
              f"edge={a.edge_at:g} {'[reached]' if covered else '[NOT reached]'}")
        if got:
            d = [p[push] for p in got]
            print(f"      push {min(d):+d}..{max(d):+d}; along-edge "
                  f"{min(p[along] for p in got):.0f}..{max(p[along] for p in got):.0f}")
        return ((len(got) > 0) if want else (len(got) == 0)), covered

    print(f"[{a.label}] lines {a.from_line + 1}..{a.to_line or 'end'} — "
          f"{len(press)} uncaptured pressure events ({dropped} dropped as captured)")
    ok_w, cov_w = report("wall", True)
    ok_s, cov_s = report("seam", False)

    bad = []
    if not cov_w:
        bad.append("wall half never reached the edge")
    if not cov_s:
        bad.append("seam half never reached the edge — its silence proves nothing")
    if not ok_w:
        bad.append("WALL HALF DID NOT CHARGE")
    if not ok_s:
        bad.append("SEAM HALF CHARGED — over-correction")
    print("  verdict:", "; ".join(bad) if bad else "as designed")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
