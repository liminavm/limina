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

# A HOLD verdict repeats every tick for as long as its condition lasts. `inelastic` on a
# settled guest emits ~1/s forever and drowns the stream (it flooded the monitor on
# 2026-08-14). Collapse consecutive repeats of the same verdict: print the first, then stay
# silent until the verdict changes or the balloon actually moves.
REPEAT_MOVE_BYTES = 64 * 2**20
_last = {"dec": None, "bal": 0}


def is_repeat(dec, actual_bytes):
    same = dec == _last["dec"] and abs(actual_bytes - _last["bal"]) < REPEAT_MOVE_BYTES
    if not same:
        _last["dec"], _last["bal"] = dec, actual_bytes
    return same

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
        elif "sweep" in d:
            print(
                "%s SWEEP  %-8s debited=%.0fM gap=%.0fM"
                % (
                    ts,
                    d.get("sweep"),
                    d.get("debited_bytes", 0) / 2**20,
                    d.get("gap_bytes", 0) / 2**20,
                ),
                flush=True,
            )
        else:
            # Print the shape rather than formatting it as a decision. A THIRD record type
            # (sweep debits) turned up an hour after the scrub one, and this branch is what
            # caught it instead of it reading as another truncated write.
            print("%s UNKNOWN-RECORD keys=%s" % (ts, ",".join(sorted(d))), flush=True)
        continue
    dec = d["decision"]
    if dec in QUIET:
        continue
    if is_repeat(dec, d.get("actual_bytes", 0)):
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
