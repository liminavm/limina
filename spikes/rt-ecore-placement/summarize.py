#!/usr/bin/env python3
"""Aggregate placement.c output (matrix.txt) per arm: E/P sample counts per rep, split by whether
the thread was at RT priority (>= 97) when sampled, plus timer-wake placement and lateness.

usage: summarize.py matrix.txt [--series]
"""
import re
import sys
from collections import defaultdict, OrderedDict

path = sys.argv[1]
series = "--series" in sys.argv
arms = OrderedDict()
notes = []
kv = re.compile(r"(\w+)=([^\s]+)")
for line in open(path):
    line = line.rstrip("\n")
    if line.startswith("#"):
        if "NOTE" in line or "ABORT" in line:
            notes.append(line)
        continue
    m = re.match(r"(\S+) r(\d+) (.*)$", line)
    if not m:
        continue
    arm, rep, rest = m.group(1), int(m.group(2)), m.group(3)
    a = arms.setdefault(arm, {"reps": defaultdict(list), "mon": {}, "rec": {}, "series": defaultdict(list)})
    if " kind=" in rest:
        d = dict(kv.findall(rest))
        a["reps"][rep].append(d)
    elif rest.startswith("monitor"):
        a["mon"][rep] = rest
    elif rest.startswith("recommended_cores"):
        a["rec"][rep] = rest
    elif " series " in rest:
        a["series"][rep].append(rest.split(":", 1)[1].strip())

for arm, a in arms.items():
    print(f"== {arm}")
    for rep, thrs in sorted(a["reps"].items()):
        for d in thrs:
            n = lambda k: int(d.get(k, 0))
            rt = n("E_rt") + n("P_rt")
            ts = n("E_ts") + n("P_ts")
            line = (
                f"  r{rep} thr={d['thr']} {d['kind']:5} pri={d['pri']} rtfrac={d['rtpri_frac']}"
                f" | at-RT E={n('E_rt')} P={n('P_rt')}"
                f" ({100 * n('E_rt') / rt:.1f}% E)" if rt else
                f"  r{rep} thr={d['thr']} {d['kind']:5} pri={d['pri']} rtfrac={d['rtpri_frac']} | at-RT none"
            )
            line += f" | ts E={n('E_ts')} P={n('P_ts')}"
            if ts:
                line += f" ({100 * n('E_ts') / ts:.1f}% E)"
            if n("wakeE") + n("wakeP"):
                line += f" | wake E={n('wakeE')} P={n('wakeP')}"
            if "late_us_p50" in d:
                line += f" | late p50={d['late_us_p50']} p99={d['p99']} max={d['max']}us"
            if d.get("ret_policy") != "0" or d.get("ret_qos") != "0":
                line += f" | ret_policy={d['ret_policy']} ret_qos={d['ret_qos']}"
            print(line)
        if rep in a["rec"]:
            print(f"  r{rep} {a['rec'][rep]}")
        if rep in a["mon"]:
            print(f"  r{rep} {a['mon'][rep]}")
        if series:
            for s in a["series"][rep]:
                print(f"  r{rep} series {s}")
for n in notes:
    print(n)
