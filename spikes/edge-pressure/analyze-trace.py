#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Summarise a `LIMINA_EDGE_TRACE=1` recording into the numbers a gesture constant is made of.

The chrome-ask constants were guessed three times and wrong three times, each guess fixing the
previous symptom and exposing the next. The way out is to record the movement the user actually
intends and read the constants off it — which needs the trace grouped into *gestures*, not lines.

A gesture here is a run of top-band events with no gap longer than --gap seconds. For each one it
reports what the current model would charge it: the time spent pushing (capped per event, the
`REVEAL_TICK_CAP` rule), the accumulated upward push in points, and whether it would have fired.

    spikes/edge-pressure/analyze-trace.py /tmp/enhanced-efi-kk-worker.log
    spikes/edge-pressure/analyze-trace.py --from 12.0 --to 20.0 log   # one recorded take
"""

import argparse
import re
import sys

# The model under test. Keep in step with crates/limina/src/window/input.rs.
HOLD = 0.25
TICK_CAP = 0.05
DECAY = 0.4
PUSH = 40.0

LINE = re.compile(
    r"\[REVEAL\] t=(?P<t>[-\d.]+) src=(?P<src>\w+) p=\((?P<x>[-\d.]+),(?P<y>[-\d.]+)\) dy=(?P<dy>[-\d.]+) "
    r"top=(?P<top>[-\d.]+) overlaid=(?P<ov>\w+) push=(?P<push>[-\d.]+) "
    r"charge=(?P<charge>[-\d.]+) ask=(?P<ask>\w+) (?P<why>\S+)"
)


def parse(path, t_from, t_to):
    out = []
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            m = LINE.search(line)
            if not m:
                continue
            e = m.groupdict()
            for k in ("t", "x", "y", "dy", "top", "push", "charge"):
                e[k] = float(e[k])
            e["t"] /= 1000.0
            e["ov"] = e["ov"] == "true"
            e["ask"] = e["ask"] == "true"
            if t_from is not None and e["t"] < t_from:
                continue
            if t_to is not None and e["t"] > t_to:
                continue
            out.append(e)
    return out


def group(events, gap):
    runs, cur = [], []
    for e in events:
        if cur and e["t"] - cur[-1]["t"] > gap:
            runs.append(cur)
            cur = []
        cur.append(e)
    if cur:
        runs.append(cur)
    return runs


def peak(run):
    """The high-water marks the app itself recorded, and when it fired.

    Read, do not re-simulate. An earlier version of this replayed the model here and got a
    different answer than the app, because only *some* of the early-return branches reset the
    charge — the trace's own `push=`/`charge=` fields are the ground truth and cost nothing.
    """
    hi_push = max((e["push"] for e in run), default=0.0)
    hi_charge = max((e["charge"] for e in run), default=0.0)
    fired = next((e["t"] - run[0]["t"] for e in run if e["ask"]), None)
    return hi_charge, hi_push, fired


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--gap", type=float, default=1.0, help="seconds of silence that ends a gesture")
    ap.add_argument("--from", dest="t_from", type=float)
    ap.add_argument("--to", dest="t_to", type=float)
    args = ap.parse_args()

    events = parse(args.log, args.t_from, args.t_to)
    if not events:
        print("no [REVEAL] lines — was LIMINA_EDGE_TRACE=1 set, and is the pointer reaching the "
              "top band of a notch=extend overlay?", file=sys.stderr)
        return 1

    runs = group(events, args.gap)
    print(f"{len(events)} events, {len(runs)} gestures "
          f"(model: hold={HOLD}s cap={TICK_CAP}s decay={DECAY}s push={PUSH}pt)\n")
    print(f"{'#':>3} {'start':>7} {'dur':>6} {'evts':>5} {'maxpush':>8} {'maxchg':>7} "
          f"{'strokes':>7} {'fired':>7}  reasons")
    for i, run in enumerate(runs, 1):
        charge, push, fired = peak(run)
        pushes = [e for e in run if e["why"] == "push"]
        # A stroke is an unbroken run of pushing events; every break is a finger lift or a pause.
        strokes = 0
        prev = None
        for e in pushes:
            if prev is None or e["t"] - prev > DECAY:
                strokes += 1
            prev = e["t"]
        why = {}
        for e in run:
            why[e["why"]] = why.get(e["why"], 0) + 1
        reasons = " ".join(f"{k}={v}" for k, v in sorted(why.items(), key=lambda kv: -kv[1]))
        print(f"{i:>3} {run[0]['t']:>7.1f} {run[-1]['t'] - run[0]['t']:>6.2f} {len(run):>5} "
              f"{push:>8.1f} {charge:>7.3f} {strokes:>7} "
              f"{('%.2fs' % fired) if fired is not None else '   no':>7}  {reasons}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
