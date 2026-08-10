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
   exists iff a target/actual gap is open, at the retry loop's ~5 Hz; closing the gap
   silences it instantly. *(Corrected in Phase 2: the "33 MiB/s grind, 55× slower
   than free pages" originally reported here was an instrument artifact — fills
   against warm cache run at ~2.4 GiB/s. See the S7 correction block.)* The bench
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

> **CORRECTION (Phase-2 autopsy).** This section originally reported phase B as a
> "33 MiB/s grind — 55× slower than against free pages". That was an instrument
> artifact: the harness divided the last sample's `actual` by the fixed 60 s window,
> and the fill had completed in **0.84 s** (2048/60 ≈ 33 is the arithmetic's
> fingerprint — every run "measured" it exactly). The archived `host.csv` timeline is
> unambiguous; the raw CSVs were always right. Everything below is the corrected
> reading, and it also corrects phase C, which the same lens misread as a 90 s
> held-gap plateau.

- **Phase B (2 GiB target into the cache-full guest): filled in 0.84 s (~2.4 GiB/s)**
  — warm cache is *not* a meaningfully slower source than free pages
  (`__GFP_NORETRY`'s light direct reclaim keeps up at GiB/s). Zero puff lines. There
  is no slow-grind state: the driver fills fast or it fails.
- **Phase C (chase to the ceiling): the driver took everything, in seconds** — 3072
  closed in 0.4 s, 3328 in 0.2 s, the final RAM−512 step (3584) in 2.5 s. The journal
  puts all **10 puff lines inside that final 2.5 s stall** (~4.6 lines/s — the §2
  5 Hz retry loop, visible verbatim; the ratelimit never even engaged), then silence
  for the remaining ~85 s because the gap had *closed*, not held. The **journal
  positive control passed** — spam exists exactly while `target > actual` — but the
  "natural plateau" this phase was built to find does not exist even on a bare 4 GiB
  console: kswapd (606 ticks, ~6 s CPU) + zram surrender everything up to RAM−512
  within seconds of it being asked for.
- **Phase D (gap closed): 0 lines in 30 s** — trivially, since C had already closed
  itself; D still pins the requeue model's silence direction. Jointly, C+D confirm
  §2: spam exists exactly while `target > actual` is open, and only then.

### H1/H2/H3 adjudication

- **H2 (benign transients) is dead as an explanation for the dogfood spam**: 14 bench
  scenarios of acute squeezes, ramps, and thrash produced *zero* puff lines except when
  a target/actual gap was deliberately held. Transients don't even register at the
  ratelimit.
- **H1 is sharpened, not confirmed as originally worded**: its "free-starved fills
  grind slowly" half is **FALSE** (fills run at GiB/s from any reclaimable source —
  see the correction above); its "held gap ⇒ steady ~5 Hz spam; closed gap ⇒ instant
  silence" half is **CONFIRMED**. The dogfood hours-of-spam state therefore requires
  a target the driver can *never* fill — a permanently open gap against genuinely
  unreclaimable memory — not a slow fill in progress. What Phase 1 did NOT capture is
  the *policy* holding such a gap organically — S4's idle converge had 0% gap
  residency, and S2's squeezes closed their gaps by releasing. The missing vehicle is
  the **desktop-shaped steady state** (a warm-cache guest under moderate with ongoing
  light activity, tens of minutes): H1 predicts the policy parks the target above
  what the driver can extract and sits there. That run, on the enhanced 16k tier, is
  the Phase-2 opener.
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

# Phase 2 results (2026-08-10, enhanced 16k tier + edges)

Runs: `s0enh-1786376713`, `s1enh-1786377277`, `s2enh-1786377332`, `s4enh-1786377583`
(first enhanced matrix), `s7enh-1786378025` (guest-wedge forensics, see incidents) —
remaining runs appended below as they land. Guest: the F44 enhanced test golden,
EFI-booted on its own `7.1.6-limina16k` kernel, **real limina-agent** (the harness
never joins the control plane on this tier). Every metrics.json carries the tier
stamp (`verify_tier`: in-guest PAGESIZE + 7.x kernel), because both the F43 golden
and the stock image would otherwise pass a green run silently.

## S0 — the smoke boot every enhanced number stands on

Boot-to-ssh **14.9 s**; the real agent's reports were flowing before ssh was even up
(first consumed report at +0 ms), median consumed-report gap **1005 ms** — the agent's
1 s idle tick, now measured in vivo with no relay in the loop. Moderate converged on
its own: 15 target steps, final target = actual = **3833 MiB** with `reclaimed=` equal
**to the byte** (stock: 99.2%). gdm runs headless with `NRestarts=0` (no churn hazard
for idle scenarios); agent unit stable; 0 puff lines.

## S1enh — the 16k mechanism is the same mechanism

Same shape as stock S1 (3 GiB excursions, no policy): inflate **2129 / 1864 MiB/s**
(cold/warm; stock 1833–1860), deflate **2471 / 2528 MiB/s** (stock 2464–2475), 100%
of ballooned bytes reclaimed on both cycles, 0 puff lines. D4/I3/I4 are
tier-independent at these sizes; nothing about "deflates too slowly" lives in the
16k driver either.

## S2enh — the real agent doesn't just detect faster, it changes WHICH release fires

Moderate + the disabled denominator (Phase 2's axis is the tier; the mode ladder was
Phase 1's), same 3 GiB burst, same floors. Stock numbers in parentheses:

| R MiB/s | burst wall | detection | release shape | swapped | direct reclaim |
|---------|-----------|-----------|---------------|---------|----------------|
| 512  | 6.18 s | **1152 ms** (1517) | dribble −521 MiB | **0** (2 M) | 0 (~0) |
| 1024 | 3.19 s | **530 ms** (1962) | dribble −442 MiB (stock: to 0) | **75 M** (551 M) | 0 (250 M) |
| 2048 | 1.67 s | **318 ms** (1831) | dribble −411 MiB (stock: to 0) | **872 M** (1808 M) | 66 M (773 M) |
| ∞    | 0.66 s | **391 ms** (1639) | to 0 | 1493 M (1652 M) | 96 M (619 M) |

1. **Detection is 2–5× faster than the stock bench measured** — and the reason is
   instrument-honesty, not magic: Phase 1's harness relay read guest PSI over ssh
   (hundreds of ms of staleness per report), the real agent reads `/proc` in-guest and
   writes vsock in microseconds. The production stack is better than Phase 1 credited;
   the remaining latency is D1 (the avg10 EMA) plus the 1 s tick.
2. **Fresh reports flip the release tier.** Catching the burst earlier on the pressure
   ramp means the gentler below-allowance/shortfall path fires (proportional dribbles,
   −411..−521 MiB) where stock's stale reports arrived at panic-to-0 severity. That
   halves swap casualties at 1–2 GiB/s AND keeps ~3.3 GiB ballooned through the burst —
   no full dump, no 5-minute-cooldown re-inflate cycle afterward.
3. The release now lands **mid-burst even unthrottled** (det 391 ms < 658 ms wall);
   on stock, ≥2 GiB/s bursts finished before the release arrived. zram still carries
   the worst case (1.5 G swapped at ∞), but the policy is no longer a bystander there.
4. All points survived, **0 OOM kills**, 0 puff lines. (Casualty columns are 16 KiB
   pages ×16 KiB; stock's were 4 KiB pages — both quoted in MiB here.)

## S4enh — convergence with a live desktop underneath

Moderate, real agent, 6144 max: 18 steps to target=actual **3726 MiB**, median report
gap **1004 ms**, converge 117.6 s (the loop started before the harness could watch —
onset "0 ms" means the agent was already reporting at boot), gap residency 1.2%,
0 puff lines, and **2 "reversals" that are the allowance regulator working**: the ramp
topped at 3802 MiB, ~90 s later the guest's own session activity pushed avail to
849 MiB — under moderate's 1 GiB allowance — and the policy gave back 194 MiB, then
re-trimmed in two small steps. Stock's bare console never moved; the enhanced guest is
a real desktop and the closed loop tracks it. `reclaimed=` overshoots `actual` (4.11 G
vs 3.91 G) because it counts cumulative madvise across those give-backs.

## S5 — a busy control plane does not stretch D2 (clean negative)

Run `s5-1786380516`, stock tier, moderate @ 512 MiB/s, fresh boot per point. A second
control connection spammed heartbeats at ~170–200 msg/s (1890 delivered) through the
burst window:

|        | detection | median report gap | p95 gap |
|--------|-----------|-------------------|---------|
| quiet  | 1431 ms   | 1865 ms           | 3157 ms |
| chatty | 1481 ms   | 1768 ms           | 3181 ms |

The 50 ms deltas are noise. The control plane's one-serve-thread-per-peer design
(`crates/limina/src/control.rs`) isolates the policy's report stream from peer traffic —
the D2-starvation question closes with a no. (The *other* starvation seam — the agent
sending reports only on idle poll ticks, so host→agent traffic defers them at the
source — is agent-side, not mux-side; §6/§10.4 of the design carry it.) Note both
points' ~1.8 s median gap is the stock harness relay's ssh staleness, consistent with
the S2enh finding that the real agent's 1 s cadence is what production actually gets.

## S8 — 35 min of desktop steady state: the organic gap did NOT appear

Run `s8enh-1786380667` (`s8enh-1786378742` is the trace-only remnant of a first
attempt killed 22 min in by the same background-task reaper as the Phase-1 matrix —
see incidents): seated EFI F44 enhanced desktop (coexist venus display, real
GNOME session), cache warmed by real file reads (`tar` over `/usr` + `/var/lib`),
then 35 minutes under `moderate` with the real agent and nothing synthetic.

**Zero gap episodes. Zero `Out of puff` lines. 0.00% gap residency.** And not because
the policy was asleep: 38 sent target changes tracked the session across the window
(final target=actual 2873 MiB; cumulative `reclaimed=` 3.5 G shows several give-back/
re-take rounds), the hold histogram is dominated by `dead-band` (2067 of 2124 consumed
reports — converged tracking), kswapd burned 82 ticks in 35 min and direct reclaim
stayed at zero. The closed loop on the enhanced tier is *healthy* in exactly the shape
we could synthesize.

What this means for H1: the **mechanism** is proven (S7: spam iff held gap) and the
**policy trigger** is now bounded — a warm-cache idle desktop does not produce it in
35 min. What this run deliberately lacked is what the S7 ballast had to simulate: a
large pinned anon working set (browsers, editors) held for hours against a shrinking
MemFree floor, and the dogfood VM's own sizing. The next honest step when tuning
starts is a read-only trace capture on the dogfood guest itself (LIMINA_BALLOON_TRACE
is deployable there) rather than more synthetic horizons here.

## S7enh — six constructions, one finding: the driver has no self-preservation

The stock S7 needed one run. The enhanced S7 got six constructions and every one
ended the same way — and that convergence, not a green run, is the result. The goal
was a *healthy* guest holding an unfillable target (the dogfood state: hours of
`Out of puff` spam from a perfectly usable desktop). The record (all run dirs
archived here):

| # | construction | outcome |
|---|-------------|---------|
| 1 | stock sizing (4 GiB, cap RAM−512) | desktop squeezed to catatonia; agent+sshd died (`s7enh-1786378025`) |
| 2 | 6 GiB, no pinning | 16k+zram guest yielded *everything* — 4 escalations to cap in 1.5 s, zero puff |
| 3 | + 3 GiB mlocked ballast, zram on | the chase ground the **desktop** out via zram once cache was gone (`s7enh-1786380313`) |
| 4 | ballast + swapoff, 6 GiB | wall sat at absolute-zero cache → OOM collapse (`s7enh-1786382977`; zram variant repeated at 10 GiB, `s7enh-1786383422`) |
| 5 | fixed mid-range target ("the grind") | closed **silently in seconds** — the 33 MiB/s grind it was built on was the instrument artifact (`s7enh-1786383821`) |
| 6 | ballast + swapoff + 10 GiB scale | every 256 MiB escalation closed in one 200 ms sample (4096→6400 MiB in <5 s); the OOM killer fed the unprotected desktop to the balloon; sshd reset (`s7enh-1786384443`) |

Attempt 6's honest phase B: 2 GiB against warm cache in **1.25 s (~1.6 GiB/s)** on
16k — same order as stock's corrected 2.4 GiB/s and S1's 1840 MiB/s. Fill speed is
tier-independent and source-independent; there is no grind state anywhere.

**The finding:** the virtio-balloon driver (both page sizes) will satisfy any target
the host asks for, up to and including the guest's own working set — cache first,
then the desktop's anon via zram, and with swap off the OOM killer makes up the
difference. "Unfillable while healthy" is not a state the guest can be *driven* into
from the target side; every path there ends in eviction or death. The wall that
protects the guest **does not exist in the guest — it has to live in the host
policy** (the MemFree-informed target clamp / gap-decay lever just acquired its
strongest justification). Conversely, the dogfood healthy-spam state must be the
*policy* parking a target just above what the driver can extract *without* the guest
noticing — a narrow band the S8 idle desktop never entered and a synthetic chaser
overshoots by construction.

Consequences recorded in code: the enhanced S7 now **skips** with the conclusion
(the scenario is unconstructible, not flaky); the stock run remains the journal
channel's positive control, and the dogfood field spam is the enhanced-tier positive
control. `puff_since` panics on dead ssh and `count_oom_since` brackets the chase —
both guards were paid for by these six runs.

## Phase-2 instrument incidents (tuition paid, guards added)

1. **The "33 MiB/s grind" was arithmetic, not a measurement.** Phase 1's S7 divided
   the last sample's `actual` by the fixed 60 s window; the fill had completed in
   0.84 s. Every run "measured" 2048/60 ≈ 33 exactly — an identical number across
   configurations is an instrument smell, and it went unnoticed for a full phase.
   The harness now records `slow_fill_reach_ms` (first sample at target) and computes
   the rate over the time the fill actually took. Phase 1's S7 section carries the
   correction block; the raw CSVs were always right. Worse than the wrong number was
   the wrong *model* it seeded: attempt 5 of the enhanced S7 construction was built
   on the grind's existence.
2. **The background-task reaper strikes twice.** The chained S7+S8 run was SIGTERM'd
   at ~48 min — same ceiling that killed the Phase-1 matrix. Long multi-scenario runs
   now go through a detached `nohup` runner script (macOS has no `setsid(1)`; two
   launch attempts failed before landing on `nohup ... </dev/null >/dev/null 2>&1 &`
   from a foreground shell), with progress polled from its log.
3. **A dead guest must never read as zero.** Attempt 1's overshooting cap killed the
   enhanced guest's sshd, and `puff_since`'s `unwrap_or(0)` laundered the dead ssh
   into "zero puff lines" — a green-looking number measuring nothing. It now panics
   on ssh failure, and `count_oom_since` brackets phase C so "the wall is healthy"
   is asserted from the journal, not inferred from ssh survival.

## Phase 2: the questions, answered across tiers

1. **Deflation ("releases too slowly").** The mechanism is tier-independent
   (S1enh: deflate ~2.5 GiB/s, inflate ~1.9–2.1 GiB/s, 100% reclaimed to the byte)
   and detection is still the whole story — but the **real agent is 2–5× faster
   than Phase 1 measured** (S2enh: 318–1152 ms vs the stock relay's 1517–3541 ms;
   Phase 1's numbers carried harness ssh staleness). Fresher reports don't just
   shave latency, they **change which release fires**: proportional dribbles
   (−400..−520 MiB) instead of panic-to-0, halving swap casualties at 1–2 GiB/s and
   keeping ~3.3 GiB ballooned through a burst. The policy's genuine coverage still
   ends between 1 and 2 GiB/s; zram carries the rest (unchanged from stock).
2. **Inflation.** Healthy on both tiers. With a live desktop underneath (S4enh) the
   converge is clean, and the "reversals" observed are the allowance regulator
   correctly giving back when session activity dips below the allowance — a
   behavior, not a bug.
3. **`Out of puff`.** The mechanism was settled on stock (spam iff an open
   target/actual gap, at the driver's ~5 Hz; instant silence on close). Phase 2's
   two additions: (a) the **slow-grind state does not exist** — the "33 MiB/s"
   Phase-1 number was an instrument artifact, fills run at GiB/s from any
   reclaimable source on both page sizes; (b) the enhanced tier **cannot be driven
   into healthy-spam from the target side at all** (S7enh, six constructions — the
   driver has no self-preservation). Combined with S8 (35 idle desktop minutes,
   zero episodes, policy actively tracking), the dogfood state is bounded to a
   narrow band: the policy parking a target just past what the driver can extract
   invisibly. The next honest step is the read-only `LIMINA_BALLOON_TRACE` capture
   on the dogfood guest — not a seventh synthetic construction.
4. **Edges.** The control plane does not stretch detection (S5: 1890 spam
   heartbeats moved D2 by ~50 ms — per-peer serve threads isolate the mux; clean
   negative). The enhanced boot substrate is sound (S0: real agent reports on the
   idle tick with no harness feeding, gdm+agent stable).

**Handoff to tuning (design §10), sharpened by this phase:** the MemFree-informed
target clamp (lever 2) is now *mandatory-shaped* — the guest will not protect
itself; light's flap debounce (lever 7) and the io-PSI give-back term (lever 6)
stand as found in Phase 1; the agent fast-path (lever 4) is already validated at
the source. Tuning starts replay-first, with a dogfood trace capture as its first
fixture.
