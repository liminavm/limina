# Balloon characterization bench: measuring the autoballoon before tuning it

Status: DESIGN (2026-08-10). Companion to `docs/design/m6-dynamic-memory.md` (mechanism +
policy as built) and the two incident postmortems folded into it (the 2026-07-03 limit
cycle, the 2026-07-09 sticky-Warn wedge). This doc designs the *instrument*: a repeatable
set of tests and measurements that characterize the current behavior of every reclaim mode
(`disabled` / `light` / `moderate` / `aggressive`), so that heuristic changes — the
motivating suspicion is **"the balloon does not deflate quickly enough when needed"** —
can be measured instead of felt.

Tuning heuristics are explicitly **out of scope** here (§9 lists the levers we expect to
reach for). The instrument comes first: both prior balloon incidents were only understood
once someone sat down and recorded a time series; this bench makes that recording a
one-command artifact instead of an incident response.

## 1. The question, decomposed: the deflation latency chain

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

Clock note: guest wall-clock is anchored to host `CLOCK_REALTIME` (PL031 + TimeSync, see
`limina-guest-clock`), so guest-side and host-side samples can share a timeline with ~ms
alignment. All traces record `CLOCK_REALTIME` timestamps.

## 2. Instrument gaps (prerequisite code changes)

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
   PSI averages; the sampler (§3) must also record the cumulative `total=` counters from
   `/proc/pressure/memory` *and* `/proc/pressure/io`, plus `pgmajfault`/`pswpin`/`pswpout`
   from `/proc/vmstat`. Per-tick deltas of PSI `total=` are the obvious fast-detection
   heuristic candidate — traces that captured them let us back-test that heuristic
   against recorded runs without a single re-boot (§6).

## 3. Architecture: recorder, scenarios, summarizer — then replay

Three pieces, all living in the repo (`spikes/` convention for results, harness code in
`crates/limina-test`):

- **Recorder.** Two samplers writing timestamped CSV/JSONL, merged on the shared
  realtime clock:
  - *In-guest*: a self-contained script staged over ssh and run **detached in the guest**
    (`setsid nohup`, output to a guest tmpfile fetched at the end) sampling at 250 ms:
    meminfo (MemTotal/MemAvailable/Cached/SwapFree), `/proc/pressure/{memory,io}` (avgs
    *and* totals), vmstat counters. Sampling in-guest is load-bearing: deflate latency
    needs sub-second resolution and per-sample ssh round-trips would alias it.
  - *Host-side* (in the bench process): balloon socket `stats` (target/actual/reclaimed)
    at 250 ms, worker `phys_footprint` (`Guest::worker_phys_footprint()`), host pressure
    sysctls, and the `LIMINA_BALLOON_TRACE` file collected at the end.
- **Scenario library** (§5): parameterized workloads with a marked `t0` (workload onset
  written into the trace), each producing one merged trace + one metrics row.
- **Summarizer.** One shared implementation computing the §4 metrics from a merged
  trace, so before/after runs are comparable by construction. Output: a metrics JSON per
  run + a human table per matrix sweep, committed under
  `spikes/balloon-bench-<date>/RESULTS.md` with the raw traces beside it.

Harness note: like `balloon_psi.rs`/`balloon_burst.rs`, the bench **plays the agent** on
the stock baseline (synthetic or relayed-real reports over a control-plane connection) —
that keeps D2 under the bench's control. Enhanced-tier runs use the real `limina-agent`
and measure D2 as it actually is; the trace records report inter-arrival times in both
cases (instrument gap #4 makes starvation visible).

## 4. Metrics (defined once, computed by the summarizer)

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
- **Pressure exposure** — time-integral of PSI `some avg10` (memory *and* io) over the
  run; the sticky-wedge class shows up in io-PSI while memory-PSI sleeps.
- **Workload slowdown** — scenario completion time vs the same scenario on
  `reclaim=disabled` (each sweep runs the `disabled` baseline first; it is the
  denominator, not a guess).

Inflate side (the regression guard — deflation tuning must not resurrect the limit
cycle):

- **Convergence time** — idle onset → target within dead-band of the mode's allowance
  target.
- **Oscillation count** — target direction reversals per hour, plus max target/actual
  gap held over time (the "Out of puff" signature).
- **Report cadence** — inter-arrival distribution of `MemPressure` at the policy (D2
  observed directly).

## 5. Scenario library

Each scenario is parameterized and emits the standard trace; the matrix (§7) picks
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

## 6. The fast tuning loop: L0 trace replay

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

## 7. Matrix and phasing

Axes: reclaim mode {disabled, light, moderate, aggressive} × host pressure {normal,
warn, critical — injected} × guest tier {stock-4k baseline (harness plays agent),
enhanced-16k (real agent)} × scenario. The full product is hours of wall-clock; phase
it:

- **Phase 0 (instrument):** §2 gaps + recorder + summarizer + S1. Deliverable: the D4
  number per tier, and the trace/metrics pipeline proven end-to-end.
- **Phase 1 (characterize the complaint):** stock baseline; S2 sweep + S3 across all
  modes at host-normal, S6 for the host axis, S4 as the inflate guard. Deliverable:
  `spikes/balloon-bench-<date>/RESULTS.md` — the current-behavior baseline the user
  asked for, with the latency chain attributed.
- **Phase 2 (tiers + edges):** enhanced-16k runs of S1/S2/S4 (real agent ⇒ real D2;
  16k coalescing ⇒ possibly different D4), S5. Then tuning starts, replay-first.

## 8. What gates where

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

## 9. Levers we expect to tune (recorded now, implemented later)

Not commitments — the bench decides. Ordered by expected relevance to "deflates too
slowly":

1. **Faster detection signal**: per-report deltas of PSI `total=` (µs of stall per tick
   — reacts in one report instead of the avg10 EMA's ~10 s), and/or a
   MemAvailable-slope predictor ("at this burn rate the guest is starved in <2 s →
   release now"). Instrument gap #4 records both candidates from day one.
2. **Graduated release**: today's release is binary (shortfall-sized dribble below the
   allowance vs panic-to-0 at 10%); a middle tier (e.g. release proportional to
   pressure slope) may cut relief latency without full dumps.
3. **Agent-side fast path**: report immediately on threshold crossings (avail dropping
   below allowance, PSI total spiking) instead of only the idle tick — fixes D2's
   worst case if S5 shows it matters.
4. **D4 throughput**, only if S1 shows the driver is a bottleneck (larger leak batches,
   etc. — guest-kernel territory, we own it).

## 10. Cross-references

- `docs/design/m6-dynamic-memory.md` — mechanism, policy as built, both incident
  addenda.
- `crates/limina/src/balloon_policy.rs` — the pure `decide()` this bench characterizes.
- `crates/limina-test/tests/balloon_burst.rs` — the S2 ancestor (fixed-size burst
  guard).
- Memories: `limina-m6-dynamic-memory`, `limina-balloon-oscillation`,
  `limina-mem-overhead` (Run D: the cache-cost numbers behind the allowances).
