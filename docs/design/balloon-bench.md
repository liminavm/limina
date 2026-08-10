# Balloon characterization bench: measuring the autoballoon before tuning it

Status: DESIGN (2026-08-10). Companion to `docs/design/m6-dynamic-memory.md` (mechanism +
policy as built) and the two incident postmortems folded into it (the 2026-07-03 limit
cycle, the 2026-07-09 sticky-Warn wedge). This doc designs the *instrument*: a repeatable
set of tests and measurements that characterize the current behavior of every reclaim mode
(`disabled` / `light` / `moderate` / `aggressive`), so that heuristic changes can be
measured instead of felt. Three questions drive it:

1. **"The balloon does not deflate quickly enough when needed"** (the motivating
   suspicion) — §1.1 decomposes it into a measurable latency chain.
2. **When and how does inflation actually happen** — not just "does it converge" but the
   real cadence, step sizes, gate behavior, and what the host actually gets back
   (§1.2).
3. **Why does the guest keep logging `Out of puff`** — §2 pins the verified driver
   semantics and the hypotheses the bench must discriminate.

Tuning heuristics are explicitly **out of scope** here (§10 lists the levers we expect to
reach for). The instrument comes first: both prior balloon incidents were only understood
once someone sat down and recorded a time series; this bench makes that recording a
one-command artifact instead of an incident response.

## 1. The questions, decomposed

### 1.1 The deflation latency chain

"Deflates too slowly" is not one number. From the moment a guest workload starts eating
memory to the moment the balloon has actually handed pages back, the release path crosses
four stages, each separately measurable and each a different tuning conversation:

| # | Stage | Mechanism today | Expected latency character |
|---|-------|-----------------|----------------------------|
| D1 | **Detection** — the guest signal crossing the release threshold | PSI `some avg10 ≥ 10%` (a ~10 s exponential average — it *lags pressure onset by design*), OR `guest_starved` (MemAvailable < max(256 MiB, total/64) — instantaneous meminfo) | seconds; the avg10 EMA is the suspected long pole |
| D2 | **Report transport** — the signal reaching the policy | `limina-agent` piggybacks `MemPressure` on its **idle** heartbeat tick (~1 s cadence, `guest/limina-agent/src/main.rs`); a busy control-plane stream defers the idle tick, so reports can thin out exactly when the guest is busy | ~1 s nominal; starvation under load is an open question this bench must answer, not assume away |
| D3 | **Decision + command** — `BalloonPolicy::decide` → `target` write | pure function + one socket write; deflation is un-gated (no dwell/cooldown/calm gate) | milliseconds; already unit-tested |
| D4 | **Guest execution** — the driver leaking the balloon | config-change interrupt → `leak_balloon` in the guest driver, host clears bitmap (no madvise on deflate; re-fault zero-fills) | **completely unmeasured today** — deflate throughput (MiB/s) is a number nobody has |

The headline deliverables of the bench are (a) this chain measured stage by stage per
mode, and (b) one end-to-end number per scenario: *workload onset → guest memory actually
relieved*. If D4 is fast and D1 dominates (likely), the tuning conversation is about
faster detection signals; if D4 is slow, it's about the driver/device — entirely
different work. Measuring the stages separately is what makes a later change
attributable.

### 1.2 The inflation chain

Inflation is the direction with all the gates, and it has its own stages — plus a
host-side stage deflation doesn't have (inflating is only *worth something* if the host
actually reclaims the pages):

| # | Stage | Mechanism today | What to measure |
|---|-------|-----------------|-----------------|
| I1 | **Calm detection** — deciding the guest is squeezable | inflation requires `some avg10` AND `avg60 ≤ 2%`, no post-release cooldown (5 min), host-pressure allowance per mode | how long after genuine idleness inflation actually starts; how often the gates hold (the trace's "why held" field) |
| I2 | **Pacing** — the commanded ramp | 256 MiB steps, 2 s dwell, 16 MiB dead band, allowance-clamped | commanded vs achieved cadence; convergence time; reversals |
| I3 | **Guest allocation** — `fill_balloon` filling the target | `balloon_page_alloc()` per guest page, `tell_host` in 256-PFN (1 MiB) array rounds; **this is where `Out of puff` lives** (§2) | actual-follows-target lag; allocation-failure episodes and their duration |
| I4 | **Host reclaim** — the point of the exercise | `process_ifq` coalesces and `MADV_FREE_REUSABLE`s full host pages; on a 4k guest coalescing is best-effort by design | `reclaimed=` vs `actual` (coalescing efficiency per tier), worker `phys_footprint` delta — MiB the host *really* got back per MiB ballooned |

Clock note: guest wall-clock is anchored to host `CLOCK_REALTIME` (PL031 + TimeSync, see
`limina-guest-clock`), so guest-side and host-side samples can share a timeline with ~ms
alignment. All traces record `CLOCK_REALTIME` timestamps.

## 2. The `Out of puff` question

Verified against the shipped guest kernel (`third_party/linux`, `limina` branch @
`07db31df`), not recalled:

- `fill_balloon` (`drivers/virtio/virtio_balloon.c:242`) allocates one guest page at a
  time via `balloon_page_alloc()` (`mm/balloon.c:147`), whose flags are
  `__GFP_NOMEMALLOC | __GFP_NORETRY | __GFP_NOWARN | GFP_HIGHUSER_MOVABLE`: an
  **opportunistic allocation that performs no meaningful reclaim** — it fails whenever
  *free* pages are short, even when MemAvailable (which counts droppable page cache)
  looks plentiful.
- On failure it logs the ratelimited `Out of puff! Can't get 4 pages`
  (`VIRTIO_BALLOON_PAGES_PER_PAGE` = PAGE_SIZE/4k = 4 on the 16k kernel — one guest
  page), `msleep(200)`, and breaks.
- `update_balloon_size_func` (`:551`) **re-queues itself whenever a target/actual gap
  remains**. So a target the guest cannot reach is a *permanent 5 Hz retry loop*:
  alloc-fail → log → sleep 200 ms → requeue, forever, with the attendant wakeups
  (relevant to the idle-overhead work too).

So the driver is correct but relentless, and the question becomes: **why does the policy
hold a target the guest can't reach?** Hypotheses the bench must discriminate:

- **H1 — the MemAvailable/MemFree mismatch (structural, prime suspect).** The policy
  sizes targets from **MemAvailable**; the driver satisfies them from **free pages**
  (`__GFP_NORETRY` won't evict cache). Whenever avail sits at/above the mode's allowance
  (so the below-allowance give-back never fires) while free is short, the target/actual
  gap — and the log spam — persists indefinitely. Steady state on a desktop guest with a
  warm cache is exactly this shape.
- **H2 — benign transients.** During any legitimate inflation ramp, concurrent guest
  activity can momentarily empty the free lists; a few ratelimited lines around inflation
  steps are expected and harmless. Distinguish from H1 by episode *duration* and by
  whether the gap closes.
- **H3 — held stale targets.** The gap can also be an artifact of gates: the dead band
  swallows a small corrective decrease, or a cooldown/calm gate blocks the *next*
  decision while the driver keeps chasing the last one. The decision trace's "why held"
  field makes these visible directly.

Attribution method: the recorder's kernel-journal channel (§4) timestamps every
`Out of puff` line onto the shared timeline; the summarizer correlates episodes with the
target−actual gap, MemFree, MemAvailable, and the active policy gate. Deliverables: puff
episodes/hour, episode-duration histogram, and fraction of run time spent with a held
unreachable target — per mode. Scenario S7 (§6) reproduces the loop mechanically so its
cost (log rate, wakeup rate, CPU) is quantified in isolation.

## 3. Instrument gaps (prerequisite code changes)

Small, mechanical, and required before any measurement run means anything:

1. **Host-pressure injection seam.** `Light` and `Moderate` differ *only* under host
   Warn/Critical, and the 32 GB dev Mac reads Normal essentially always — without an
   override, two-thirds of the mode×host matrix is unreachable. Add
   `LIMINA_HOST_PRESSURE=normal|warn|critical` checked at the top of
   `read_host_pressure()` (`crates/limina/src/balloon_policy.rs`), test/bench only,
   logged loudly when active. The blend logic keeps its unit tests; the env var is a
   bypass, not a third input.
2. **Expose `target=` in the harness.** The worker's `stats` reply already includes
   `target=` (added after the oscillation incident — the target/actual gap *is* the
   oscillation signature), but `Guest::balloon_stats()` parses only
   `actual=`/`reclaimed=` (`crates/limina-test/src/lib.rs:1922`). Return a struct
   `{ target, actual, reclaimed }` and migrate the three call sites.
3. **Policy decision trace.** The policy logs decisions at `debug` as free text. Follow
   the `LIMINA_DISPLAY_TRACE`/`LIMINA_WAKE_TRACE` precedent: `LIMINA_BALLOON_TRACE=<file>`
   makes the supervisor append one JSON line per *consumed report* (not just per target
   change): timestamp, the full `MemPressure` as received, host pressure (raw + blended +
   whether injected), current/decided target, and which gate held when the decision is
   `None` (dwell / cooldown / calm / dead-band / hold). "Why didn't it move" is exactly
   the question the incident debugging kept re-deriving from scattered logs.
4. **Guest-side sampler records more than the policy consumes.** The proto carries only
   PSI averages; the sampler (§4) must also record the cumulative `total=` counters from
   `/proc/pressure/memory` *and* `/proc/pressure/io`, plus `pgmajfault`/`pswpin`/`pswpout`
   from `/proc/vmstat`. Per-tick deltas of PSI `total=` are the obvious fast-detection
   heuristic candidate — traces that captured them let us back-test that heuristic
   against recorded runs without a single re-boot (§7).

## 4. Architecture: recorder, scenarios, summarizer — then replay

Three pieces, all living in the repo (`spikes/` convention for results, harness code in
`crates/limina-test`):

- **Recorder.** Two samplers writing timestamped CSV/JSONL, merged on the shared
  realtime clock:
  - *In-guest*: a self-contained script staged over ssh and run **detached in the guest**
    (`setsid nohup`, output to a guest tmpfile fetched at the end) sampling at 250 ms:
    meminfo (MemTotal/MemAvailable/**MemFree**/Cached/SwapFree — free vs available is
    the H1 discriminator), `/proc/pressure/{memory,io}` (avgs *and* totals), vmstat
    counters, and the **reclaim-work channel**: kswapd0 CPU ticks
    (`/proc/<pid>/stat` utime+stime) plus `pgscan/pgsteal_{kswapd,direct}` from vmstat.
    The kswapd deltas are the kernel doing the balloon's work in the background; the
    *direct*-reclaim deltas are allocating processes stalled doing that work themselves —
    the user-visible-latency shape of the same squeeze, and (with pgmajfault/pswpin) the
    honest cost side of every inflation. Sampling in-guest is load-bearing: deflate
    latency needs sub-second resolution and per-sample ssh round-trips would alias it.
  - *Kernel journal channel*: at run end, `journalctl -k -o short-unix` filtered to
    `virtio_balloon` (the `Out of puff` lines and anything else the driver says) becomes
    trace events on the shared timeline — the raw material for the §2 attribution.
    Caveat recorded per event: the lines are `dev_info_ratelimited`, so counts are a
    floor, not a census; episode boundaries (first/last line + the gap trace) are the
    honest unit, not line counts.
  - *Host-side* (in the bench process): balloon socket `stats` (target/actual/reclaimed)
    at 250 ms, worker `phys_footprint` (`Guest::worker_phys_footprint()`), host pressure
    sysctls, and the `LIMINA_BALLOON_TRACE` file collected at the end.
- **Scenario library** (§6): parameterized workloads with a marked `t0` (workload onset
  written into the trace), each producing one merged trace + one metrics row.
- **Summarizer.** One shared implementation computing the §5 metrics from a merged
  trace, so before/after runs are comparable by construction. Output: a metrics JSON per
  run + a human table per matrix sweep, committed under
  `spikes/balloon-bench-<date>/RESULTS.md` with the raw traces beside it.

Harness note: like `balloon_psi.rs`/`balloon_burst.rs`, the bench **plays the agent** on
the stock baseline (synthetic or relayed-real reports over a control-plane connection) —
that keeps D2 under the bench's control. Enhanced-tier runs use the real `limina-agent`
and measure D2 as it actually is; the trace records report inter-arrival times in both
cases (instrument gap #4 makes starvation visible).

## 5. Metrics (defined once, computed by the summarizer)

Release path (the complaint):

- **Detection latency** — `t0` (workload onset) → first target *decrease* in the policy
  trace. Isolates D1+D2+D3.
- **Execution latency / deflate throughput** — first target decrease → `actual` reaching
  the target; also reported as MiB/s. Isolates D4. (Scenario S1 measures D4 alone, with
  no policy in the loop.)
- **Relief latency** (end-to-end headline) — `t0` → guest MemAvailable recovering above
  the mode's allowance (or the workload completing, per scenario).
- **Casualties** — OOM-kill count (hard fail, always), `pgmajfault` delta, `pswpin`/
  `pswpout` deltas, peak swap usage.
- **Reclaim work** — per-phase deltas of `pgscan/pgsteal_kswapd` (background kernel work
  the balloon induces), `pgscan/pgsteal_direct` (allocation-latency work — a nonzero
  direct-reclaim delta means guest processes paid for the squeeze in their own stalls),
  and kswapd0 CPU time. A tuning change that trades a second of relief latency for a
  direct-reclaim storm did not win.
- **Pressure exposure** — time-integral of PSI `some avg10` (memory *and* io) over the
  run; the sticky-wedge class shows up in io-PSI while memory-PSI sleeps.
- **Workload slowdown** — scenario completion time vs the same scenario on
  `reclaim=disabled` (each sweep runs the `disabled` baseline first; it is the
  denominator, not a guess).

Inflate side (a first-class subject in its own right, and the regression guard —
deflation tuning must not resurrect the limit cycle):

- **Inflation onset latency** — genuine idle onset → first target *increase* (I1: how
  long the calm/cooldown gates actually hold, from the trace's "why held" field).
- **Convergence time and achieved cadence** — idle onset → target within dead-band of
  the mode's allowance target; achieved step rate vs the commanded 256 MiB/2 s ramp.
- **Actual-follows-target lag** (I3) — per inflation step, target write → `actual`
  catching up; stalls here are allocation failures.
- **Oscillation count** — target direction reversals per hour, plus max target/actual
  gap held over time.
- **Reclaim effectiveness** (I4) — `reclaimed=` vs `actual` (coalescing efficiency, per
  guest tier) and worker `phys_footprint` delta: MiB the host really got back per MiB
  ballooned. Inflation that doesn't shrink the worker is pure guest pain for nothing.
- **Report cadence** — inter-arrival distribution of `MemPressure` at the policy (D2
  observed directly).

`Out of puff` attribution (§2):

- **Puff episodes per hour and episode-duration histogram** (an episode = journal lines
  bracketed by a nonzero target−actual gap; ratelimiting makes line *counts* a floor,
  so episodes are the unit).
- **Unreachable-target residency** — fraction of run time with a held nonzero
  target−actual gap, cross-tabbed with MemFree vs MemAvailable at the time (H1 says:
  gap ∧ avail ≥ allowance ∧ free low).

## 6. Scenario library

Each scenario is parameterized and emits the standard trace; the matrix (§8) picks
mode × host-pressure × guest tier.

- **S1 — mechanism deflate/inflate (no policy).** Boot with balloon control but
  *without* the policy; inflate to a floor by writing targets directly, settle, then
  write `target 0` at a recorded `t0`; sample until `actual=0` and MemAvailable
  recovers. Repeat for inflate. This is the **pure D4 number** (deflate/inflate MiB/s
  per tier) and the calibration for everything else.
- **S2 — allocation-rate sweep (the headline).** The `balloon_burst.rs` allocator
  generalized: touch anonymous memory at a *controlled* rate R (throttled chunk loop),
  against a balloon pre-inflated to the mode's cap. Sweep R (e.g. 100/250/500/1000/2000
  MiB/s and unthrottled) to find the **max survivable rate** per mode — no OOM, workload
  completes, relief latency recorded. "Deflates too slowly" becomes "mode X survives
  ≤Y MiB/s; the wall is D1/D4 because …".
- **S3 — cache starvation (the wedge class).** A file-read working set larger than the
  mode's allowance (re-read a multi-GiB file) squeezing io-PSI up while memory-PSI stays
  quiet — the 2026-07-09 signature. Measures the `guest_starved` release path latency
  and proves the wedge stays dead under whatever tuning follows.
- **S4 — idle-inflate convergence.** From boot, idle guest, real or synthetic idle
  reports: time-to-converge, oscillation count, stability over ≥10 min. The inflate-side
  regression guard.
- **S5 — burst under a busy control plane.** S2 at a fixed rate while the control plane
  carries synthetic traffic (e.g. a chatty peer). Directly answers the D2 starvation
  question: does report inter-arrival stretch, and does detection latency follow?
- **S6 — host-pressure staircase.** With the injection seam: Normal → Warn → Critical →
  Normal, dwell at each step, idle guest. Verifies each mode's allowance ladder engages
  and disengages (Light does nothing at Normal, engages at Warn; floors hold at
  Critical; release on recovery), and measures those transition latencies.
- **S7 — unreachable-target chase (the `Out of puff` loop in isolation, no policy).**
  Command a target the guest demonstrably cannot fill (e.g. above MemFree while a
  cache-warm workload holds MemAvailable high — the H1 shape, synthesized), hold it for
  minutes, then drop the target to `actual` and confirm silence. Measures the retry
  loop's real cost — journal rate, guest wakeup rate (`LIMINA_WAKE_TRACE` numbers
  exist to compare against), CPU — and calibrates what a "persistent" episode looks
  like in the trace so the summarizer's episode detector is validated against ground
  truth. Also the desktop-shaped variant: a steady-state warm-cache guest under
  `moderate`, ≥30 min, counting episodes with *no* synthetic help — the dogfood
  complaint reproduced or falsified.

## 7. The fast tuning loop: L0 trace replay

A boot-per-iteration tuning loop dies of friction (each L2 run is minutes; a sweep is
hours). `decide()` is already pure and `DecideInputs` already carries `now`; the trace
format doubles as a replay fixture:

- **Replay harness** (host-side unit-test-level code, no HVF): feed a recorded report
  stream through a candidate policy with a synthetic clock; emit the target trace it
  *would* command. Committed fixtures: recorded S2/S3 traces, plus a synthetic
  reconstruction of the 2026-07-03 limit-cycle and the 2026-07-09 sticky-Warn wedge
  shapes as canonical regressions.
- **Honesty limit (open loop).** Replay feeds recorded guest signals; a different policy
  would have *changed* those signals, so replay ranks candidates on **detection latency
  and gate behavior only** (when would it have first released, what held it back). It
  cannot predict relief latency or casualties. Candidates that win in replay get one L2
  validation run. A closed-loop guest model is deliberately out of scope until the L2
  traces exist to validate one against.

## 8. Matrix and phasing

Axes: reclaim mode {disabled, light, moderate, aggressive} × host pressure {normal,
warn, critical — injected} × guest tier {stock-4k baseline (harness plays agent),
enhanced-16k (real agent)} × scenario. The full product is hours of wall-clock; phase
it:

- **Phase 0 (instrument): DONE 2026-08-10.** §3 gaps shipped, S1 ran — stock-tier D4 =
  deflate ~2.5 GiB/s / inflate ~1.8 GiB/s; pipeline proven. Results:
  `spikes/balloon-bench-2026-08-10/RESULTS.md`.
- **Phase 1 (characterize the complaints): DONE 2026-08-10.** S2/S3/S4/S6/S7 on the
  stock baseline, every channel positive-controlled; same RESULTS file. Headlines:
  detection (1.5–3.5 s) is the whole deflation story; the policy covers bursts
  ≤512 MiB/s and zram carries ≥2 GiB/s; `Out of puff` = held target/actual gap,
  confirmed (H2 dead); the organic policy-held gap was NOT captured — it moves to
  the Phase-2 steady state. Two findings feed back into §10 (levers 6–7).
- **Phase 2 (tiers + edges):** enhanced-16k runs of S1/S2/S4/S7 (real agent ⇒ real D2;
  16k pages change both I3 allocation and I4 coalescing, and the dogfood `Out of puff`
  reports come from a 16k guest), S5, and the **desktop-shaped ≥30-min warm-cache
  steady state** (S7's deferred variant — where H1 predicts the policy parks an
  unfillable target). Then tuning starts, replay-first.

## 9. What gates where

- **`scripts/test-boot.sh` (the ~28 min suite) gains at most one thin guard:** a
  deflate-throughput floor (S1 shape: inflate 2 GiB, `target 0`, assert `actual=0`
  within a bound) — cheap, pass/fail, catches a D4 regression. Existing
  `balloon.rs`/`balloon_inflate.rs`/`balloon_psi.rs`/`balloon_burst.rs` stay as-is.
- **The bench matrix runs only on demand**, behind its own env (`LIMINA_BALLOON_BENCH=1`
  + a runner script/xtask), never in the default suite. It asserts nothing except
  no-OOM; its output is traces + metrics.
- **Replay regressions** (the incident fixtures) are plain `cargo test` — fast, always
  on.
- All measurement runs happen on **local clones on the dev Mac** — the complaint may
  originate on the dogfood guest, but the dogfood machine and its guest stay
  hands-off; we reproduce shapes locally (S2/S3 are exactly that).

## 10. Levers we expect to tune (recorded now, implemented later)

Not commitments — the bench decides. Ordered by expected relevance:

1. **Faster detection signal** (deflate): per-report deltas of PSI `total=` (µs of
   stall per tick — reacts in one report instead of the avg10 EMA's ~10 s), and/or a
   MemAvailable-slope predictor ("at this burn rate the guest is starved in <2 s →
   release now"). Instrument gap #4 records both candidates from day one.
2. **Close the H1 gap** (`Out of puff`): if H1 confirms, either size/clamp inflation
   targets by something the driver can actually allocate (MemFree-informed, not
   MemAvailable alone), or detect a persistently-held target−actual gap in the policy
   and back the target off to `actual` (gap decay). Policy-side first; teaching the
   *driver* to reclaim harder is guest-kernel territory we own but a bigger hammer
   (upstream chose `__GFP_NORETRY` deliberately — inflation stealing cache under the
   guest's feet is its own failure mode, see Run D).
3. **Graduated release** (deflate): today's release is binary (shortfall-sized dribble
   below the allowance vs panic-to-0 at 10%); a middle tier (e.g. release proportional
   to pressure slope) may cut relief latency without full dumps.
4. **Agent-side fast path**: report immediately on threshold crossings (avail dropping
   below allowance, PSI total spiking) instead of only the idle tick — fixes D2's
   worst case if S5 shows it matters.
5. **D4 throughput**, only if S1 shows the driver is a bottleneck (larger leak batches,
   etc. — guest-kernel territory, we own it). *Phase 0 answered: it is not.*
6. **An io-PSI / refault term in the give-back rule** (from S3): moderate's allowance
   charges a measured 5× I/O penalty on working sets above it while every existing
   threshold reads "fine" — cache-miss burn should be a loosening signal, not just
   starvation.
7. **Debounce host-level improvements** (from S6): light drops its entire ramp on a
   single Normal sample, and the sysctl blend can flap at the 40% availability
   boundary. Demotions (toward squeezing less) can stay instant; promotions back to
   Normal deserve a dwell.

## 11. Cross-references

- `docs/design/m6-dynamic-memory.md` — mechanism, policy as built, both incident
  addenda.
- `crates/limina/src/balloon_policy.rs` — the pure `decide()` this bench characterizes.
- `crates/limina-test/tests/balloon_burst.rs` — the S2 ancestor (fixed-size burst
  guard).
- Memories: `limina-m6-dynamic-memory`, `limina-balloon-oscillation`,
  `limina-mem-overhead` (Run D: the cache-cost numbers behind the allowances).
