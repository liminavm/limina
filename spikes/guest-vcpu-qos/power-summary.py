#!/usr/bin/env python3
"""Mean CPU package power and cluster activity per window, from a powermetrics cpu_power capture
and power-arms.sh's phases.txt.

    power-summary.py <powermetrics.txt> <phases.txt>

Each sample is attributed to the window its header timestamp falls in; the first and last
sample of a window are dropped, so a sample straddling a boundary does not count.
"""
import collections
import datetime
import re
import statistics
import sys

samples = []  # (epoch, cpu_mw, {cluster: (freq_mhz, active_pct)})
cur = None
for line in open(sys.argv[1], errors="replace"):
    m = re.match(r"\*\*\* Sampled system activity \((.+?)\) \(", line)
    if m:
        if cur:
            samples.append(cur)
        t = datetime.datetime.strptime(m[1], "%a %b %d %H:%M:%S %Y %z").timestamp()
        cur = [t, None, {}]
        continue
    if cur is None:
        continue
    m = re.match(r"CPU Power: (\d+) mW", line)
    if m:
        cur[1] = int(m[1])
    m = re.match(r"(\w+)-Cluster HW active frequency: (\d+) MHz", line)
    if m:
        cur[2].setdefault(m[1], [None, None])[0] = int(m[2])
    m = re.match(r"(\w+)-Cluster HW active residency:\s+([\d.]+)%", line)
    if m:
        cur[2].setdefault(m[1], [None, None])[1] = float(m[2])
if cur:
    samples.append(cur)

windows = {}
for line in open(sys.argv[2]):
    ep, _, label, edge = line.split()
    windows.setdefault(label, {})[edge] = int(ep)

groups = collections.defaultdict(list)
print("| window | n | CPU mW mean | E active % | E MHz | P active % (P or P0) | P1 active % |")
print("|---|---|---|---|---|---|---|")
for label, w in windows.items():
    if "begin" not in w or "end" not in w:
        continue
    inw = [s for s in samples if w["begin"] <= s[0] <= w["end"] and s[1] is not None][1:-1]
    if not inw:
        continue
    mw = statistics.mean(s[1] for s in inw)

    def cl(name, i):
        v = [s[2][name][i] for s in inw if name in s[2] and s[2][name][i] is not None]
        return statistics.mean(v) if v else float("nan")

    print(f"| {label} | {len(inw)} | {mw:.0f} | {cl('E', 1):.0f} | {cl('E', 0):.0f} | {(cl('P0', 1) if 'P0' in inw[0][2] else cl('P', 1)):.0f} | {cl('P1', 1):.0f} |")
    kind = re.sub(r"-r\d+", "", label)
    groups[kind].append(mw)

print("\n| arm/window | CPU mW per rep | mean |")
print("|---|---|---|")
for k, v in groups.items():
    print(f"| {k} | {' / '.join(f'{x:.0f}' for x in v)} | {statistics.mean(v):.0f} |")
