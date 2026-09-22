# Where xnu runs a time-constraint thread on an asymmetric host

Measured 2026-09-22 on an Apple M1 Max (8 P + 2 E, `hw.ncpu` 10), macOS 26.6.2 (25G83). No
`limina-vmm` ran during any arm. The host was not otherwise idle: at rest the two E-cores sat at
30–50% busy and the P-cores at 0–28% (the `monitor-idle` baseline), from host background load.

**Verdict: the premise is false.** xnu serves a `THREAD_TIME_CONSTRAINT_POLICY` thread on an
efficiency core, and prefers one for a thread that runs in short bursts. RT placement follows what
the thread *does*. A bursting RT thread is placed like an ordinary one and runs about 96% of its time
on E. A saturating RT thread runs 100% on P. It is *stickier* to P than ordinary work: with every
P-core already busy with ordinary spinners it still ran 0% on E, where ordinary spinners spilled
18–20% onto E. The rule that a little vCPU never takes the band still holds, but for a different
reason, given at the end.

## Method

- **CPU id.** `pthread_cpu_number_np` disassembles to `mrs x9, TPIDR_EL0; and x9, x9, #0xfff` on
  this release. `TPIDRRO_EL0` holds the TSD pointer, not the CPU (a per-thread constant; it disagreed
  with the API on 100% of samples). The probe reads `TPIDR_EL0 & 0xfff` inline and cross-checks
  against the API on every sample. They disagree on 0–39 of ~10^5 samples per thread: a migration
  between the two reads.
- **Encoding check.** One ordinary spinner per CPU for 3 s (`calib-all`). Every thread saw every id
  0–9.
- **Which ids are E.** Two independent signals, and they agree that ids 0 and 1 are the E-cores:
  - Two and four `QOS_CLASS_BACKGROUND` spinners ran only on ids 0 and 1, at priority 4, and the
    kernel's per-CPU tick counters showed CPUs 0 and 1 at 99–100% busy.
  - Loop throughput on ids 0 and 1 was 126–206 iterations/µs, against 434–497 on ids 2–9.
- **Sampling.** Each worker records its own CPU about every 20 µs while it runs. Every sample is
  sorted by the thread's `pth_curpri` (re-read every 1 ms): 97 or above counts as at-RT. So a thread
  that xnu's fail-safe demotes cannot count as an RT thread on E. Throughout, every RT thread in every
  arm read priority 97 on 100% of its priority reads.
- **RT parameters.** Those of libkrun's `set_realtime_band`: period 16.667 ms, computation 1 ms,
  constraint 2 ms, preemptible. Spinning threads park for 100 µs every 250 ms (the heartbeat).
- **Repeats.** 3 repetitions of every arm, interleaved; 10 s for burst arms, 12 s for the rest. Every
  RT thread ends at its own deadline.
- **Placement trap.** A thread blocked in `pthread_join` lends its priority to the thread it waits
  on. In the first calibration, the first `BACKGROUND` worker ran at priority 31 on P-cores because
  `main` was joining it. The probe therefore sleeps past the end of the run before it joins.

`run.sh calib <out>` / `run.sh matrix <out> 0,1 3` reproduce this, and `summarize.py <out>/matrix.txt`
aggregates the output.

## Placement: samples on E vs P while at RT priority

Each cell is one repetition, summed over the arm's measured threads. The ordinary-thread control
arms appear in *italics*.

| arm | r1 E / P | r2 E / P | r3 E / P |
|---|---|---|---|
| (a) burst: 16.667 ms timer wake + 300 µs spin, *2 plain* | *11552 / 4808 (71% E)* | *15799 / 404 (98%)* | *15402 / 519 (97%)* |
| (a) burst, 1 RT | 6256 / 2220 (74% E) | 7988 / 209 (98%) | 7939 / 378 (95%) |
| (a) burst, 2 RT | 16015 / 540 (97%) | 16094 / 660 (96%) | 16292 / 360 (98%) |
| (b) spin on an idle host, *2 plain* | *0 / 1195685* | *1 / 1195895* | *14 / 1196135* |
| (b) spin, 1 RT | 0 / 598469 | 0 / 598474 | 0 / 598468 |
| (b) spin, 2 RT | 0 / 1196084 | 0 / 1196070 | 0 / 1196088 |
| (c) spin + 8 ordinary hogs, *2 plain* | *221096 / 904443 (20% E)* | *220314 / 914226 (19%)* | *202508 / 931354 (18%)* |
| (c) spin + 8 hogs, 2 RT | 0 / 1196057 | 0 / 1196090 | 0 / 1196054 |
| (d) burst, 2 RT, `BACKGROUND` then RT | 16433 / 720 (96%) | 16112 / 750 (96%) | 16232 / 570 (97%) |
| (d) burst, 2 RT, RT then `BACKGROUND` | 16125 / 658 (96%) | 16363 / 600 (96%) | 16176 / 628 (96%) |
| (d) spin, 2 RT, `BACKGROUND` then RT | 189 / 1195815 | 194 / 1195874 | 97 / 1195964 |
| (d) spin, 2 RT, RT then `BACKGROUND` | 0 / 1196065 | 0 / 1196056 | 0 / 1196044 |

Timer-wake placement agrees with the table: in (a), 1 RT thread woke on E 451/599, 585/599 and
573/599 times across the three repetitions. RT wakes on an E-core are punctual: 16–23 µs late at the
median, p99 at most 77 µs, worst 157 µs. The ordinary control woke on E just as often and was
4.3–4.9 ms late at the median.

**(e) idle, then saturate.** One thread did (a)-style bursts for 5 s, then spun. It was re-run with
100 ms buckets (3 reps × 2 arms, 8 s, `results-e100.txt`). While bursting, the thread logged about 82 E
samples per 100 ms. Spinning starts up to one period before the 5 s mark, because the burst loop
stops when the next deadline would pass it. Counting E samples above the burst rate until the
thread's first bucket entirely on P:

| | r1 | r2 | r3 |
|---|---|---|---|
| RT, time spinning on E before moving to P | 15.6 ms | 21.5 ms | 17.6 ms |
| ordinary, time spinning on E before moving to P | 27.9 ms | 24.1 ms | 21.3 ms |

After that, zero E samples. A thread that begins on E moves to P within about 15–30 ms of
saturating, whether or not it is banded; the RT thread moved slightly sooner in all three reps.

**(d) `BACKGROUND` with RT.** Setting `QOS_CLASS_BACKGROUND` after the time-constraint policy fails:
`pthread_set_qos_class_self_np` returns 1 (`EPERM`). Setting it before succeeds, and then the
time-constraint policy overrides it. The thread runs at priority 97, and once saturated it runs on P
(0.008–0.016% E), just as a plain RT thread does. Nothing about the combination confines a thread to
E.

## Cluster parking

Not observed on this host, and there is no evidence that the signals used here could observe it.
Three non-root signals were sampled every 100 ms in every arm:
- `kern.sched_recommended_cores` (readable without root) was `0x3ff` in every sample.
- `PROCESSOR_BASIC_INFO.running` was never 0.
- `PROCESSOR_CPU_LOAD_INFO` tick deltas were never 0 on a single CPU. Once, in one 100 ms interval
  (a-burst-rt2 r1, during a 146 ms monitor stall), the delta was 0 on all ten CPUs at once,
  including busy ones. That is an accounting artifact, not parking.

None of these was seen to change in any run, so none is validated as a parking oracle. The P-cluster
state in the M4 Pro panic remains unexplained. `kern.suspend_cluster_powerdown` exists (0 here) and
may be relevant; it was not investigated.

## What this means for limina

- **The E-core premise is false.** Nothing in xnu's placement protects the host from RT spinners
  running on the E-cluster. The panic shape — RT threads owning every online core while the
  P-clusters were down — is consistent with this. On this host, saturated RT threads always moved to
  P, and the P-cores were never seen offline, so the panic was not reproduced here.
- **The little-vCPU rule stands on a different reason.** Banding a little vCPU would run it at
  priority 97 and, whenever it is busy, on a P-core. The `BACKGROUND` confinement that makes it
  little is overridden (`BACKGROUND` before RT) or refused outright (`BACKGROUND` after RT).
- **Banding an idle vCPU does not move it to P.** A banded, mostly idle vCPU does most of its work on
  an E-core and still wakes punctually, about 20 µs late.

The `set_realtime_band` comment in libkrun (`vmm/src/macos/vcpu_sched.rs`) states the little-vCPU
rule on this reason.
