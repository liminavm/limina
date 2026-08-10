# Balloon bench — Phase 0 results (2026-08-10)

Instrument: `docs/design/balloon-bench.md`. This file accumulates the bench's measured
results; raw artifacts sit beside it per run (`s1-<unix-secs>/`: guest.csv, host.csv,
journal.txt, metrics.json).

## S1 — mechanism deflate/inflate (no policy), stock-4k baseline

Run: `s1-1786366404` (limina @ `081881b`). Stock F44 baseline, 6144 MiB RAM, idle guest,
two direct-target 3 GiB inflate→deflate cycles at the worker's balloon socket — the PSI
policy was **not** running. Host poll 200 ms, in-guest sampler 250 ms.

| leg | cycle 0 | cycle 1 |
|-----|---------|---------|
| inflate 3 GiB | 1.66 s = **1840 MiB/s** | 1.65 s = **1860 MiB/s** |
| deflate 3 GiB | 1.25 s = **2464 MiB/s** | 1.24 s = **2475 MiB/s** |

`Out of puff` lines: **0** (idle guest, free pages plentiful — expected).

### The headline: D4 is not the bottleneck

The first-ever deflate-throughput number says the guest driver hands back 3 GiB in
**~1.25 s**, reproducibly (cold and warm cycles within 0.5%). Inflate fills at ~1.8 GiB/s
on an idle guest. Nobody perceives a 1.2-second mechanism as "not deflating quickly
enough" — so the complaint, if real, lives upstream: **D1 detection (the ~10 s PSI avg10
EMA) and D2 transport (the ~1 s idle-tick agent cadence) are where the seconds are.**
Phase 1's S2 rate sweep measures those directly.

(Resolution note: numbers are quantized by the 200 ms host poll — legs complete between
samples, so true throughput is slightly higher than quoted. Immaterial at this scale.)

### Secondary observations

- **`reclaimed=` tracked ~99% of ballooned bytes** (3.20 of 3.22 GB madvised on cycle 0;
  cumulative 6.42 GB after cycle 1) — host-side coalescing loses almost nothing even on
  the worst-case 4k guest *when the guest is fresh and its free pages are contiguous*.
  Fragmented steady-state guests are the case the two-tier docs call best-effort; measure
  before generalizing.
- **Worker `phys_footprint` barely moved** (−252 MiB on the first inflate, −38 MiB on the
  second, flat on deflates) against 3.2 GB madvised. Not a contradiction: a freshly
  booted idle guest has never faulted most of that RAM, so the madvise had nothing
  resident to release — `reclaimed=` counts bytes *madvised*, not bytes *returned*. The
  I4 "MiB the host really got back" metric is only meaningful against a **dirtied**
  guest. Follow-up: S1 needs a `dirty-first` variant (touch N GiB in the guest, free it,
  then balloon) before the I4 number means anything.
- **The kernel-journal channel returned empty and has no positive control yet.** Zero
  balloon lines is *plausible* for this run (the driver only speaks on failure), but an
  empty grep also looks exactly like a broken pipeline. S7 (unreachable-target chase)
  forcibly generates `Out of puff` lines and is the channel's validation — treat
  journal-based conclusions as unproven until it runs.

### Instrument status after Phase 0

All four §3 gaps are shipped (`7d92cb2`, `6c7d3a7`) and the recorder pipeline produced
complete artifacts end-to-end on the first run (103 guest samples / 32 host samples /
metrics.json). Bench gating works: the scenario is invisible to the default suite and
runs via

```
LIMINA_BALLOON_BENCH=1 scripts/test-boot.sh debug --no-capture -E 'test(s1_mechanism_deflate_inflate)'
```

Next (Phase 1): S2 allocation-rate sweep per mode (the max-survivable-rate headline),
S3 cache starvation, S4 idle-inflate convergence, S6 host-pressure staircase via
`LIMINA_HOST_PRESSURE`, S7 for `Out of puff` + journal-channel validation.
