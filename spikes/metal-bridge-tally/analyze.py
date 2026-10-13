#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Reduce a run-tally.sh output dir to one table per workload.

A report window counts for a workload only when it lies wholly inside that workload's
[start, end] (window start = host stamp of the report line minus the window length), so
start-up and the gaps between workloads never mix in. Rates are means over those windows.

Usage: analyze.py <out dir> [--top N]
"""
import collections
import re
import sys
from pathlib import Path

KK_WIN = re.compile(r"^(\S+) \[kk-tally\] window ([\d.]+)s: ([\d.]+) msg/s, wire ([\d.]+) B/s "
                    r"\(([\d.]+) B/msg\), reply ([\d.]+) B/s, sync ([\d.]+)/s, bridge ([\d.]+) thread-s/s")
KK_CLASS = re.compile(r"^\S+ \[kk-tally\]   class  (\w+)\s+([\d.]+)/s wire\s+([\d.]+) B/s "
                      r"reply\s+([\d.]+) B/s\s+([\d.]+) thread-s/s")
KK_FN = re.compile(r"^\S+ \[kk-tally\]   (fn|calls|bytes|time|sync)\s+(\S+)\s+(\w+)\s+([\d.]+)/s "
                   r"wire\s+([\d.]+) B/s reply\s+([\d.]+) B/s\s+([\d.]+) thread-s/s\s+([\d.]+) us/call")
VENUS = re.compile(r"^(\S+) \[virglrs\] venus (ring|context): ([\d.]+) us/cmd ([\d.]+) cmd/batch "
                   r"([\d.]+) cmd/s ([\d.]+) B/cmd ([\d.]+) thread-s/s \(n=\d+ cmd in \d+ batch over ([\d.]+)s")
# Sync-class calls that block on the GPU rather than ask Metal for a value: in a guest KK these
# are waits on a host-signalled fence, not request/reply round trips.
WAITS = {"mtl_shared_event_wait_until_signaled_value", "mtl_command_queue_wait_for_drawable"}
# Calls that only fill in a descriptor or an options object: a protocol would carry the finished
# object in the one message that consumes it (encoder begin, pipeline/sampler/state create, commit).
BUILDER = re.compile(r"_descriptor_(set|get)_|^mtl_set_max_buffer_binding_count$"
                     r"|^mtl_commit_options_add_feedback_handler$")
VREND = re.compile(r"^(\S+) \[virglrs\] vrend submit: ([\d.]+) cmd/s\s+([\d.]+) dw/s\s+([\d.]+) batch/s\s+"
                   r"([\d.]+) us/cmd\s+([\d.]+)% of wall.*over ([\d.]+)s")


def main():
    out = Path(sys.argv[1])
    top = int(sys.argv[sys.argv.index("--top") + 1]) if "--top" in sys.argv else 12
    marks = [(w, float(a), float(b)) for w, a, b in
             (l.split("\t") for l in (out / "marks.tsv").read_text().splitlines() if l.strip())]

    def owner(stamp, secs):
        for w, a, b in marks:
            if a <= stamp - secs and stamp <= b:
                return w
        return None

    kk = collections.defaultdict(list)       # workload -> [window dict]
    venus = collections.defaultdict(list)    # workload -> [(origin, cmd/s, B/s, thread-s/s)]
    vrend = collections.defaultdict(list)    # workload -> [(cmd/s, B/s, %wall)]
    cur = None
    for line in (out / "worker.stamped.log").read_text(errors="replace").splitlines():
        if m := KK_WIN.match(line):
            stamp, secs = float(m[1]), float(m[2])
            w = owner(stamp, secs)
            cur = None
            if w:
                cur = {"msg": float(m[3]), "wire": float(m[4]), "reply": float(m[6]),
                       "sync": float(m[7]), "bridge": float(m[8]), "query": 0.0,
                       "folded": 0.0, "full": False,
                       "class": {}, "fn": {}}
                kk[w].append(cur)
        elif cur is not None and (m := KK_CLASS.match(line)):
            cur["class"][m[1]] = (float(m[2]), float(m[3]), float(m[5]))
        elif cur is not None and (m := KK_FN.match(line)):
            if m[1] == "fn":
                cur["full"] = True
            elif cur["full"]:
                continue
            cur["fn"][m[2]] = (m[3], float(m[4]), float(m[5]), float(m[7]), float(m[8]))
            if m[3] == "sync" and m[2] not in WAITS:
                cur["query"] += float(m[4])
            if m[3] not in ("local", "diag") and not BUILDER.search(m[2]):
                cur["folded"] += float(m[4])
        elif m := VENUS.match(line):
            if w := owner(float(m[1]), float(m[8])):
                venus[w].append((m[2], float(m[5]), float(m[5]) * float(m[6]), float(m[7])))
        elif m := VREND.match(line):
            if w := owner(float(m[1]), float(m[7])):
                vrend[w].append((float(m[2]), float(m[3]) * 4, float(m[6])))

    def mean(xs):
        return sum(xs) / len(xs) if xs else 0.0

    print(f"# {out}\n")
    print("| workload | windows | KK msg/s | KK wire KB/s | B/msg | KK sync/s | KK bridge thread-s/s "
          "| venus cmd/s | venus ring+ctx KB/s | venus thread-s/s | vrend cmd/s | vrend KB/s "
          "| msg per venus+vrend cmd | KK wire / stream bytes | KK query round trips/s "
          "| KK folded msg/s | folded msg per cmd |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for w, _, _ in marks:
        ws = kk.get(w, [])
        n = len(ws)
        msg, wire = mean([x["msg"] for x in ws]), mean([x["wire"] for x in ws])
        sync, bridge = mean([x["sync"] for x in ws]), mean([x["bridge"] for x in ws])
        # venus reports ring and context separately per window; sum per window, then average.
        vwin = len([v for v in venus.get(w, []) if v[0] == "ring"]) or 1
        vcmd = sum(v[1] for v in venus.get(w, [])) / vwin
        vbytes = sum(v[2] for v in venus.get(w, [])) / vwin
        vbusy = sum(v[3] for v in venus.get(w, [])) / vwin
        rcmd = mean([r[0] for r in vrend.get(w, [])])
        rbytes = mean([r[1] for r in vrend.get(w, [])])
        cmds, bytes_ = vcmd + rcmd, vbytes + rbytes
        print(f"| {w} | {n} | {msg:,.0f} | {wire / 1e3:,.1f} | {wire / msg if msg else 0:.1f} "
              f"| {sync:,.1f} | {bridge:.3f} | {vcmd:,.0f} | {vbytes / 1e3:,.1f} | {vbusy:.3f} "
              f"| {rcmd:,.0f} | {rbytes / 1e3:,.1f} "
              f"| {msg / cmds if cmds else 0:.2f} | {wire / bytes_ if bytes_ else 0:.2f} "
              f"| {mean([x['query'] for x in ws]):,.1f} "
              f"| {mean([x['folded'] for x in ws]):,.0f} "
              f"| {mean([x['folded'] for x in ws]) / cmds if cmds else 0:.2f} |")

    for w, _, _ in marks:
        ws = kk.get(w, [])
        if not ws:
            continue
        print(f"\n## {w} ({len(ws)} windows)\n")
        classes = collections.defaultdict(lambda: [0.0, 0.0, 0.0])
        for x in ws:
            for c, v in x["class"].items():
                for i in range(3):
                    classes[c][i] += v[i] / len(ws)
        print("| class | calls/s | wire KB/s | thread-s/s |\n|---|---|---|---|")
        for c, (calls, wire, t) in sorted(classes.items(), key=lambda kv: -kv[1][0]):
            print(f"| {c} | {calls:,.0f} | {wire / 1e3:,.1f} | {t:.3f} |")
        fns = collections.defaultdict(lambda: [None, 0.0, 0.0, 0.0])
        for x in ws:
            for f, (cls, calls, wire, t, _) in x["fn"].items():
                fns[f][0] = cls
                fns[f][1] += calls / len(ws)
                fns[f][2] += wire / len(ws)
                fns[f][3] += t / len(ws)
        print(f"\n| function (top {top} by calls; r1-r2 logged only top lists, so their rows undercount) | class "
              "| calls/s | wire KB/s | thread-s/s |\n|---|---|---|---|---|")
        for f, (cls, calls, wire, t) in sorted(fns.items(), key=lambda kv: -kv[1][1])[:top]:
            print(f"| {f} | {cls} | {calls:,.0f} | {wire / 1e3:,.1f} | {t:.3f} |")


if __name__ == "__main__":
    main()
