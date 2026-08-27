# How late macOS wakes a thread that asked for a 16.7 ms deadline

An idle guest vCPU traps on WFI. limina reads the guest's virtual-timer deadline and parks a
host thread until it (`vmm/src/macos/vstate.rs::wait_for_event`). Whatever lateness that host
wait carries becomes the guest's timer lateness, and at a 60 Hz frame cadence a few milliseconds
of it is a missed flip. `wakeprobe.c` measures that lateness directly: request a deadline
16.667 ms out, take `mach_absolute_time()` on wake, record the overshoot.

Measured 2026-08-27, macOS 26.5, M1 Max, 400 iterations per cell, microseconds of lateness.
Each policy runs in its own child process — thread policies are additive on a thread.

## Idle host

| policy | wait | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| default | `pthread_cond_timedwait` | 1671 | 4289 | 20643 | 46629 |
| default | `nanosleep` | 1532 | 5104 | 9854 | 12706 |
| default | `mach_wait_until` | 1580 | 4500 | 10723 | 13219 |
| default | kqueue `EVFILT_TIMER` | 1694 | 3162 | 13295 | 23653 |
| default | kqueue + `NOTE_CRITICAL` | **59** | 2177 | 8901 | 14196 |
| `QOS_CLASS_USER_INTERACTIVE` | `mach_wait_until` | 1687 | 4150 | 12803 | 16827 |
| `LATENCY_QOS_TIER_0` | `mach_wait_until` | 1562 | 4365 | 11254 | 23938 |
| `THREAD_TIME_CONSTRAINT_POLICY` | `mach_wait_until` | **18** | **29** | **40** | **52** |
| `THREAD_TIME_CONSTRAINT_POLICY` | `mach_wait_until` + 0.5 ms spin | **0** | **0** | **3** | **8** |

## Host under load (8 spinners)

| policy | wait | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| default | `mach_wait_until` | 1213 | 2167 | 3631 | 4034 |
| default | kqueue + `NOTE_CRITICAL` | 37 | 492 | 1996 | 3326 |
| `THREAD_TIME_CONSTRAINT_POLICY` | `mach_wait_until` | 6 | 9 | 14 | 16 |
| `THREAD_TIME_CONSTRAINT_POLICY` | `mach_wait_until` + 0.5 ms spin | 0 | 0 | 0 | 1 |

Full output: `results-idle.txt`, `results-loaded.txt`.

## What the numbers say

**The wait primitive barely matters; the scheduling band decides everything.** `nanosleep`,
`pthread_cond_timedwait`, `mach_wait_until` and a plain kqueue timer are within noise of each
other — all land around 1.5 ms median and multi-millisecond tails. Swapping one for another is
not a fix.

**QoS class and latency-QoS tier change nothing measurable.** `QOS_CLASS_USER_INTERACTIVE` left
the thread's latency tier at 0 and its lateness unchanged; setting `LATENCY_QOS_TIER_0`
explicitly moved the tier (to `0xff0001`) and still changed nothing. So the dominant term is not
timer coalescing — it is how long a runnable thread waits to get a core. The hybrid wait proves
it independently: waking 0.5 ms *early* to spin still lands ~1.2 ms late under the default
policy, which can only mean the thread was runnable and not running.

**`THREAD_TIME_CONSTRAINT_POLICY` is the lever, and it is worth two orders of magnitude.**
Median 1580 µs → 18 µs, worst case 13 ms → 52 µs. It is the band CoreAudio's render thread uses:
declare a period, a computation slice and a constraint, and the scheduler treats the thread as
real-time.

**`NOTE_CRITICAL` is the cheap partial.** It fixes the median (59 µs) without any policy change,
but leaves a multi-millisecond tail — still enough to drop frames, just fewer.

**An idle host is the hostile case.** Every default-policy tail is *worse* idle than under load
(max 46.6 ms vs 5.0 ms). Cores in deep idle at low clocks take longer to pick a thread up. That
is precisely the state a quiet guest puts the machine in, which is why the symptom appears when
nothing is happening and vanishes when anything is.

## What this does and does not describe

The probe measures the host. It does **not** measure limina's WFI park, because on macOS 26.5 /
Apple silicon that park never runs: a guest's `WFI` does not trap out to libkrun, HVF parks the
vCPU inside `hv_vcpu_run` (`HvCore::Hypervisor::VcpuStateManager::wait_for_interrupt`) and serves
the virtual timer from its own `VirtualClock` thread. Measured with the `LIMINA_WFI_LATENCY`
counters plus a `sample` of the worker: 30 s of idle desktop, 48,489 vCPU exits, every one of them
an MMIO read.

The numbers still matter, for one reason: HVF's wait blocks on **our** vCPU thread, so the
scheduling band is still ours to set. Whether setting it reaches the wakeup that is actually late
is an open experiment, not a conclusion — HVF's clock thread is not ours.

## Reproducing

    clang -O2 -o wakeprobe wakeprobe.c
    ./wakeprobe [iterations] [deadline_us]
