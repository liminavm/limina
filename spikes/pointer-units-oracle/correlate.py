#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
"""Join the host [WIRE] trace with the guest cursor-plane log.

Answers, from measurement:
  1. UNITS: for each CRTC, which interval of the ABS range lands on it; the seam fraction is
     compared against the logical-extents and pixel-extents predictions.
  2. SEAM BUG: an interleaved timeline (host events + guest cursor placements) around any
     moment you name with --around t_us, for reading a teleport event for the record.

Usage:
  correlate.py wire.log guest.log --logical 1512x982,2048x1152 --pixel 3024x1964,2560x1440
  correlate.py wire.log guest.log --around 1755550000000000 --window-ms 500
"""
import argparse
import bisect
import re
import sys

ABS_MAX = 0x7FFF  # matches limina-input ABS_MAX
EV_ABS, ABS_X, ABS_Y = 3, 0, 1

WIRE_ABS = re.compile(r"\[WIRE\] t=(\d+) dev=abs type=(\d+) code=(\d+) value=(-?\d+)")
WIRE_REL = re.compile(r"\[WIRE\] t=(\d+) dev=rel dx=(-?\d+) dy=(-?\d+)")
GUEST = re.compile(
    r"t=(\d+) plane=\d+ crtc=(\S+) pos=(-?\d+),(-?\d+) size=(\d+)x(\d+) fb=(\d+)"
)


def parse_wire(path):
    """Pair consecutive ABS_X/ABS_Y writes into positions; collect REL bursts."""
    abs_pos, rel = [], []
    pend_x = None
    for line in open(path, errors="replace"):
        m = WIRE_ABS.search(line)
        if m:
            t, typ, code, val = map(int, m.groups())
            if typ != EV_ABS:
                continue
            if code == ABS_X:
                pend_x = (t, val)
            elif code == ABS_Y and pend_x is not None:
                abs_pos.append((t, pend_x[1], val))
                pend_x = None
            continue
        m = WIRE_REL.search(line)
        if m:
            t, dx, dy = map(int, m.groups())
            rel.append((t, dx, dy))
    return abs_pos, rel


def parse_guest(path):
    out = []
    for line in open(path, errors="replace"):
        m = GUEST.search(line)
        if m:
            t, crtc, x, y, w, h, fb = m.groups()
            out.append((int(t), crtc, int(x), int(y), f"{w}x{h}", int(fb)))
    return out


def units_report(abs_pos, guest, args):
    """Per CRTC: the ABS_X interval whose events were followed (<=60ms) by a cursor
    placement with a live fb on that CRTC. The seam fraction decides the model."""
    gt = [g[0] for g in guest]
    intervals = {}
    for t, x, _y in abs_pos:
        i = bisect.bisect_left(gt, t)
        for j in range(i, min(i + 8, len(guest))):
            g = guest[j]
            if g[0] - t > 60_000:
                break
            if g[5] == 0:  # no fb -> cursor hidden on that CRTC
                continue
            lo, hi = intervals.get(g[1], (ABS_MAX, 0))
            intervals[g[1]] = (min(lo, x), max(hi, x))
            break
    print("== UNITS ==")
    if not intervals:
        print("no correlated samples — check clocks and that the poller saw a live fb")
        return
    for crtc, (lo, hi) in sorted(intervals.items()):
        print(f"  {crtc}: ABS_X in [{lo}, {hi}]  ({lo/ABS_MAX:.4f}..{hi/ABS_MAX:.4f})")
    for name, spec in (("logical", args.logical), ("pixel", args.pixel)):
        if not spec:
            continue
        widths = [int(s.split("x")[0]) for s in spec.split(",")]
        total = sum(widths)
        fr = widths[0] / total
        print(f"  {name} model predicts first seam at fraction {fr:.4f}")


def timeline(abs_pos, rel, guest, around, window_us):
    rows = []
    rows += [(t, f"host abs x={x} y={y}") for t, x, y in abs_pos]
    rows += [(t, f"host REL dx={dx} dy={dy}") for t, dx, dy in rel]
    rows += [(t, f"guest {c} pos={x},{y} fb={fb}") for t, c, x, y, _s, fb in guest]
    rows.sort()
    print(f"== TIMELINE around t={around} ±{window_us}us ==")
    for t, s in rows:
        if abs(t - around) <= window_us:
            print(f"  {t}  {s}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("wire")
    ap.add_argument("guest")
    ap.add_argument("--logical", help="per-monitor logical sizes, WxH,WxH in guest order")
    ap.add_argument("--pixel", help="per-monitor pixel sizes, WxH,WxH in guest order")
    ap.add_argument("--around", type=int, help="epoch µs to dump the merged timeline at")
    ap.add_argument("--window-ms", type=int, default=250)
    args = ap.parse_args()

    abs_pos, rel = parse_wire(args.wire)
    guest = parse_guest(args.guest)
    print(f"parsed: {len(abs_pos)} abs positions, {len(rel)} rel bursts, "
          f"{len(guest)} guest samples", file=sys.stderr)
    units_report(abs_pos, guest, args)
    if args.around:
        timeline(abs_pos, rel, guest, args.around, args.window_ms * 1000)


if __name__ == "__main__":
    main()
