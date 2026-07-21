# wakeup-probe — what Activity Monitor's "Idle Wake Ups" actually shows

**Date:** 2026-07-21. **Question:** the user observed limina-vmm at "20k+ Idle Wake Ups"
in Activity Monitor while a windowed venus VM ran the blobs WebGL demo — an order of
magnitude over kernel_task, and the number fluctuated. A prior memory claimed AM's column
is cumulative-since-launch; the fluctuation disproves that (cumulative counters can't go
down). Which of macOS's per-process wakeup counters is AM showing, and is the rate real?

## Method

macOS exposes two wakeup counters per process via `proc_pid_rusage(RUSAGE_INFO_V4)`
(readable without root for same-uid targets):

- `ri_pkg_idle_wkups` — wakeups that brought the package out of idle. This is what
  `top`'s IDLEW column shows (cumulative on the first sample, deltas after).
- `ri_interrupt_wkups` — all interrupt wakeups.

`procwake.c` samples both and prints cumulatives + per-interval rates. Build:
`cc -O2 -o procwake procwake.c`; run: `./procwake <pid> [interval_s] [count]`.

## Result (limina-vmm, 6-vCPU enhanced venus guest, blobs demo fullscreen)

| counter | rate |
|---|---|
| pkg_idle_wkups | ~8/s |
| interrupt_wkups | **~21,500/s** |

The "20k+" Activity Monitor shows matches the **interrupt-wakeup rate** (AM windows it per
refresh interval — hence the fluctuation). Conclusions:

1. **AM "Idle Wake Ups" is a windowed rate of `ri_interrupt_wkups`, NOT cumulative and NOT
   `top`'s IDLEW.** Comparing AM's number against top's pkg-idle counter is apples/oranges.
2. The 2026-07-01 venus-ring fix remains valid for what it measured (pkg-idle at true idle:
   ~150/s → ~2-4/s); it says nothing about under-load interrupt churn.
3. **~21.5k wakeups/s under a 60 fps venus workload (~360/frame) is a real optimization
   target**: doorbell eventfds, the worker main-thread kevent loop, cross-thread condvar
   hops (vcpu → event loop → gpu worker → vkr-ring → KK → vkr-queue), fence/present
   handshakes. CSW ~7k/s on the same process. Candidate levers: virtio notification
   suppression (EVENT_IDX) on the hot queues, batching doorbells, shortening the per-submit
   wake chain.
