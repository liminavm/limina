# gpuscore, second run: the guard held, and the controller reads its own actuator as demand

Yesterday's workload repeated on the `GIVEBACK_FREE_CEILING` build, with both ends of the
graphics pool and the whole-VM memory sampled on one clock (15 s cadence) and a live tail on the
balloon decision trace. Phase boundaries are the driver's stated marks (`MARKS.md`).

Two results, and the second was not what the run was for.

## 1. The guard held, and the ratchet did not recur

| phase | host regions | guest blobs | footprint MB | compressed MB | balloon MB | free MB |
|---|---|---|---|---|---|---|
| 1 baseline, no Firefox | 7,810 | 98 | 9,795 | 1,543 | 18,141 | 1,108 |
| 2 Firefox open, idle | 8,833 | 114 | 11,694 | 1,535 | 17,509 | 861 |
| 3 page loaded, not started | 9,206 | 116 | 11,957 | 1,536 | 17,447 | 843 |
| 4 **benchmark running** | 28,546 | 238 | 15,191 | 2,277 | 16,856 | 808 |
| 5 post-benchmark, tab open | 11,283 | 328 | 13,661 | 2,284 | 17,134 | 713 |
| 6 tab closed | 11,107 | 326 | 13,243 | 2,271 | 17,516 | 715 |
| 7 **Firefox quit** | 7,817 | 98 | 10,487 | 2,167 | 18,273 | 719 |

Peaks: 39,274 regions (07:52:57), 332 guest blobs (07:54:33), 16,554 MB footprint (07:54:17) —
against yesterday's 64,414 / 415 / 19,688 for the same benchmark. No 40 G ratchet. Total balloon
lending across the whole run stayed under 1.5 G and fully recovered.

**Firefox quit returned everything in one sample, exactly to baseline**: blobs 326 → 98 against a
baseline of 98, regions 11,131 → 7,845 against 7,814, DRM clients 19 → 16 against 16. This is the
fourth independent route to "the graphics pool does not retain host-side", after the gdm drain,
the window A/B, and yesterday's run.

Footprint is the one counter that does not return: 10,487 MB against a 9,795 MB baseline. That
+692 MB is accounted for almost exactly by compressed memory, which grew +624 MB over the same
span (1,543 → 2,167). The working set moved into the compressor and has not been re-faulted or
scrubbed out; nothing is unaccounted.

Closing the tab returned almost nothing (328 → 326 blobs, regions flat), same as yesterday, and
the guest's own counter agrees — so that is Firefox holding, not us.

## 2. The controller reads its own actuator position as guest demand

During the heavy phase the balloon swung **16.84 → 15.39 → 16.53 G in 24 seconds** — ~1.4 G peak
to peak at roughly 1 Hz — with memory PSI flat at **zero** throughout. Two explanations fit the
eyeball, and they predict opposite orderings:

- **workload** — the benchmark allocates in ~1 Hz bursts, `free_kib` moves first, we track it.
- **self-feedback** — deflating hands pages to the guest and RAISES `free_kib`; inflating lowers
  it. So `free_kib` is partly our own actuator position fed back as a sensor.

Cross-correlating the *differences* (`lead-lag.py`; correlating levels would score high at every
lag, since both series are strongly autocorrelated):

```
 lag +0   corr(d_free, d_target) = +0.420    the correct control response
 lag +1                          = -0.661    strongest
 lag +2                          = -0.324    decaying toward zero
```

Free **lags** the target by one tick with the mechanically-expected negative sign. The
quantitative version is decisive — a purely mechanical response predicts a regression slope of
exactly −1:

```
slope d_free / d_actual = -0.977   (n = 39)
r^2                     =  0.556
residual stdev          =  174 MiB
```

**56% of the variance in the signal the controller treats as guest demand is the controller
reading back its own last operation**, at a coefficient indistinguishable from −1. The genuine
demand signal is the 174 MiB residual underneath it. The one-tick feedback term (−0.66) is
*stronger* than the same-tick control response (+0.42), which is how 1.4 G swings arise with PSI
at zero.

This reframes the 08-13 allowance-path overshoot. Releasing to a guest holding 16 GB free is not
a threshold wanting tuning; it is this loop integrating in the deflate direction.

### What this does not yet establish

39 decisions in one heavy phase is not a general claim. Before writing a fix, run the same
regression over a quiet window and over the 08-13 md5sum trace. `r^2 = 0.556` also leaves a lot
unexplained — the residual is real guest demand, and a fix must not suppress it along with the
feedback.

Fix direction (unwritten, unmeasured): subtract the known in-flight balloon delta from `free_kib`
before using it as demand, or do not decide again until the previous operation has settled.

## Traps this run walked into

**The trace has more than one record shape.** Scrub records carry no `decision` key. Formatting
one through the decision formatter prints an all-None line that reads exactly like a truncated
write — a scrub firing mid-benchmark was waved off as a `tail(1)` artifact before being caught.
`decision-tail.py` now renders them explicitly.

**The lead/lag slicing was inverted on its first run** and printed the opposite verdict with
identical confidence. The convention is now stated at the loop, and the sign is checked rather
than only the lag: our own inflate must push free *down*, so self-feedback requires a negative
correlation. A positive correlation at a positive lag is some other coupling and must not be
called self-feedback.

**The oscillation damped on its own** once the heavy phase ended. Severity is bounded; the
mechanism is not thereby innocent.

## Still open

The guard has only been shown to decline when the guest is fine. Its failure mode is the
opposite — refusing to release to a guest that genuinely is starving behind the balloon — and
nothing here put the guest under real memory shortage. That needs a workload with genuine io
pressure (a from-scratch compile), scored against a stated criterion: the guard is wrong if
sustained io-full ≥ 10% with the guest thrashing coincides with `free > 1 GiB` and a
`converged`/`cooldown` verdict instead of `giveback`.
