#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Summarise the `[EDGE]` half of a `LIMINA_EDGE_TRACE=1` recording: edge resistance.

Companion to analyze-trace.py (which reads `[REVEAL]`, the chrome ask). Same discipline, and it
matters more here: this reads the model's OWN logged state (`acc`, `through`, `thr`, `warp`) and
never re-derives it. Round 2 of this spike lost time to an analyzer that re-simulated the model
and disagreed with the app, because only some branches reset the accumulator — the resistance
model has strictly more branches (a corner arm that does not accumulate, per-axis drains, a
zero-delta early return), so re-simulating it would be worse.

A *gesture* is a run of held-or-pushing events with no gap longer than --gap seconds. For each
one it reports which edge, how long it took to break through (if it did), how far the cursor was
warped against the user in total, and what the guest received as pressure.

    spikes/edge-pressure/analyze-resist.py /tmp/enhanced-efi-kk-worker.log
    spikes/edge-pressure/analyze-resist.py --from 12.0 --to 20.0 log    # one labelled take
    spikes/edge-pressure/analyze-resist.py --takes log                  # auto-split into takes
"""

import argparse
import math
import re

LINE = re.compile(
    r"\[EDGE\] t=(?P<t>[-\d.]+) cur=\((?P<x>[-\d.]+),(?P<y>[-\d.]+)\) "
    r"d=\((?P<dx>[-\d.]+),(?P<dy>[-\d.]+)\) "
    r"fit=\((?P<fx>[-\d.]+),(?P<fy>[-\d.]+) (?P<fw>[-\d.]+)x(?P<fh>[-\d.]+)\) "
    r"overlaid=(?P<ov>\w+) free=(?P<free>\w+) overflow=\((?P<ox>[-\d.]+),(?P<oy>[-\d.]+)\) "
    r"acc=\((?P<ax>[-\d.]+),(?P<ay>[-\d.]+)\) through=(?P<through>\w+) thr=(?P<thr>[-\d.]+) "
    r"warp=(?P<warp>[-\d.]+) outside=(?P<outside>\w+) escaped=(?P<escaped>\w+)"
)

FLOATS = ("t", "x", "y", "dx", "dy", "fx", "fy", "fw", "fh", "ox", "oy", "ax", "ay", "thr", "warp")
BOOLS = ("ov", "free", "through", "outside", "escaped")


def parse(path, t_from, t_to):
    out = []
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            m = LINE.search(line)
            if not m:
                continue
            e = m.groupdict()
            for k in FLOATS:
                e[k] = float(e[k])
            for k in BOOLS:
                e[k] = e[k] == "true"
            if t_from is not None and e["t"] < t_from * 1000:
                continue
            if t_to is not None and e["t"] > t_to * 1000:
                continue
            out.append(e)
    return out


def which_edge(e):
    """Which edge this event is against, from the position and the fit. Descriptive only."""
    eps = 2.0
    at_left = e["x"] - e["fx"] <= eps
    at_right = e["fx"] + e["fw"] - e["x"] <= eps
    at_bottom = e["y"] - e["fy"] <= eps
    at_top = e["fy"] + e["fh"] - e["y"] <= eps
    names = [n for n, hit in
             (("left", at_left), ("right", at_right), ("bottom", at_bottom), ("top", at_top)) if hit]
    return "+".join(names) if names else "inside"


def group(events, gap):
    """Split into gestures: runs of engaged events (held, or accumulating) separated by `gap`."""
    takes, cur, last_t = [], [], None
    for e in events:
        engaged = (not e["free"]) or e["ax"] > 0 or e["ay"] > 0
        if not engaged:
            continue
        if last_t is not None and (e["t"] - last_t) / 1000.0 > gap:
            if cur:
                takes.append(cur)
            cur = []
        cur.append(e)
        last_t = e["t"]
    if cur:
        takes.append(cur)
    return takes


def report(g, idx):
    t0, t1 = g[0]["t"], g[-1]["t"]
    dur = (t1 - t0) / 1000.0
    edges = {}
    for e in g:
        edges[which_edge(e)] = edges.get(which_edge(e), 0) + 1
    edge = max(edges, key=edges.get)
    held = [e for e in g if not e["free"]]
    broke = next((e for e in g if e["free"] and e["through"]), None)
    warp_total = sum(e["warp"] for e in g)
    warp_max = max((e["warp"] for e in g), default=0.0)
    travel = sum(math.hypot(e["dx"], e["dy"]) for e in g)
    pressure = sum(math.hypot(e["ox"], e["oy"]) for e in g)
    peak_acc = max((max(e["ax"], e["ay"]) for e in g), default=0.0)
    escaped = sum(1 for e in g if e["escaped"])
    outside = sum(1 for e in g if e["outside"])
    print(f"  #{idx:<3} t={t0/1000:7.2f}s  {edge:<12} {len(g):>4} events  {dur:5.2f}s")
    print(f"        held {len(held):>4}   broke through: "
          f"{'no' if not broke else f'yes at {(broke[
              't'] - t0)/1000:.2f}s'}   peak acc {peak_acc:6.1f} / thr {g[0]['thr']:.0f}")
    print(f"        travel {travel:7.1f} pt   forwarded to guest {pressure:7.1f} pt   "
          f"warped against user {warp_total:6.1f} pt (max {warp_max:.1f})")
    if escaped or outside:
        print(f"        outside the fit {outside}   escaped the view {escaped}"
              f"   <- must be 0 warp; got {sum(e['warp'] for e in g if e['escaped']):.1f}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--gap", type=float, default=1.0, help="seconds of idle that ends a gesture")
    ap.add_argument("--from", dest="t_from", type=float, default=None)
    ap.add_argument("--to", dest="t_to", type=float, default=None)
    args = ap.parse_args()

    events = parse(args.log, args.t_from, args.t_to)
    if not events:
        print("no [EDGE] lines in range — is LIMINA_EDGE_TRACE=1 set, and the tap installed?")
        return
    print(f"{len(events)} [EDGE] events, "
          f"t={events[0]['t']/1000:.1f}s..{events[-1]['t']/1000:.1f}s, thr={events[0]['thr']:.0f}")
    gestures = group(events, args.gap)
    print(f"{len(gestures)} gestures (gap {args.gap}s)\n")
    for i, g in enumerate(gestures, 1):
        report(g, i)

    # The two aggregates that judge the model rather than a gesture.
    warped_while_escaped = sum(e["warp"] for e in events if e["escaped"])
    print(f"\nwarp applied while the pointer had escaped the view: {warped_while_escaped:.1f} pt"
          f"  (MUST be 0.0 — anything else is the pointer being dragged off another display)")
    broke = [g for g in gestures if any(e["free"] and e["through"] for e in g)]
    if broke:
        times = sorted(((next(e for e in g if e["free"] and e["through"])["t"] - g[0]["t"]) / 1000.0)
                       for g in broke)
        mid = times[len(times) // 2]
        print(f"time-to-breakthrough over {len(times)} gestures: "
              f"min {times[0]:.2f}s  median {mid:.2f}s  max {times[-1]:.2f}s")


if __name__ == "__main__":
    main()
