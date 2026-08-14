#!/usr/bin/env python3
"""Full-fidelity live watch over a balloon-trace.jsonl. NOTHING is filtered out.

Feed it `ssh <host> tail -F .../balloon-trace.jsonl` on stdin.

Why this exists alongside `decision-tail.py`: that tool drops a QUIET set of verdicts
(cooldown/converged/not-idle/dead-band/not-calm/dwell) and collapses consecutive repeats,
to keep a human-readable stream. That is a reasonable alert stream and a BAD forensic
record -- on 2026-08-14 the churn under investigation turned out to be a limit cycle whose
inflation half is `set`/`dwell`/`dead-band` ticks, i.e. two of its three phases live in the
suppressed set. A monitor must never make a state invisible.

So: every record is rendered, one line each, and once a minute a CENSUS line reports the
count of EVERY label seen in that minute plus the balloon's range. If a label is rare it is
still in the stream; if a label is spammy it is still counted. Reversals in the balloon's
ACTUAL fill above REVERSAL_BYTES are called out on their own REV line, because the reversal
count is the metric the churn investigation is keyed on.

Null discipline follows decision-tail.py: `num()` for arithmetic, `mib()` for display, and a
missing/None value always renders as `?` -- never as 0. (A null `debited_bytes` on a sweep
START record was read as a zero-yield sweep once already.)
"""
import collections
import json
import sys
import time

REVERSAL_BYTES = 1 << 30
CENSUS_SECONDS = 60
G = float(1 << 30)


def num(d, key):
    v = d.get(key)
    return 0 if v is None else v


def mib(d, key):
    v = d.get(key)
    return "?" if v is None else "%.0fM" % (v / 2**20)


def gib(d, key):
    v = d.get(key)
    return "?" if v is None else "%.2fG" % (v / G)


def pct(d, key):
    """PSI fields are hundredths of a percent in the trace."""
    v = d.get(key)
    return "?" if v is None else "%.2f" % (v / 100.0)


class Census:
    def __init__(self):
        self.start = None
        self.reset(None)

    def reset(self, t):
        # `start` is seeded from the FIRST record's own timestamp, not wall clock: replaying a
        # captured trace through this tool must produce the same census boundaries as the live
        # stream did, and seeding from time.time() makes every replayed window negative-length.
        self.start = t
        self.labels = collections.Counter()
        self.lo = None
        self.hi = None
        self.sent_up = 0
        self.sent_down = 0
        self.reversals = 0

    def add(self, label, bal):
        self.labels[label] += 1
        self.lo = bal if self.lo is None else min(self.lo, bal)
        self.hi = bal if self.hi is None else max(self.hi, bal)

    def line(self, t):
        lab = " ".join("%s=%d" % kv for kv in self.labels.most_common())
        span = "?" if self.lo is None else "%.2f..%.2fG" % (self.lo / G, self.hi / G)
        return "%s CENSUS %ds bal %s up=%d down=%d rev=%d | %s" % (
            time.strftime("%H:%M:%S", time.localtime(t)),
            int(t - self.start),
            span,
            self.sent_up,
            self.sent_down,
            self.reversals,
            lab or "(no decisions)",
        )


def main():
    census = Census()
    # Reversal tracking on the ACTUAL fill: keep the running extreme in the current
    # direction and call a reversal when the fill moves REVERSAL_BYTES back the other way.
    piv = None
    direction = 0
    prev_target = None

    for line in sys.stdin:
        try:
            d = json.loads(line)
        except Exception:
            print("UNPARSEABLE %r" % line[:120], flush=True)
            continue
        ts_ms = num(d, "ts_ms")
        t = ts_ms / 1000.0
        ts = time.strftime("%H:%M:%S", time.localtime(t))
        if census.start is None:
            census.start = t

        if "decision" not in d:
            if "scrub" in d:
                print(
                    "%s SCRUB  %-8s bal=%s reached=%s%% resume_pages=%s gen=%s trigger=%s"
                    % (
                        ts,
                        d.get("scrub"),
                        gib(d, "actual_bytes"),
                        d.get("reached_pct"),
                        d.get("resume_pages"),
                        d.get("gen"),
                        d.get("trigger"),
                    ),
                    flush=True,
                )
                census.labels["SCRUB:" + str(d.get("scrub"))] += 1
            elif "sweep" in d:
                print(
                    "%s SWEEP  %-8s debited=%s gap=%s"
                    % (ts, d.get("sweep"), mib(d, "debited_bytes"), mib(d, "gap_bytes")),
                    flush=True,
                )
                census.labels["SWEEP:" + str(d.get("sweep"))] += 1
            else:
                print("%s UNKNOWN-RECORD keys=%s" % (ts, ",".join(sorted(d))), flush=True)
                census.labels["UNKNOWN"] += 1
            continue

        dec = d["decision"]
        actual = num(d, "actual_bytes")
        census.add(dec, actual)

        tgt = d.get("new_target_pages")
        if d.get("sent") and tgt is not None and prev_target is not None:
            if tgt > prev_target:
                census.sent_up += 1
            elif tgt < prev_target:
                census.sent_down += 1
        if tgt is not None:
            prev_target = tgt

        # Reversal detection on the actual fill.
        if piv is None:
            piv = actual
        else:
            delta = actual - piv
            nd = 1 if delta > 0 else (-1 if delta < 0 else 0)
            if nd and nd != direction and abs(delta) >= REVERSAL_BYTES:
                if direction != 0:
                    census.reversals += 1
                    print(
                        "%s REV    %.2fG -> %.2fG (%+.2fG)" % (ts, piv / G, actual / G, delta / G),
                        flush=True,
                    )
                direction = nd
                piv = actual
            elif nd == direction and delta:
                piv = actual

        total = num(d, "total_kib")
        availp = "?" if not total else "%.1f" % (100.0 * num(d, "avail_kib") / total)
        print(
            "%s %-14s tgt=%s->%s act=%s free=%s avail=%s%% "
            "s10=%s s60=%s io10=%s host=%s cd=%s pf=%s comp=%s rel=%s swp=%s"
            % (
                ts,
                dec,
                ("?" if d.get("current_pages") is None else "%.2fG" % (d["current_pages"] * 4096 / G)),
                ("-" if tgt is None else "%.2fG%s" % (tgt * 4096 / G, "" if d.get("sent") else "(unsent)")),
                gib(d, "actual_bytes"),
                mib(d, "free_kib") if d.get("free_kib") is None else "%.0fM" % (d["free_kib"] / 1024),
                availp,
                pct(d, "some_avg10"),
                pct(d, "some_avg60"),
                pct(d, "io_full_avg10"),
                d.get("host"),
                d.get("cooldown_active"),
                gib(d, "footprint_bytes"),
                gib(d, "compressed_bytes"),
                gib(d, "released_bytes"),
                d.get("sweeps"),
            ),
            flush=True,
        )

        if census.start is None:
            census.start = t
        if t - census.start >= CENSUS_SECONDS:
            print(census.line(t), flush=True)
            census.reset(t)


if __name__ == "__main__":
    main()
