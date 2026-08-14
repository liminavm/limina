#!/usr/bin/env python3
"""Re-derive every number the 2026-08-14 allowance-shortfall windows are cited for.

    python3 ab-score.py ab-baseline-prefix.jsonl
    python3 ab-score.py ab-band-settle1.jsonl

See AB-README.md for what each window is and how to read the results honestly. In short: trust
the within-window structure (deflate sizes vs the allowance arithmetic, damped:sent ratio) and
distrust anything that scales with load (reversal rate, release traffic), because the two windows
carry different workloads.

Null discipline, as everywhere in this directory: a missing/None value is never coalesced to 0 in
anything reported to a human. A null `new_target_pages` means "no target commanded", and counting
it as a target of zero would invent a full release out of a hold.
"""
import collections
import datetime
import json
import statistics
import sys

G = 1 << 30
PAGE = 4096
# The dogfood VM: 24 GiB given, Moderate at host-normal => allowance = max/8.
GUEST_GIB = 24
ALLOWANCE_G = GUEST_GIB / 8
BAND_PCT = 25
# Below this, a window is too short for its load-scaling numbers to mean anything cross-sample.
SHORT_WINDOW_S = 20 * 60


def hhmmss(ms):
    return datetime.datetime.fromtimestamp(ms / 1000).strftime("%H:%M:%S")


def load(path):
    rows, sweeps = [], []
    for line in open(path):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if "decision" in d:
            rows.append(d)
        elif "sweep" in d:
            sweeps.append(d)
    return rows, sweeps


def pages_g(v):
    """Pages -> GiB, or None if the field was not reported."""
    return None if v is None else v * PAGE / G


def reversals(rows, threshold=G):
    """Direction changes in the driver's ACTUAL fill exceeding `threshold`.

    Measured on `actual_bytes`, not on the commanded target: the target can be walked down in
    many small steps that the guest experiences as one continuous movement, and the metric is
    meant to count movements the guest feels.
    """
    piv, direction, n = rows[0]["actual_bytes"], 0, 0
    for d in rows[1:]:
        delta = d["actual_bytes"] - piv
        nd = 1 if delta > 0 else (-1 if delta < 0 else 0)
        if nd and nd != direction and abs(delta) >= threshold:
            if direction:
                n += 1
            direction, piv = nd, d["actual_bytes"]
        elif nd == direction and delta:
            piv = d["actual_bytes"]
    return n


def credit_lag(rows, horizon=10):
    """Reports until MemAvailable reflects >=90% of a sent shortfall deflate.

    This is the measurement that sets SHORTFALL_SETTLE_REPORTS. `never` counts releases the
    guest consumed instead of banking — those are why the constant is the median rather than
    the p90: over-settling delays real relief to a guest that is genuinely eating memory.
    """
    lags, never = [], 0
    for i, d in enumerate(rows):
        if d["decision"] != "shortfall" or not d.get("sent"):
            continue
        if d.get("new_target_pages") is None or d.get("current_pages") is None:
            continue
        released_kib = (d["current_pages"] - d["new_target_pages"]) * 4
        base = d["avail_kib"]
        for k in range(1, horizon + 1):
            if i + k >= len(rows):
                break
            if rows[i + k]["avail_kib"] >= base + released_kib * 0.9:
                lags.append(k)
                break
        else:
            never += 1
    return lags, never


def main():
    path = sys.argv[1]
    rows, sweeps = load(path)
    if not rows:
        sys.exit(f"{path}: no decision records")
    span_s = (rows[-1]["ts_ms"] - rows[0]["ts_ms"]) / 1000
    print(f"{path}")
    print(f"  {hhmmss(rows[0]['ts_ms'])}..{hhmmss(rows[-1]['ts_ms'])}  "
          f"{span_s/60:.1f} min, {len(rows)} decisions, {len(sweeps)} sweep records")
    if span_s < SHORT_WINDOW_S:
        print(f"  !! SHORT WINDOW (<{SHORT_WINDOW_S//60} min): do NOT quote the LOAD-DEPENDENT")
        print("     numbers below against another window. An 11-minute slice of this very build")
        print("     read as release traffic +57% (a regression); the build's full 80 minutes read")
        print("     as +6% (flat). The slice had straddled a burst.")

    census = collections.Counter(d["decision"] for d in rows)
    print(f"\n  census: {dict(census.most_common())}")
    if "shortfall" not in census and "allowance-band" not in census:
        print("  (no shortfall/allowance-band labels: this is a PRE-FIX build, where allowance")
        print("   deflates were labelled `set` and were indistinguishable from every other move)")

    n = reversals(rows)
    print(f"\n  reversals >1G: {n}  ({n/(span_s/3600):.0f}/hour)   [LOAD-DEPENDENT — see README]")

    bal = [d["actual_bytes"] / G for d in rows]
    print(f"  balloon range: {min(bal):.2f}..{max(bal):.2f} G")
    rel = (rows[-1]["released_bytes"] - rows[0]["released_bytes"]) / G
    print(f"  release traffic: {rel:.1f} G ({rel/(span_s/3600):.0f} G/hour)   [LOAD-DEPENDENT]")

    # --- the load-independent half: mechanism structure ---
    sent_down = [d for d in rows
                 if d.get("sent") and d.get("new_target_pages") is not None
                 and d.get("current_pages") is not None
                 and d["new_target_pages"] < d["current_pages"]]
    by_label = collections.Counter(d["decision"] for d in sent_down)
    print(f"\n  sent target DECREASES by verdict: {dict(by_label)}")

    sf = [d for d in sent_down if d["decision"] == "shortfall"]
    if sf:
        sizes = sorted(pages_g(d["current_pages"]) - pages_g(d["new_target_pages"]) for d in sf)
        av = sorted(d["avail_kib"] / 1048576 for d in sf)
        med_size = statistics.median(sizes)
        med_av = statistics.median(av)
        print(f"  shortfall deflate size G: min {sizes[0]:.3f} med {med_size:.3f} "
              f"p90 {sizes[int(len(sizes)*.9)]:.3f} max {sizes[-1]:.3f}")
        print(f"  availability at deflate G: med {med_av:.2f}  "
              f"(allowance {ALLOWANCE_G:.2f}, inflate bound {ALLOWANCE_G*(1+BAND_PCT/100):.2f})")
        # The arithmetic self-check: a shortfall deflate should equal allowance - avail.
        print(f"  CHECK deflate == allowance - avail: {med_size:.3f} vs "
              f"{ALLOWANCE_G - med_av:.3f}  (a mismatch means the model is wrong, not the data)")
        damped = census.get("shortfall-damped", 0)
        print(f"  damped:sent = {damped}:{len(sf)} = {damped/len(sf):.2f}  "
              f"(1.00 means damping only skips every other report)")
        lags, never = credit_lag(rows)
        if lags:
            s = sorted(lags)
            print(f"  credit lag (reports): n={len(s)} med {s[len(s)//2]} "
                  f"p90 {s[int(len(s)*.9)]} max {s[-1]}; never-credited {never}")

    psi = sorted(d["some_avg10"] / 100 for d in rows)
    io = sorted(d["io_full_avg10"] / 100 for d in rows)
    print(f"\n  guest load: memory PSI some10 med {psi[len(psi)//2]:.2f}% max {psi[-1]:.2f}% | "
          f"io-full med {io[len(io)//2]:.2f}% max {io[-1]:.2f}%")
    print("  ^ CHECK THIS BEFORE CONCLUDING ANYTHING: a window with PSI ~0 throughout never")
    print("    exercised the cycle, and its calm is not evidence of a fix.")


if __name__ == "__main__":
    main()
