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
| `ab-band-settle1.jsonl` | 13:16–14:36 | band + damping, `SHORTFALL_SETTLE_REPORTS = 1` (`0dfd688`) | the settle=1 build's entire run, boot to next deploy |

## What each one established

**`ab-baseline-prefix.jsonl`** — convergence parks the guest at `avail == allowance` exactly, so
every transient crosses the deflate bound. ~0.85 G guest transients drove 1.41 G deflates followed
by 14 s re-inflations, ~19 s period. Decision census is dominated by `set`/`dwell`/`dead-band`;
note there is no `shortfall` label in this file at all — allowance deflates were still labelled
`set` on this build, which is why the cycle's inflation half was invisible in the alert stream.

**`ab-band-settle1.jsonl`** — the band works and the settle was too short. Deflate size fell to
0.459 G median (from ~1.41 G), exactly the `allowance - avail` the model predicts, and the tail
that still reaches ~1.0–1.8 G is where the band was fully pierced — band intact gives ~0.46 G,
band pierced gives baseline-sized. But `shortfall` and `shortfall-damped` run at a 1.14 ratio
across the window: at `SETTLE = 1`, damping only skips every other report. Measured credit lag
(reports until `MemAvailable` reflects 90% of a release) is median 3, p90 8, n=266, 35
never-credited — which is what moved the constant to 3 in `1a19a5b`.

## Reproducing the numbers

`ab-score.py` re-derives everything either window is cited for:

```
python3 ab-score.py ab-baseline-prefix.jsonl
python3 ab-score.py ab-band-settle1.jsonl
```

## Reading these honestly

**The load-scaling numbers here were misread twice, in opposite directions. Both mistakes came
from the window, not the data**, and the sequence is worth keeping because it is the failure mode
this whole directory exists to guard against.

First read: "the baseline ran at PSI 0.00% throughout, the settle=1 window had real pressure", used
to set aside both load-scaling comparisons. Wrong — the baseline's PSI *median* is 0.00% but its
peaks are higher. Second read, from an 11-minute slice of the settle=1 build: release traffic
128 → 201 G/hour, reversals unchanged, called a probable regression. Also wrong. That slice was
11 minutes against the baseline's 52 and happened to straddle a burst.

The archived settle=1 file is now the build's **entire 80-minute run**, which is a fair match to
the 52-minute baseline on load:

| | memory PSI some10 med / max | io-full med / max |
|---|---|---|
| baseline (pre-fix), 52 min | 0.00% / 10.78% | 0.78% / 16.75% |
| band + settle=1, 80 min | 0.00% / 8.56% | 0.83% / 18.02% |

Against that matched pair:

- **release traffic: 128 → 136 G/hour.** Essentially flat, not up 57%.
- **reversals: 68 → 58/hour.** A modest real improvement.

So settle=1 was not a regression, and the case for `SHORTFALL_SETTLE_REPORTS = 3` (`1a19a5b`) rests
on the mechanism instead: at settle=1 the damped:sent ratio is 1.14, i.e. damping skips barely
every other report, while the measured credit lag is median 3. The damping is simply
under-applied.

**The rule this earns: a load-scaling number needs windows matched in duration AND in PSI
distribution, and a sub-20-minute window is not enough to establish either.** Within-window
structure is what survives — deflate size against the allowance arithmetic (median 0.459 G vs a
predicted 0.459 G, exact) and the damped:sent ratio held steady across every reading.

After settle=3 deploys, the numbers to check are the **damped:sent ratio (should rise toward 3)
and the sent-decrease count**, over a window of comparable length and PSI. Release traffic is a
weak instrument here — it barely moved between two builds with very different deflate behaviour.

A quiet trace proves nothing here. The cycle only appears while the guest is under a compositor
dev loop; it went silent at 11:24 by itself and again at 13:33. Any future window used as evidence
needs guest pressure in it — check the PSI columns before drawing a conclusion from calm.
