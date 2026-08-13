#!/usr/bin/env python3
"""A/B the io-pain fix (limina 3334ef1) from a balloon trace window.

THE DEFECT, for whoever reads this later. The settled-free cooldown lift (0a57407) let a
guest with a calm memory PSI and a large free list bypass the post-release cooldown. But the
give-back path is keyed on `io_full_avg10`, which cannot tell balloon thrash from the guest's
own disk IO -- so a guest merely doing heavy IO (io_full10 23-32%, memory PSI 0, 15.6 G free)
had its cooldown lifted the instant it was armed. Measured cost: 21 give-backs per MINUTE,
balloon oscillating 1.33 <-> 1.58 G on a 4-second period.

The fix resets the settle timer whenever io pain is present, which makes a directly
falsifiable claim:

    for every report with io_full_avg10 > IO_PRESSURE_LOW (200 = 2.00%), free_settled_ms == 0

That is what VIOLATIONS counts, and it is the whole test -- a single violation under load
means the fix is not doing what it says. Everything else here is context for judging severity:
the give-back rate against the recorded pre-fix baseline, and the balloon's swing (the
oscillation was visible as a sawtooth between two levels seconds apart).

Usage: io-giveback-ab.py <trace.jsonl> [--since-ms N] [--label TEXT]
"""
import json
import sys

IO_PRESSURE_LOW = 200  # keep in step with balloon_policy.rs:99


def main() -> int:
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 2
    path = args[0]
    since = 0
    label = "window"
    for i, a in enumerate(args):
        if a == "--since-ms" and i + 1 < len(args):
            since = int(args[i + 1])
        if a == "--label" and i + 1 < len(args):
            label = args[i + 1]

    rows = []
    for line in open(path):
        try:
            d = json.loads(line)
        except Exception:
            continue
        if "decision" not in d or d.get("ts_ms", 0) < since:
            continue
        rows.append(d)
    if not rows:
        print(f"{label}: no decision rows at or after ts {since}")
        return 1

    span_min = (rows[-1]["ts_ms"] - rows[0]["ts_ms"]) / 60000.0 or 1e-9
    io_rows = [d for d in rows if d.get("io_full_avg10", 0) > IO_PRESSURE_LOW]
    # THE assertion. A row under io pain that still reports a settled free list means the timer
    # was not reset and the cooldown can be bypassed -- the exact 2026-08-13 regression.
    violations = [d for d in io_rows if d.get("free_settled_ms", 0) != 0]

    givebacks = [d for d in rows if d["decision"] == "giveback"]
    gb_under_io = [d for d in givebacks if d.get("io_full_avg10", 0) > IO_PRESSURE_LOW]
    actual = [d.get("actual_bytes", 0) for d in rows]
    swing = (max(actual) - min(actual)) / (1 << 20) if actual else 0

    print(f"=== {label}: {len(rows)} reports over {span_min:.1f} min")
    print(f"  io pain (io_full10 > 2.00%): {len(io_rows)} reports"
          f" ({100.0 * len(io_rows) / len(rows):.0f}%),"
          f" peak {max((d.get('io_full_avg10', 0) for d in rows), default=0) / 100.0:.2f}%")
    print(f"  mem PSI some_avg10 peak:     {max((d.get('some_avg10', 0) for d in rows), default=0) / 100.0:.2f}%")
    print(f"  give-backs:                  {len(givebacks)}"
          f" ({len(givebacks) / span_min:.1f}/min, {len(gb_under_io)} of them under io pain)")
    print(f"  balloon swing:               {swing:.0f} MiB"
          f" ({min(actual) / (1 << 30):.2f} .. {max(actual) / (1 << 30):.2f} G)")
    print(f"  cooldown decisions:          {sum(1 for d in rows if d['decision'] == 'cooldown')}")
    print()
    if not io_rows:
        print("  INCONCLUSIVE: no io pain in this window -- the workload never loaded the disk"
              " hard enough to arm the defect. Re-run with a heavier or longer IO burst.")
        return 3
    if violations:
        v = violations[0]
        print(f"  FAIL: {len(violations)} reports under io pain still had a settled free list.")
        print(f"        first at ts {v['ts_ms']}: io_full10={v['io_full_avg10'] / 100.0:.2f}%"
              f" free_settled_ms={v['free_settled_ms']} decision={v['decision']}")
        return 1
    print(f"  PASS: all {len(io_rows)} reports under io pain had free_settled_ms == 0.")
    print("        Pre-fix baseline for comparison: 21 give-backs/min, 250 MiB swing on a 4 s period.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
