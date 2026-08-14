# The allowance-shortfall A/B: two field windows, one clock

Raw decision traces for the 2026-08-14 allowance-shortfall limit cycle and the fix for it.
Both are verbatim slices of the dogfood VM's `balloon-trace.jsonl` — no post-processing, so any
claim made from them can be re-derived. Analysis and conclusions live in `NOTES.md`.

**They are here because the log rotates one generation per VM start.** Each deploy pushes the
live trace to `balloon-trace.1.jsonl` and the deploy after that destroys it. Both of these
windows were already one rotation from gone when they were archived; a third bundle would have
taken the baseline with it. If you take a field measurement off this VM, copy the window out
before the next deploy or you will not get a second chance at it.

| file | window | build | what it is |
|---|---|---|---|
| `ab-baseline-prefix.jsonl` | 10:33–11:25 | pre-fix (`7994695`) | the limit cycle as found: 60 reversals >1 G in 50 min |
| `ab-band-settle1.jsonl` | 13:22–13:33 | band + damping, `SHORTFALL_SETTLE_REPORTS = 1` (`0dfd688`) | the fix's first busy window |

## What each one established

**`ab-baseline-prefix.jsonl`** — convergence parks the guest at `avail == allowance` exactly, so
every transient crosses the deflate bound. ~0.85 G guest transients drove 1.41 G deflates followed
by 14 s re-inflations, ~19 s period. Decision census is dominated by `set`/`dwell`/`dead-band`;
note there is no `shortfall` label in this file at all — allowance deflates were still labelled
`set` on this build, which is why the cycle's inflation half was invisible in the alert stream.

**`ab-band-settle1.jsonl`** — the band works and the settle was too short. Deflate size fell to
0.42 G median (from ~1.41 G), and the three that still reached ~1.1–1.2 G all occurred at
availability 1.76–1.94 G, i.e. where the band was fully pierced — band intact gives 0.4 G, band
pierced gives baseline-sized. But `shortfall` and `shortfall-damped` alternate at a 1.09 ratio
across the whole window: at `SETTLE = 1`, damping only skips every other report. Measured credit
lag (reports until `MemAvailable` reflects 90% of a release) was median 3, p90 7, n=55 — which is
what moved the constant to 3 in `1a19a5b`.

## Reproducing the numbers

`ab-score.py` re-derives everything either window is cited for:

```
python3 ab-score.py ab-baseline-prefix.jsonl
python3 ab-score.py ab-band-settle1.jsonl
```

## Reading these honestly

**The two windows are not a controlled A/B**, but they are more comparable than first claimed, and
correcting that changed a conclusion. The initial read was "the baseline ran at PSI 0.00%
throughout, the settle=1 window had real pressure", which was used to set aside both load-scaling
comparisons. The scorer shows that is wrong — the baseline's PSI *median* is 0.00% but its peaks
are **higher**, not lower:

| | memory PSI some10 med / max | io-full med / max |
|---|---|---|
| baseline (pre-fix) | 0.00% / **10.78%** | 0.78% / **16.75%** |
| band + settle=1 | 0.05% / 5.74% | 1.93% / 10.60% |

So the settle=1 window is, if anything, the *lighter* load. That removes the excuse from the two
load-scaling numbers and they have to be read as findings:

- **reversals: 68/hour → 65/hour.** Essentially unchanged. The band shrank the deflates; it did
  not stop the balloon moving.
- **release traffic: 128 G/hour → 201 G/hour.** Up ~57% on a lighter workload. At `SETTLE = 1`
  damping halved the deflate *rate* while the band cut each deflate's *size*, and the net was more
  releases, not fewer — 67 sent decreases in 11 minutes against 233 in 52. Per hour that is 365 vs
  269. Smaller releases, more of them, and each one is its own unmap + `MADV_FREE_REUSABLE` batch.

That is the case for `SHORTFALL_SETTLE_REPORTS = 3` (`1a19a5b`) and the number to re-check after
it deploys: **release traffic and sent-decrease count, not just reversal count.** If settle=3 does
not bring release traffic back toward the baseline, the damping approach is not paying for itself
and the band should be evaluated on its own.

What is *independently* solid, because it is within-window structure rather than a cross-sample
comparison: deflate size against the allowance arithmetic (median 0.450 G vs a predicted 0.450 G,
exact), and the damped:sent ratio. Those hold regardless of the workload question.

A quiet trace proves nothing here. The cycle only appears while the guest is under a compositor
dev loop; it went silent at 11:24 by itself and again at 13:33. Any future window used as evidence
needs guest pressure in it — check the PSI columns before drawing a conclusion from calm.
