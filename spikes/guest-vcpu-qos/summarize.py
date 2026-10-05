#!/usr/bin/env python3
"""One row per arm, reps side by side, from measure.sh output.

    summarize.py <results-dir> [<results-dir>...]
"""
import collections
import pathlib
import re
import sys

ARMS = ["off", "band", "off+lat0", "band+lat0"]
rows = collections.defaultdict(lambda: collections.defaultdict(list))
for d in sys.argv[1:]:
    for f in sorted(pathlib.Path(d).glob("*-r[0-9].txt")):
        arm = f.name.rsplit("-r", 1)[0]
        for line in f.read_text().splitlines():
            m = re.search(r"timerlat period_us=(\d+) p50=(\d+) p90=\d+ p99=(\d+)", line)
            if m:
                rows[arm][f"tl{m[1]}_p50"].append(int(m[2]))
                rows[arm][f"tl{m[1]}_p99"].append(int(m[3]))
            m = re.search(r"dutyprobe gap_us=5000 busy_us=(\d+) chunk_ns p50=(\d+)", line)
            if m:
                rows[arm][f"duty{m[1]}"].append(int(m[2]))
            m = re.search(r"futex_wake p50=([\d.]+)us", line)
            if m:
                rows[arm]["futex"].append(float(m[1]))
            m = re.search(r"fcprobe presented: n=\d+ rate=([\d.]+)/s .* over25ms=(\d+)", line)
            if m:
                rows[arm]["fps"].append(float(m[1]))
                rows[arm]["over25"].append(int(m[2]))
            m = re.search(r"commit_to_present: p50=([\d.]+)", line)
            if m:
                rows[arm]["c2p"].append(float(m[1]))

cols = [
    ("fps", "presented fps"),
    ("over25", "frames >25 ms /10 s"),
    ("c2p", "commit→present p50 ms"),
    ("tl16667_p50", "timer 16.7 ms p50 µs"),
    ("tl16667_p99", "timer 16.7 ms p99 µs"),
    ("tl4000_p50", "timer 4 ms p50 µs"),
    ("tl1000_p50", "timer 1 ms p50 µs"),
    ("duty300", "chunk ns, 6% duty"),
    ("duty10000", "chunk ns, 67% duty"),
    ("futex", "futex wake p50 µs"),
]
arms = [a for a in ARMS if a in rows]
print("| metric | " + " | ".join(arms) + " |")
print("|---|" + "---|" * len(arms))
for key, name in cols:
    cells = []
    for a in arms:
        v = rows[a][key]
        cells.append(" / ".join(f"{x:g}" for x in v) if v else "-")
    print(f"| {name} | " + " | ".join(cells) + " |")
