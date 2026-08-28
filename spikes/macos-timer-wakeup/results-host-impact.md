# What a banded VM charges the host

Measured 2026-08-28, M1 Max on AC. Two instruments, because the band starves two things
differently: `hostlate-matrix.sh` (an ordinary host thread's lateness on a 16.667 ms deadline,
6 reps x 900 samples per cell) and `host-throughput.sh` (fixed work at ordinary priority, wall
clock, 3 reps per cell). Every loaded cell proves the guest is saturated — the guest's own idle
percentage — before it measures.

## Throughput: banding every vCPU takes the machine

Aggregate rate of an 8-thread host job, `hostwork 8 4000`:

| arm | idle guest | saturated guest |
|---|---|---|
| no VM at all | 3458 Miter/s | — |
| unbanded | 3454 | 2050 / 2058 / 2038 |
| static `rt` | 3452 | **538 / 521 / 1602** |
| `rt+dyn` | 3448 | 2043 / 2026 / 2038 |

An **idle** guest costs the host nothing under any policy — eight reservations held by threads that
are not running take nothing, which is the design's whole premise. A **saturated** guest costs the
host 41% of its throughput unbanded, which is roughly proportional sharing: eight host threads and
eight busy vCPUs on ten cores, both sides wanting everything.

Banding every vCPU turns that contest into a seizure: the host keeps **15%** of its solo
throughput, a 3.8x slowdown against the same guest unbanded, and the job runs 9 s → 61 s.
`rt+dyn` returns **exactly** the unbanded numbers, rep for rep — so the disarm is complete, not
partial. A sampler leaving even one or two vCPUs banded would show here, because the static arm
shows what a handful of reservations does to this job.

Note the static arm's third rep (1602 Miter/s against 538 and 521). Even the catastrophic
configuration is stochastic, the same way the guest-side collapse is.

## Latency: the policy does not move it

Share of deadlines missed by more than half a frame, pooled over 5400 samples per cell:

| arm | idle guest | saturated guest |
|---|---|---|
| no VM at all | 22.0% | — |
| unbanded | 19.9% | 13.3% |
| static `rt` | 19.9% | 10.5% |
| `rt+dyn` | 19.4% | 11.5% |

Every arm is within noise of an empty host, and the *loaded* cells are consistently the best ones —
a busy machine keeps cores awake and wakes threads sooner, the same effect that makes a loaded
guest render better. Misses beyond a whole frame are 0-8 per 5400 everywhere, with no ordering.
**A VM's vCPU scheduling policy does not move host wake latency.**

That is what the mechanism predicts, in hindsight: a real-time thread cannot be preempted, so
ordinary host work does not get woken *late* by one — it simply does not get *run*. Latency was
never the quantity to watch.

## Two ways this measured nothing while looking like it measured something

**A saturated cell that was never loaded.** Spinners started as background children of an ssh
session take SIGHUP when the session exits. Every "saturated" cell in the first pass was an idle
guest, and it read as a *result*: the host's throughput under a "saturated" guest came back
identical to an empty host, which is what exposed it. Start them with `setsid nohup`, and make each
cell print the guest's measured idle percentage before it measures anything.

**A `pkill` that matched its own command line.** `pkill -f 'while :'` over ssh matches the very
remote shell carrying the pattern, so the kill takes down its own session, ssh returns 255, and
under `set -e` the script dies holding the VM — after which the next arm cannot boot the disk. Give
the spinners a marker and bracket the pattern (`limina[-]spin`), and give the script an EXIT trap
so a VM never outlives it.

**And an instrument aimed at the wrong question.** The first latency answer came from `wakeprobe`,
which spends ~80 s sweeping six waits x four policies to produce a single sample per cell. Against
a heavy tail that yields a number that looks precise and reproduces nothing: reps of one arm
disagreed by 2 ms to 25 ms, and the ordering between policies flipped between passes. It is the
right instrument for "which lever exists" and the wrong one for "does this move the host". Hence
`hostlate.c`: one wait, one policy, many samples, and counts rather than a max that one unlucky
wake decides.

`hostlate`'s p50 (~5.6 ms) does not match `wakeprobe`'s default-policy cell (~2.0 ms); the two
differ in process and thread shape and were never cross-calibrated. Compare within an instrument,
not across them.
