#!/usr/bin/env python3
"""Stream the interesting balloon decisions out of a live balloon-trace.jsonl.

Feed it `ssh <host> tail -F .../balloon-trace.jsonl` on stdin. Every steady-state
verdict is dropped so the stream only carries the decisions that MOVE the balloon
(or explain why a move was refused for a reason worth reading): set, giveback,
gap-decay, free-exhausted, inelastic.

Written 2026-08-14 for the gpuscore allowance-path run. It exists as a file rather
than an inline -c because escaping an f-string through two shell layers is how the
first attempt died.
"""
import json
import sys
import time

QUIET = {"cooldown", "converged", "not-idle", "dead-band", "not-calm", "dwell"}

for line in sys.stdin:
    try:
        d = json.loads(line)
    except Exception:
        continue
    ts = time.strftime("%H:%M:%S", time.localtime(d.get("ts_ms", 0) / 1000))
    # The trace carries more than one record shape. Scrub records have no `decision` key at
    # all -- rendering them through the decision formatter prints an all-None line that reads
    # exactly like a truncated write, which is how a scrub firing mid-benchmark got waved off
    # as a tail(1) artifact on 2026-08-14. Handle the shape explicitly.
    if "decision" not in d:
        if "scrub" in d:
            print(
                "%s SCRUB %-6s bal=%.2fG reached=%s%% resume_pages=%s gen=%s"
                % (
                    ts,
                    d.get("scrub"),
                    d.get("actual_bytes", 0) / 2**30,
                    d.get("reached_pct"),
                    d.get("resume_pages"),
                    d.get("gen"),
                ),
                flush=True,
            )
        else:
            print("%s UNKNOWN-RECORD keys=%s" % (ts, ",".join(sorted(d))), flush=True)
        continue
    dec = d["decision"]
    if dec in QUIET:
        continue
    bal = d.get("actual_bytes", 0) / 2**30
    free = d.get("free_kib", 0) / 1024
    print(
        "%s %-14s bal=%.2fG tgt=%s free=%.0fM some60=%s io=%s host=%s"
        % (
            ts,
            dec,
            bal,
            d.get("new_target_pages"),
            free,
            d.get("some_avg60"),
            d.get("io_full_avg10"),
            d.get("host"),
        ),
        flush=True,
    )
