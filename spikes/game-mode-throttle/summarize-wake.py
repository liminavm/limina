#!/usr/bin/env python3
"""Summarise wake.m output from run.sh: per arm, Game Mode window (t=7..20) vs outside.

For each wake kind: median of the per-second p50s, and the worst per-second p99 (microseconds).
busy: mean CPU share and worst stretch without running. prio: the set of priorities seen.
Usage: summarize-wake.py <run.sh log>
"""
import re, sys, statistics, collections

line_re = re.compile(r'^(\S+) t=(\d+) prio main=(-?\d+).*?\| fifo (\S+) \| plain (\S+) \| kqcrit (\S+) \| '
                     r'dstrict (\S+) \| busy ([\d.]+)% maxgap ([\d.]+)ms \| au (\S+)')
stat_re = re.compile(r'(\d+)/(\d+)/(\d+)\((\d+)\)')
rows = collections.defaultdict(lambda: collections.defaultdict(list))
for line in open(sys.argv[1]):
    m = line_re.match(line)
    if not m:
        continue
    arm, t, prio = m.group(1), int(m.group(2)), int(m.group(3))
    if t < 2 or t > 30:
        continue  # thread start-up and teardown seconds
    win = 'gm' if 8 <= t <= 20 else ('out' if t <= 5 or t >= 24 else None)
    if win is None:
        continue  # transitions
    r = rows[(arm, win)]
    r['prio'].append(prio)
    for name, val in zip(('fifo', 'plain', 'kqcrit', 'dstrict', 'au'),
                         (m.group(4), m.group(5), m.group(6), m.group(7), m.group(10))):
        s = stat_re.match(val)
        if s:
            r[name + '50'].append(int(s.group(1)))
            r[name + '99'].append(int(s.group(2)))
    r['busy'].append(float(m.group(8)))
    r['gap'].append(float(m.group(9)))

def fmt(r, k):
    if not r[k + '50']:
        return f"{k:7s}      -"
    return f"{k:7s} {statistics.median(r[k + '50']):>7.0f} / {max(r[k + '99']):>7.0f}"

for (arm, win), r in sorted(rows.items()):
    print(f"{arm:16s} {win:3s} prio={sorted(set(r['prio']))} busy={statistics.mean(r['busy']):.0f}% "
          f"maxgap={max(r['gap']):.1f}ms")
    for k in ('fifo', 'plain', 'kqcrit', 'dstrict', 'au'):
        print("      " + fmt(r, k))
