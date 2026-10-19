#!/usr/bin/env python3
"""summarize.py <ab-output> — per-arm median/min/max of total_Miter from ab.sh lines, the delta of
each arm's median against base, and the per-round paired delta (arm vs base in the same round),
which cancels slow host drift. Also checks every module arm held its timer rate."""
import re, statistics, sys
from collections import defaultdict

runs = defaultdict(dict)
fires = defaultdict(list)
for line in open(sys.argv[1]):
    m = re.match(r"round=(\d+) arm=(\S+) .*total_Miter ([\d.]+) \|(.*)", line)
    if not m:
        continue
    r, arm, tot, tail = int(m[1]), m[2], float(m[3]), m[4]
    runs[arm][r] = tot
    for f, rd in re.findall(r"fires=(\d+) reads=(\d+)", tail):
        fires[arm].append((int(f), int(rd)))

base = runs["base"]
bmed = statistics.median(base.values())
print(f"{'arm':15} {'n':>2} {'median':>9} {'min':>9} {'max':>9} {'d_median':>9} {'paired_med':>10} {'paired_min':>10} {'paired_max':>10}  fires/cpu reads/fire")
for arm in sorted(runs, key=lambda a: (a != "base", a)):
    v = list(runs[arm].values())
    med = statistics.median(v)
    paired = [100 * (runs[arm][r] / base[r] - 1) for r in runs[arm] if r in base]
    fr = fires.get(arm)
    fs = f"{min(f for f, _ in fr)}-{max(f for f, _ in fr)} {fr[0][1] / fr[0][0]:.0f}" if fr else "-"
    print(f"{arm:15} {len(v):2} {med:9.1f} {min(v):9.1f} {max(v):9.1f} {100 * (med / bmed - 1):+8.2f}% "
          f"{statistics.median(paired):+9.2f}% {min(paired):+9.2f}% {max(paired):+9.2f}%  {fs}")
