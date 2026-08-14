#!/usr/bin/env python3
"""Does the guest's free list LEAD the balloon target, or LAG it?

The 2026-08-14 gpuscore run showed the balloon swinging ~1.4 G peak-to-peak at ~1 Hz with
memory PSI flat at zero. Two explanations fit the eyeball:

  workload   -- the benchmark allocates and frees in bursts, `free_kib` moves first, and the
                controller is faithfully tracking a genuinely oscillating guest.
  self-feedback -- the controller's own actuator moves its sensor (deflating hands pages to the
                guest, RAISING free_kib; inflating lowers it), so it is chasing its own tail.

These make opposite predictions about ordering, which is what this script measures. Cross-correlate
d(free) against d(target) at a range of lags:

  peak at NEGATIVE lag  -> free moves BEFORE the target  -> workload-driven (innocent)
  peak at POSITIVE lag  -> free moves AFTER the target   -> self-feedback (our defect)

Correlating the LEVELS would be misleading -- both are strongly autocorrelated and would show a
high score at every lag. Differences are what carry the timing.

Usage: lead-lag.py <trace.jsonl> [start HH:MM:SS] [end HH:MM:SS]
"""
import json
import sys
import time


def main():
    path = sys.argv[1]
    lo = sys.argv[2] if len(sys.argv) > 2 else None
    hi = sys.argv[3] if len(sys.argv) > 3 else None

    rows = []
    for line in open(path):
        try:
            d = json.loads(line)
        except Exception:
            continue
        if "decision" not in d or d.get("new_target_pages") is None:
            continue
        ts = time.strftime("%H:%M:%S", time.localtime(d["ts_ms"] / 1000))
        if lo and ts < lo:
            continue
        if hi and ts > hi:
            continue
        rows.append((ts, d["new_target_pages"], d.get("free_kib", 0)))

    if len(rows) < 10:
        print("only %d usable rows in window -- INCONCLUSIVE" % len(rows))
        return

    dt = [rows[i][1] - rows[i - 1][1] for i in range(1, len(rows))]
    df = [rows[i][2] - rows[i - 1][2] for i in range(1, len(rows))]

    def corr(a, b):
        n = len(a)
        if n < 3:
            return None
        ma, mb = sum(a) / n, sum(b) / n
        va = sum((x - ma) ** 2 for x in a) ** 0.5
        vb = sum((x - mb) ** 2 for x in b) ** 0.5
        if va == 0 or vb == 0:
            return None
        return sum((a[i] - ma) * (b[i] - mb) for i in range(n)) / (va * vb)

    print("window %s..%s, %d decisions with a target" % (rows[0][0], rows[-1][0], len(rows)))
    print()
    print(" lag   corr(d_free, d_target)   meaning")
    best = (0, 0.0)
    # `lag` is defined as how far free LAGS the target: corr(df[i], dt[i - lag]).
    # Getting this slicing backwards inverts the entire conclusion, and did on the first
    # run of this script (2026-08-14) -- the labels said "free leads" while the arithmetic
    # was pairing a later free-change with an earlier target-change, which is the opposite.
    for lag in range(-5, 6):
        if lag > 0:
            a, b = df[lag:], dt[: len(dt) - lag]
        elif lag < 0:
            a, b = df[: len(df) + lag], dt[-lag:]
        else:
            a, b = df, dt
        c = corr(a, b)
        if c is None:
            continue
        if abs(c) > abs(best[1]):
            best = (lag, c)
        tag = "free leads" if lag < 0 else ("free lags" if lag > 0 else "same tick")
        bar = "#" * int(abs(c) * 40)
        print("%+3d    %+.3f  %-11s %s" % (lag, c, tag, bar))

    print()
    lag, c = best
    if lag > 0:
        # SIGN MATTERS as much as the lag. Inflating the balloon TAKES pages from the guest,
        # so mechanical self-feedback predicts a NEGATIVE correlation: target up -> free down
        # a tick later. A positive correlation at a positive lag is some other coupling and
        # should not be called self-feedback.
        if c < 0:
            print("STRONGEST at lag %+d, corr %+.3f (free LAGS, negative)." % (lag, c))
            print("SELF-FEEDBACK: the target moves first and free_kib moves the opposite way a")
            print("tick later -- exactly what our own inflate/deflate does to the guest's free")
            print("list. The controller is reading its own actuator position as demand.")
        else:
            print("STRONGEST at lag %+d, corr %+.3f (free LAGS, but POSITIVE)." % (lag, c))
            print("Not the self-feedback signature -- our own operation would push free the")
            print("other way. Some other coupling; investigate before naming it.")
    elif lag < 0:
        print("STRONGEST at lag %+d, corr %+.3f (free LEADS): workload-driven -- the guest" % (lag, c))
        print("really is oscillating and the controller is tracking it.")
    else:
        print("STRONGEST at lag 0: same-tick coupling, cannot separate cause from effect at")
        print("this sampling rate. Need a finer trace or an injected step to break the tie.")
    print("corr = %+.3f" % c)


main()
