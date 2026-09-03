#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Analyze a limina-vcpu-trace log: integrity first, then re-plug attribution.

Usage: analyze.py vcpu-trace.*.log

Integrity checks earn their place because a tuning decision will be made from this data:
sample cadence (gaps mean the sampler stalled or the guest suspended), jiffy monotonicity
(counters must never run backwards on a live kernel; a reset marks a reboot), and agreement
between the `online=` set and which cpuN fields are present.

Attribution reads the sample window just before each upward `online` transition and asks which
grow trigger (vcpu_policy.rs) could have fired: nr_running > online, loadavg1 >= online, or
neither — "neither" fingers the host-side term (worker CPU ~ online cores) or a profile floor
rise, which the guest cannot observe directly.
"""

import re
import sys
from collections import Counter


def parse_cpuset(s):
    out = set()
    for part in s.split(","):
        if "-" in part:
            a, b = part.split("-")
            out.update(range(int(a), int(b) + 1))
        elif part:
            out.add(int(part))
    return out


def parse(path):
    samples = []
    bad = 0
    with open(path) as f:
        for ln in f:
            m = re.match(
                r"(\d+) online=(\S+) nr_running=(\S+) procs_blocked=(\S+) "
                r"load=(\S+)/(\S+)/(\S+) runq=(\S+) psi_some=avg10=([\d.]+),"
                r"avg60=([\d.]+),avg300=([\d.]+),total=(\d+) psi_full=\S+(.*)",
                ln,
            )
            if not m:
                bad += 1
                continue
            cpus = dict(
                (c, tuple(int(x) for x in v.split(":")))
                for c, v in re.findall(r"(cpu\d*)=(\d+:\d+:\d+:\d+)", m.group(13))
            )
            samples.append(
                {
                    "t": int(m.group(1)),
                    "online": parse_cpuset(m.group(2)),
                    "nr_running": int(m.group(3)),
                    "load1": float(m.group(5)),
                    "load5": float(m.group(6)),
                    "psi10": float(m.group(9)),
                    "psi_total": int(m.group(12)),
                    "cpus": cpus,
                }
            )
    return samples, bad


def interval_delta(a, b):
    """Summed per-CPU (busy, idle, iowait, steal) deltas between two samples.

    Never uses the aggregate `cpu` line: /proc/stat's aggregate sums only currently-online
    CPUs, so it DROPS by a CPU's whole accumulated history at every offline — an artifact,
    not a counter reset. Per-CPU counters persist across hotplug; a CPU absent at either end
    of the interval is skipped, and a genuinely negative per-CPU delta (never yet observed)
    is skipped and counted by the caller as an anomaly.
    """
    tot = [0, 0, 0, 0]
    anomalies = 0
    for k in a["cpus"].keys() & b["cpus"].keys():
        if k == "cpu":
            continue
        d = [y - x for x, y in zip(a["cpus"][k], b["cpus"][k])]
        if any(v < 0 for v in d):
            anomalies += 1
            continue
        tot = [t + v for t, v in zip(tot, d)]
    return tot, anomalies


def busy_frac(a, b):
    """Fraction of online cpu-time spent busy between two samples, from per-CPU deltas."""
    d, _ = interval_delta(a, b)
    total = sum(d)
    return d[0] / total if total else None


def main(path):
    samples, bad = parse(path)
    n = len(samples)
    print(f"== {path}: {n} samples parsed, {bad} malformed")
    if n < 2:
        return

    # -- integrity ------------------------------------------------------------
    gaps = Counter()
    stalls = []
    anomalies = 0
    mismatches = 0
    for a, b in zip(samples, samples[1:]):
        dt = b["t"] - a["t"]
        gaps[dt] += 1
        if dt > 4:
            stalls.append((a["t"], dt))
        anomalies += interval_delta(a, b)[1]
    for s in samples:
        present = {int(k[3:]) for k in s["cpus"] if k != "cpu"}
        if present != s["online"]:
            mismatches += 1
    span = samples[-1]["t"] - samples[0]["t"]
    print(f"   span {span}s, cadence {dict(sorted(gaps.items()))}")
    print(f"   stalls>4s: {len(stalls)} {stalls[:5]}")
    print(f"   per-CPU counter anomalies: {anomalies}; online-set vs cpuN mismatches: {mismatches}")

    # -- time in state --------------------------------------------------------
    at = Counter()
    for a, b in zip(samples, samples[1:]):
        at[len(a["online"])] += b["t"] - a["t"]
    print("   time at N online:", {k: f"{v}s" for k, v in sorted(at.items())})

    # -- utilization: accumulate per-CPU deltas segment-wise ------------------
    acc = [0, 0, 0, 0]
    busy_cores_sum = 0.0  # busy jiffies per second of wall time, i.e. average busy cores
    wall = 0
    for a, b in zip(samples, samples[1:]):
        d, _ = interval_delta(a, b)
        acc = [t + v for t, v in zip(acc, d)]
        busy_cores_sum += d[0]
        wall += b["t"] - a["t"]
    total = sum(acc)
    f0, f1 = samples[0], samples[-1]
    if total:
        print(
            f"   whole-trace utilization: busy {100 * acc[0] / total:.1f}% "
            f"iowait {100 * acc[2] / total:.1f}% steal {100 * acc[3] / total:.2f}% "
            f"of ONLINE cpu-time; average busy cores {busy_cores_sum / 100 / wall:.2f}"
        )
    print(f"   PSI cpu some: total {(f1['psi_total'] - f0['psi_total']) / 1e6:.1f}s stalled over the trace")

    # -- transitions + attribution -------------------------------------------
    ups = downs = 0
    down_intervals = []
    last_down = None
    print("\n== online transitions (grow events attributed):")
    for i in range(1, n):
        a, b = samples[i - 1], samples[i]
        na, nb = len(a["online"]), len(b["online"])
        if nb == na:
            continue
        if nb < na:
            downs += 1
            if last_down is not None:
                down_intervals.append(b["t"] - last_down)
            last_down = b["t"]
            continue
        ups += 1
        last_down = None
        # the trigger fired somewhere in the ~2s before b; look at a (and i-2 for safety)
        window = samples[max(0, i - 2) : i]
        why = []
        for w in window:
            no = len(w["online"])
            if w["nr_running"] > no:
                why.append(f"nr_running {w['nr_running']}>{no}")
            if w["load1"] >= no:
                why.append(f"load1 {w['load1']}>={no}")
        why = sorted(set(why)) or ["NEITHER (host term or floor rise)"]
        util = busy_frac(a, b)
        print(
            f"   {b['t']}: {na} -> {nb}  [{'; '.join(why)}] "
            f"(load1 {a['load1']}, nr_run {a['nr_running']}, busy {100 * util:.0f}% of {na} online)"
            if util is not None
            else f"(load1 {a['load1']}, nr_run {a['nr_running']})"
        )
    print(f"\n   {ups} grows, {downs} shrink steps", end="")
    if down_intervals:
        print(
            f"; shrink step spacing min/median {min(down_intervals)}/"
            f"{sorted(down_intervals)[len(down_intervals) // 2]}s",
            end="",
        )
    print()


if __name__ == "__main__":
    for p in sys.argv[1:]:
        main(p)
