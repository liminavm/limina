# The vCPU real-time band

Why an idle guest misses frame deadlines, the band that fixes it, what the band costs the host, and
what ships. The mechanism is libkrun's (`third_party/libkrun/src/vmm/src/macos/vcpu_sched.rs`); the
policy is limina's (`worker_vcpu_sched` in `crates/limina/src/supervisor.rs`). Open items on it —
the arm cap, the parked performance clusters, idling the sampler, efficiency beyond idle — are in
`docs/hardening-backlog.md` §vCPU & power. Raw data: `spikes/macos-timer-wakeup/`.

## The symptom

An idle guest misses frame deadlines. `vkcube` alone on a 59.885 Hz output runs at about 40 FPS,
with frame times quantised to whole vblank periods. Any guest load, or `nohz=off`, fixes it.

## Why it happens

The cause is late timer wakeups — a property of host thread scheduling, not of our WFI park. On
macOS 26.5, HVF parks the vCPU inside `hv_vcpu_run` (`VcpuStateManager::wait_for_interrupt`) and
serves the vtimer from its own thread, so `vstate.rs::wait_for_event` is dead code here (30 s idle:
`WaitForEvent`, `WaitForEventTimeout` and `VtimerActivated` all 0); `LIMINA_WFI_LATENCY` stays in
place to notice if that changes. `hv_vcpu_run_until` is x86-only. HVF's wait still runs on our vCPU
thread, so a band applied to that thread helps.

For an ordinary thread, a 16.667 ms deadline is served about 1.5 ms late at the median and tens of
ms late in the tail. `THREAD_TIME_CONSTRAINT_POLICY` brings that to 18 µs median / 52 µs worst. The
wait primitive and the latency-QoS tier make no difference.

## Measured matrix

FPS, two runs each (`results-guest-arms.md`):

| policy | idle | six spinners | one spinner per vCPU |
|---|---|---|---|
| none | 43.6 / 39.2 | 54.5 / 55.7 | 60.4 / 60.4 |
| band on every vCPU | 59.5 / 59.7 | 59.6 / 59.7 | 3 and 32 frames in 20 s |
| band on vCPU 0 only | 52.1 / 53.0 | — | 59.9 / 57.8 |
| `QOS_CLASS_USER_INTERACTIVE` | 47.2 / 46.6 | 55.0 / 58.6 | 59.2 / 59.4 |
| armed per vCPU from CPU share | 58.6 / 59.5 | 59.7 / 59.7 | 59.8 / 59.1 |

MangoHud counts guest presents, not host flips, so it resolves 20 FPS effects but not 1 FPS ones.

## The band is a reservation, not a priority

Banding every busy vCPU starves the worker: during a collapse every other worker thread sat at
0.0%, and venus ring `signal->resume` took 28.7 ms average / 434.9 ms worst against 8–27 µs
unbanded. xnu's RT fail-safe (`thread_quantum_expire`) is ruled out by experiment: a forced 100 µs
park every 250 ms fired 1200 times and changed nothing. Saturated-band FPS varies from run to run
(60.6 / 55.0 / 31.8 on one boot), so never judge the collapse from one run. Host ordinary-priority
contention does not explain the spread (0/4/8 host threads: 31.8 / 29.6 / 29.8).

## What ships

`rt+dyn`: each vCPU's share of a core (`THREAD_BASIC_INFO`) is sampled every 200 ms, arming below
35% and disarming above 60%. The default is `LIMINA_VCPU_SCHED=rt+dyn#1`, set by the supervisor
unless the environment names a policy (an empty value turns it off). `#1` limits it to vCPU 0; it is
a safety setting and costs about half the idle gap (52.1 FPS). A static choice cannot work, because
the deadline that matters migrates between vCPUs.

Burst transitions from 250 ms to 8 s cost no frame over 100 ms when banded; the unbanded arm is the
one that suffers (77/123 frames over 33 ms at 250/500 ms bursts, against 2/29 banded;
`results-burst-and-contention.md`). Arming has a cost: a vCPU that has just gone idle waits a sample
plus hysteresis before it is punctual.

**Panic guards.** A banded, saturated guest can panic the host (watchdogd starved), so two guards
hold:
- `BandGuard`: a vCPU disarms itself at guest exit once it has burned `SELF_DISARM_PERIODS` (2)
  periods of CPU time at ≥ `SATURATED_PERCENT` (50) of its hold. The threshold rests on measured
  separation — real saturation ran 73.7–100%, idle false positives at most 8.5%; a 90% threshold
  would have missed 4 of 8 real saturations. An unreadable `thread_cpu_us` disarms.
- `arm_cap()`: at most half the efficiency cluster (floored at 1) may be armed, enforced every sample
  in both directions.

The venus ring thread needs no band: its doorbell → worker → `cnd_signal` wake measures 8–27 µs (max
0.13–1.54 ms) and stays flat across park lengths. The fault is specific to timer-driven wakeups on an
idle host.

## What the band costs

**Battery** (`results-battery.md`): at idle, band plus sampler cost under about 20 mW of package power
(banded 128/104 mW, unbanded 111, empty-host floor 98). While presenting, the band draws +154 mW
(+18%) for +21% frames (58.9/59.8 FPS against 47.7/49.6), so energy per frame is flat.

**Host throughput** (`results-host-impact.md`, measured 2026-08-28): the only cost. An 8-thread
host job keeps 3452–3458 Miter/s against an idle guest under every policy. Against a saturated guest it keeps 2050 unbanded
but only 538 with every vCPU banded (15% of solo, 9 s → 61 s). `rt+dyn` matches unbanded rep for rep,
which is evidence the disarm is complete. Only a CPU-bound host job has been measured.

**Host wake latency** is unchanged: pooled over 5400 samples per cell, the share of deadlines missed
by more than half a frame is 19.4–19.9% for every arm against an idle guest (empty host 22.0%), and
10.5–13.3% against a saturated one. Measure with `hostlate.c` (one wait, one policy, many samples,
counts); `wakeprobe` gives one sample per cell and reproduces nothing.

## Guest power profile

Stock F44 has no cpufreq or `platform_profile`, but tuned backs `net.hadess.PowerProfiles`
(power-profiles-daemon is inactive). The enhanced-tier design is `docs/design/power-profiles.md`.
