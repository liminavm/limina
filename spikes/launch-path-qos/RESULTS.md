# Thread policy × launch path: timer lateness and core placement of a mostly idle thread

Does starting the worker from a launchd job (`crates/limina-launch`, `ProcessType=Interactive`)
open scheduling levers for the vCPU threads that `spikes/macos-timer-wakeup/` did not have? And
which levers move a lightly loaded thread off the efficiency cores? The second question comes from
the backlog item *A lightly loaded vCPU runs its work several times slower*.

Measured 2026-10-05 on the dev Mac: M1 Max (8 P + 2 E, E ids 0 and 1 per
`spikes/rt-ecore-placement/`), macOS 26.6.2 (25G83), on AC power, Low Power Mode off. Another
session's VM ran throughout. One idle Debian guest had one vCPU banded (`97R`) and drew a few
percent of a core.

## Verdict

- **The launch path changes nothing**, on either axis. In every cell, the probe started from a shell,
  from an AppKit app opened with `open -n` (the worker's old shape), from a gui-domain launchd job
  with `ProcessType=Interactive` (its shape now), and from that job with `LegacyTimers=true` reads
  within rep-to-rep noise. Each context ran in its own coalition, confirmed from the probe's own
  coalition ids. So moving to launchd added no lever, and `LegacyTimers` is not one.
- **An ordinary thread's timer lateness is timer slack proportional to the wait, not run-queue
  delay.** On a P-core, a `mach_wait_until` deadline lands **1/3 of the wait** late under the
  default, `UTILITY` and `USER_INTERACTIVE` QoS. With `THREAD_LATENCY_QOS_POLICY` tier 0 it lands
  **1/5** late. Measured ratios: 0.334-0.336 for waits of 1.7-6.7 ms in every default and `USER_INTERACTIVE` cell (also 0.67 ms in the one cell that did not overrun, and 10.7 ms for a thread already on P), and 0.201-0.203 for tier 0 at 1.7-6.7 ms. Exact 1/3 and 1/5 read as configured constants. xnu's timer coalescing parameters (`tcoal_prio_params`, the per-tier shift and maximum in `osfmk/kern/timer_call.c`) are where to look. Whether macOS 26.6.2 changed them from 26.5 is what would reconcile this with the August numbers, rather than another run of the probe. The p90
  sits within 3 µs of the p50, which a scheduling delay would not do. On an E-core, with a 10-16 ms
  wait, the default lands 0.26-0.32 of the wait late, and tier 0 tops out near 2 ms. A full 16.667 ms
  wait that follows an overrun lands 8.35 ms late. This contradicts the 2026-08-27 reading in
  `spikes/macos-timer-wakeup/RESULTS.md` that coalescing is not the dominant term and that the
  latency tier changes nothing. That spike measured 1.5 ms at the median for a 16.667 ms wait, on
  macOS 26.5; this one measures 4-5 ms on 26.6.2. The two have not been reconciled.
- **Only the RT band fixes lateness.** `THREAD_TIME_CONSTRAINT_POLICY` lands 14-23 µs late at the
  median and under 140 µs at the worst, in every cell. A kqueue timer with `NOTE_CRITICAL` gets the
  median to 30-50 µs, but at low duty its p99 runs to 0.3-7.9 ms, and HVF's park is not a timer we
  arm. Latency tier 0 cuts the median from about 4.5 ms to about 1.8 ms at low duty. That is a partial
  fix, but one that applies to a thread. The bar for replacing the band — p50 ≤ 100 µs, p99 ≤ 2 ms,
  nothing over 8 ms, set before the run — is met by no non-RT policy.
- **At low duty, every policy runs on an E-core, the RT band included.** At 2% and 24% duty, the
  default, `UTILITY`, `USER_INTERACTIVE`, tier 0, `NOTE_CRITICAL`, RT and RT+workgroup policies run
  82-100% of their chunks on E. The same chunk takes 1.04 µs on P, and on E either 2.0 µs or 4.2 µs,
  the two E clock levels seen. That is a 2x or 4x slowdown, the same order as the 3.5x the stock
  guest measured. At 78% duty everything runs on P.
- **A workgroup moves a moderately busy thread to P, and nothing else does.** A thread that joins an
  `AudioWorkIntervalCreate` workgroup and brackets each period with
  `os_workgroup_interval_start`/`finish` runs 98-99% on P at 24% duty, at 1.04 µs per chunk, in all
  24 runs. At 2% duty it stays on E. Banding it as well undoes the move (RT+workgroup stays on E at
  24%). The workgroup does nothing for lateness.

## What this means for limina

- No launchd-side lever. Keep `ProcessType=Interactive` for the Game Mode reason it was chosen.
- **The guest-side symptom fits the host slack.** A stock guest's periodic sleep loop lands
  1.6 ms late at the median and up to 8 ms late on a 16.67 ms period, and 227 µs late on a 1 ms
  period. Later wakeups for longer idle periods is what slack proportional to the wait produces, if
  HVF's in-`hv_vcpu_run` park inherits the vCPU thread's timer slack. That is an inference: this
  probe measures `mach_wait_until`, not HVF's park.
- **Two candidates to test at guest level**, with the band off and then on, in the
  `results-guest-arms.md` shape:
  1. **Latency-QoS tier 0 on every vCPU thread.** It is a thread policy, carries no RT risk and no
     arm cap, and covers the vCPUs the band's cap leaves out. Here it cut host lateness about 2.5x
     at low duty.
  2. **An `os_workgroup` joined by the vCPU threads**, against the low-duty slowdown. It cannot
     bracket intervals the way this probe does, because the vCPU thread does not see the guest's
     periods. Whether a joined but un-bracketed thread is still placed on P is the first thing to
     measure.
  A workgroup may also pull the P-cluster out of idle for a lightly loaded thread. Measure package power (`powermetrics`) before shipping it.
- Neither is shipped or proven at guest level yet.
- **A banded, mostly idle vCPU stays on E.** RT and RT+workgroup ran on E at 2% and 24% duty. The band's panic shape is *saturated* banded threads (`docs/hardening-backlog.md`, *Explain the parked P-clusters*), so idle vCPUs are its safe regime. That bears on *Revisit the band arm cap*: a cap that counted only busy banded vCPUs might be loosened for idle ones. It is not measured here.

## Method

`lpq.c`: one measured pthread per arm, each in a fresh process because thread policies are additive.
Each 16.667 ms period, the thread waits for an absolute deadline, records how late it woke and on
which CPU (`TPIDR_EL0 & 0xfff`), then runs `--busy-us` of fixed work in 512-iteration chunks, timing
each one. An overrun resets the next deadline to a full period ahead, so a backlog is not counted as
lateness. `run.sh` builds it, the app stub and the job plists, and runs the policy × busy matrix in
each context. Contexts are interleaved within a rep and the arm order is reversed on alternate reps.
Every arm has a bound. `summarize.py` aggregates. `hostmon.sh` records the top host CPU consumers
every 10 s.

- Pass 1 (`results-2026-10-05/`) and pass 2 (`results-2026-10-05-b/`): 8 policies × busy 300 /
  4000 / 13000 µs (2% / 24% / 78% duty) × 4 contexts × 3 reps, 240 periods per arm. Each pass
  aborted one arm on its bound. Both were a workgroup at 78% duty, a shape that also shows the 8.35 ms
  overrun lateness.
- Sweep (`results-2026-10-05-sweep/`): default, `USER_INTERACTIVE`, tier 0 and workgroup, with the
  wait varied from 16.4 to 0.67 ms (busy 300-16000 µs), shell and job contexts, 120 periods.
- **Contamination:** `results-2026-10-05-b-host.txt` shows a `rustc`/`clippy-driver` build at up to
  800% between 11:08 and 11:17 and again at 11:31. That covers pass 2's rep 1 and the start of rep
  2. Rep 3 ran at load about 1.3. The contaminated reps, the clean rep and pass 1 agree, and the
  slack ratios do not move under load.

## Traps

- **Duty decides the regime, so report it.** The same policy reads 4.5 ms late on E at 2% duty and
  1.2 ms late on P at 78% duty. The difference is the wait length, not a better policy.
- **The 8.35 ms lateness is a property of a full-period wait.** Arms whose work overruns the period
  self-sustain at it: each overrun resets the deadline a full 16.667 ms ahead. Read those cells as
  "overran", not as a policy result.
- `getpriority(PRIO_DARWIN_ROLE)` and the task category read 0 in every context. Coalition ids are
  the only signal here that tells the launch paths apart.
