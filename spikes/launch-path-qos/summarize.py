#!/usr/bin/env python3
"""Aggregate lpq result lines: one row per context x policy x busy, reps side by side.

    summarize.py <results-dir>

Lateness: per-rep p50 and p99 (µs), and over-2 ms / over-8 ms counts summed over reps.
Speed: share of chunks on an E-core and the median chunk time (ns) over all reps' medians.
"""
import collections
import pathlib
import re
import statistics
import sys

rows = collections.defaultdict(list)
for f in sorted(pathlib.Path(sys.argv[1]).glob("*-r*.txt")):
    for line in f.read_text().splitlines():
        if " result " not in line:
            continue
        lbl = line.split()[0]
        ctx = lbl.rsplit("-r", 1)[0]
        kv = dict(re.findall(r"(\w+)=([\w./\-]+)", line))
        late = dict(re.findall(r"(p50|p90|p99|max)=([\d.]+)", line.split("late_us")[1].split("chunk_ns")[0]))
        rows[(ctx, kv["policy"], int(kv["busy_us"]))].append(
            dict(
                p50=float(late["p50"]),
                p99=float(late["p99"]),
                max=float(late["max"]),
                o2=int(kv["over2ms"]),
                o8=int(kv["over8ms"]),
                onE=float(kv["onE"]),
                med=float(kv["med"]),
                rtp=int(kv["rtpri_periods"]),
                n=int(kv["periods"]),
                join=kv["rc_join"],
            )
        )

ctx_order = ["shell", "app", "job", "jobLT"]
pol_order = ["default", "utility", "ui", "lat0", "critical", "wg", "wgjoin", "rt", "wgrt"]
busies = sorted({k[2] for k in rows})
for busy in busies:
    print(f"\n## busy {busy} us per 16.667 ms ({100 * busy / 16667:.0f}% duty)\n")
    print("| policy | context | p50 µs (reps) | p99 µs (reps) | max µs | >2 ms | >8 ms | on E | chunk ns (reps) |")
    print("|---|---|---|---|---|---|---|---|---|")
    for pol in pol_order:
        for ctx in ctx_order:
            rs = rows.get((ctx, pol, busy))
            if not rs:
                continue
            n = sum(r["n"] for r in rs)
            print(
                f"| {pol} | {ctx} | {' / '.join(f'{r['p50']:.0f}' for r in rs)} "
                f"| {' / '.join(f'{r['p99']:.0f}' for r in rs)} | {max(r['max'] for r in rs):.0f} "
                f"| {sum(r['o2'] for r in rs)}/{n} | {sum(r['o8'] for r in rs)}/{n} "
                f"| {statistics.mean(r['onE'] for r in rs):.2f} "
                f"| {' / '.join(f'{r['med']:.0f}' for r in rs)} |"
            )
