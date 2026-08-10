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
on an idle guest. This is the mechanism **at rest** — under real pressure the balloon
workqueue competes with a thrashing guest for CPU, which S2 captures end-to-end — but
nobody perceives a 1.2-second mechanism as "not deflating quickly enough", so the
complaint, if real, lives upstream: **D1 detection (the ~10 s PSI avg10 EMA) and D2
transport (the ~1 s idle-tick agent cadence) are where the seconds are.** Phase 1's S2
rate sweep measures those directly.

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
- **`LIMINA_BALLOON_TRACE` is equally unproven in vivo.** S1 runs no policy, so the
  decision-trace writer has only ever executed in its unit test, never in a live
  supervisor. The first policy-driven scenario (S4 or S2) must assert the trace file is
  non-empty — same positive-control pattern as S7 for the journal.
- **Reclaim-work channel added and validated for schema, not yet for signal**
  (run `s1-1786367471`, second S1 pass — throughput reproduced a third time: inflate
  1833/1858, deflate 2460/2471 MiB/s). The sampler now records kswapd0 CPU ticks and
  `pgscan/pgsteal_{kswapd,direct}`; this run showed all zeros, credible for an idle
  guest that kept ~2 GiB free after the inflate (reclaim never fired, kswapd0 never ran
  since boot). The sampler logs its kswapd0 PID discovery and the harness warns when
  discovery fails, so an all-zero column can't hide a broken lookup — but the channel's
  positive control (columns moving under a real squeeze) lands with S2/S3.

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

---

# Phase 1 results (2026-08-10, stock-4k baseline)

Runs: `s2-1786369542`, `s4-1786370724` (first matrix invocation; killed externally
during S6-moderate), `s3-1786371932` / `s6-1786373414` / `s7-1786373164` from the
re-runs after the tmpfs and seam-race fixes (`s6-1786372311` kept for the race
forensics). All artifacts archived beside this file. limina @ `a867aa1`..`58017bc`+.

## The three questions, answered for the stock tier

1. **"Deflates too slowly" = detection, quantified.** The full release path spends
   1.5–3.5 s deciding (D1 avg10 EMA + D2 ~1 s cadence) and ~1.25 s/3 GiB executing
   (S1). The policy genuinely protects allocation bursts up to ~512 MiB/s; between
   ~1–2 GiB/s its release lands mid-burst with real casualties; above ~2 GiB/s the
   release lands *after* the burst and zram is the actual safety net. Nothing OOM'd
   anywhere — the floor holds; the cost is swap + direct-reclaim stalls in the
   detection window. Tuning target: the 1.5–3.5 s (levers §10.1/§10.3-4 of the design:
   PSI `total=` deltas, avail-slope prediction, agent fast-path) — NOT the mechanism.
2. **Inflation behavior**: healthy at idle (245 ms onset, monotone 38 s converge, zero
   reversals, 99% reclaimed) and correct on the host-pressure ladder — but the ramp is
   the bottleneck on big VMs (90 s dwell < ramp on 16 G), aggressive is strictly worse
   than moderate under load (slower first release, no cheap give-back path), light
   dumps its whole ramp on one flapped Normal sample, and moderate's allowance
   knowingly charges a 5× I/O penalty on working sets above it with no signal that
   would ever loosen it.
3. **`Out of puff`**: the driver retry loop is confirmed exactly as modeled — spam
   exists iff a target/actual gap is held; fills against cache grind at 33 MiB/s (55×
   slower than against free pages); closing the gap silences it instantly. The bench
   never provoked the *policy* into holding such a gap; the desktop-shaped steady
   state on the 16k tier (Phase 2 opener) is where H1 predicts it appears.

## S2 — allocation-rate sweep: survival is real, but above ~1 GiB/s it's zram's

3 GiB anonymous burst at rate R against a balloon pre-inflated to the policy cap
(3840 MiB on 2048..6144), real pressure relayed at agent cadence. **9/9 points
survived, zero OOM kills** — the DEFLATE_ON_OOM drop remains safe even where the
policy is outrun. Swap/direct columns are per-burst deltas:

| mode | R MiB/s | burst wall | detection | release | min avail | swapped | direct reclaim |
|------|---------|-----------|-----------|---------|-----------|---------|----------------|
| disabled | ∞ | 2.0 s | — | — | 2280 M | 0 | 0 |
| moderate | 512 | 6.18 s | **1517 ms** | dribble (−210 MiB) | 213 M | 2 M | ~0 |
| moderate | 1024 | 3.19 s | 1962 ms | to 0 | 117 M | 551 M | 250 M |
| moderate | 2048 | 1.75 s | 1831 ms | to 0 | 82 M | 1808 M | 773 M |
| moderate | ∞ | 1.51 s | 1639 ms | to 0 | 121 M | 1652 M | 619 M |
| aggressive | 512 | 6.18 s | **3541 ms** | to 0 | 117 M | 583 M | 116 M |
| aggressive | 1024 | 3.17 s | 1702 ms | to 0 | 192 M | 172 M | 61 M |
| aggressive | 2048 | 1.67 s | 2272 ms | to 0 | 101 M | 1744 M | 776 M |
| aggressive | ∞ | 1.43 s | 1612 ms | to 0 | 107 M | 1774 M | 615 M |

Findings:

1. **Detection latency is 1.5–3.5 s across the board** — squarely the D1+D2 chain
   (avg10 EMA + ~1 s report cadence), two orders of magnitude above D3+D4 (S1: the
   mechanism itself moves 3 GiB in ~1.25 s). The user-facing "deflates too slowly"
   window is this 1.5–3.5 s, plus whatever the squeeze costs in that window.
2. **At ≥2048 MiB/s the release lands AFTER the burst completes** (e.g. moderate/2048:
   burst done at 1751 ms, release at 1831 ms). Survival there is entirely zram +
   direct reclaim: ~1.7–1.8 GiB compressed out, 600–780 MiB of direct-reclaim stalls.
   "Survived" and "the policy worked" separate cleanly at this rate — the policy's real
   coverage ends somewhere between 1 and 2 GiB/s.
3. **The policy genuinely covers ≤512 MiB/s** (moderate: 2 MiB swapped, zero direct
   reclaim, burst unslowed at 6.18 s vs the 6.0 s rate floor — the below-allowance
   dribble released just enough just in time).
4. **Aggressive is measurably worse than moderate at the same rate**: at 512 MiB/s its
   first release came at 3541 ms (vs 1517 ms) and cost 583 MiB of swap (vs 2 MiB) —
   it has no cache allowance, so the cheap below-allowance give-back path never fires;
   its only exits are the panic paths. The mode sold as "maximum reclaim" is also
   maximum-pain-under-load, now with numbers.
5. `Out of puff`: **zero lines in every point** — an acute squeeze does not produce the
   dogfood spam (H2 transients didn't even register); reinforces H1 (steady-state held
   gap) as the suspect. S7 adjudicates.

## S3 — cache starvation: a sustained 5× penalty the policy calls "fine"

3 GiB working-set re-read loop under a converged moderate policy (4 GiB balloon,
~1 GiB cache allowance), 120 s; `disabled` denominator (run `s3-1786371932`, after the
tmpfs fix — the first S3 was hollow, see "instrument incidents" below):

| mode | passes | median pass | io-PSI ∫ | mem-PSI ∫ | min avail | released? |
|------|--------|-------------|----------|-----------|-----------|-----------|
| disabled | 939 | **123 ms** | 175 %·s | 0 | 5.2 G | — |
| moderate | 191 | **618 ms** | 298 %·s | 861 %·s | 1.29 G | no |

The Run-D trade reproduced in vivo: the allowance-sized cache costs **5× on a working
set 3× the allowance**, held for the full two minutes — and the policy correctly (per
its current rules) never moved: avail sat at the allowance (never below, never starved),
memory-PSI averaged ~7% (neutral band), io-PSI ~2.5%. Every threshold read "fine" while
the guest ran file I/O at one-fifth speed. **The wedge-class guards work (nothing
stuck, nothing starved); what's exposed instead is the gap between "not wedged" and
"not punished"** — there is no signal in the current policy that a sustained
cache-miss burn should loosen the balloon. (Lever candidate: an io-PSI/refault-rate
term in the give-back rule.) Zero `Out of puff` the whole time — target equalled
actual throughout; consistent with §2's requeue model.

## S6 — host-pressure staircase: the ladder works; light has a flap hazard

16 GiB max, real idle reports, `@file` level stepped normal→warn→critical→normal, 90 s
dwells. Clean run (`s6-1786373414`, after the atomic-write fix; step-end targets):

| mode | normal | warn | critical | normal (recovery) |
|------|--------|------|----------|-------------------|
| light | **0** | 8960 M | 14336 M (full room) | **0** |
| moderate | 8960 M (ramp-limited) | 14329 M | 14329 M (already at floor) | 13230 M (gave back to the 2 G allowance) |

Both ladders engage and disengage correctly, and the injected level provably reached
the policy (the trace's host field follows the file). Light@warn reproduced at exactly
8960 MiB across three runs.

Two findings (the second from the first, racy pass — run `s6-1786372311`):

1. **The 90 s dwell is shorter than the ramp**: at 256 MiB/2 s the policy moves at most
   ~11 GiB per step window, so large-VM staircase steps are ramp-limited, not
   allowance-limited. Fine for direction-checking; convergence measurements need longer
   dwells or a faster inflate step.
2. **Light has no hysteresis across host-level transitions — one flapped `normal`
   sample discards the entire ramp.** Observed via an instrument race (the harness's
   level-file rewrite wasn't atomic; the policy sampled the truncation window, read
   empty → Normal): light released its full 8960 MiB instantly, then critical re-ramped
   from zero. The race is fixed (temp+rename), but the exposure is real: the *actual*
   sysctl blend can flap Warn→Normal for one sample (availability hovering at the 40%
   boundary), and light will dump minutes of reclaim progress and restart a 90+ s ramp.
   (Lever candidate: debounce host-level *improvements* — demotions can stay instant.)

## S7 — the `Out of puff` mechanism, adjudicated

4 GiB guest, no policy, cache-filled to the H1 shape (free 218 MiB / avail 3.4 GiB —
achieved exactly as §2 predicts): run `s7-1786373164`.

- **Phase B (2 GiB target into the cache-full guest): 33 MiB/s — 55× slower than
  S1's 1840 MiB/s against free pages.** `__GFP_NORETRY`'s light reclaim grinds through
  cache but barely. Zero puff lines: a *moving* fill, even a grinding one, doesn't spam.
- **Phase C (chase to the ceiling): 10 puff lines in 90 s** while a gap was held — the
  **journal channel's positive control, passed** (counts are a ratelimit floor). The
  kswapd channel also went live: 606 ticks (~6 s of kswapd CPU). Notably the driver
  eventually filled even RAM−512 MiB — given time, the 5 Hz retry loop + zram grinds
  through everything; "unfillable" is really "fillable at 33 MiB/s with spam".
- **Phase D (gap closed): 0 lines in 30 s.** (Precision: the driver had already ground
  its way to the final 3584 MiB target by the end of C — `plateau_actual ==
  chase_final_target` in the metrics — so D verified that silence *persists* once
  target equals actual rather than performing the close itself.) Jointly, C+D confirm
  the §2 requeue model: spam exists exactly while `target > actual` is held, and only
  then.

### H1/H2/H3 adjudication

- **H2 (benign transients) is dead as an explanation for the dogfood spam**: 14 bench
  scenarios of acute squeezes, ramps, and thrash produced *zero* puff lines except when
  a target/actual gap was deliberately held. Transients don't even register at the
  ratelimit.
- **H1's mechanism is confirmed** (cache-full ⇒ free-starved fills grind at 33 MiB/s;
  held gap ⇒ steady ratelimited spam; closed gap ⇒ instant silence). What Phase 1 did
  NOT capture is the *policy* holding such a gap organically — S4's idle converge had
  0% gap residency, and S2's squeezes closed their gaps by releasing. The missing
  vehicle is the **desktop-shaped steady state** (a warm-cache guest under moderate
  with ongoing light activity, tens of minutes — the deferred S7 long variant): H1
  predicts the policy will park the target above what the driver can fill from free
  pages and sit there. That run, on the enhanced 16k tier, is the Phase-2 opener.
- **H3 (gates holding a stale target)** remains plausible as a contributor and is now
  directly observable via `gap_residency` + the trace's hold reasons in any future run.

## Instrument incidents (tuition paid, guards added)

1. **The tmpfs trap.** The first S3 went green while measuring nothing: Fedora's `/tmp`
   is a RAM-backed tmpfs sized RAM/2, so the 3 GiB working-set file filled it — the
   workload's pass log ENOSPC'd (0 passes) and the sampler (CSV also in `/tmp`) died of
   the same ENOSPC 9 s in, on both points. tmpfs pages aren't reclaimable cache either,
   so the scenario couldn't have measured cache starvation even if it had run. Fixes:
   big files on disk-backed `/var/tmp` with `df` guards, `passes > 0` asserted, and the
   sampler now **fails the run if its last row is stale** — hollow time series can't go
   green again.
2. **The seam-write race** (see S6 finding 2): non-atomic level-file rewrite → one
   empty read → pinned Normal → light dumped its ramp. temp+rename in the harness.
3. **S7's first sizing tripped its own disk guard** (4.6 GiB file vs 4.45 GiB free):
   resized to a 4 GiB guest + 3 GiB file, which also produces a cleaner H1 shape.
4. The first matrix invocation was **killed externally mid-S6** (SIGKILL to the worker;
   source unidentified); S2/S4/S6-light artifacts survived, the rest re-ran. Worth
   knowing: a killed nextest leaves no orphan VMs — teardown held.

## S4 — idle-inflate convergence: the inflate side is healthy

Moderate, honest closed loop (real reports, avail shrinks as the balloon grows),
6144 MiB max: onset **245 ms** after relay start (the calm gates don't hold an idle
guest), **16 × 256 MiB steps to the full 4 GiB room in 38.4 s** (an idle 6 GiB guest's
avail exceeds allowance+room, so full room is the correct fixpoint), **0 reversals**
over a 5-minute stability window, **0% gap residency**, **0 `Out of puff` lines**,
99.2% of ballooned bytes madvised (`reclaimed=` 4.26 G / 4.29 G). Median consumed-report
gap 1268 ms (the relay's ~1 s + ssh overhead). No limit-cycle residue; the oscillation
regressions stay dead. This run is also `LIMINA_BALLOON_TRACE`'s in-vivo positive
control (16 sent changes journaled) — the trace channel is now proven.
