# A vCPU stop outside the suspend window lands in CLOCK_MONOTONIC

## The claim

When the guest's vCPUs stop while the guest kernel believes it is *running*, the elapsed
wall time is absorbed by `CLOCK_MONOTONIC`. systemd's per-service watchdogs are armed on
that clock, so a stop longer than `WatchdogSec` kills those services on the far side.

This is not repairable after the fact. Sleeptime injection moves `CLOCK_REALTIME` and
`CLOCK_BOOTTIME` only, by construction — `kernel/time/timekeeping.c`
`__timekeeping_inject_sleeptime()` adds the delta to xtime and subtracts the same delta
from `wall_to_monotonic`, which is precisely the arithmetic that keeps monotonic still.
`CLOCK_MONOTONIC` is *defined* as excluding suspend; a kernel that moved it would break
every timer in userspace.

The kernel classifies elapsed time as "suspend" only between `timekeeping_suspend()` and
`timekeeping_resume()`. Time outside that interval is running time, permanently.

## Measured (2026-08-28, stock F44, 6 vCPU, host M1 Max / macOS 26.5)

`kill -STOP` on the worker for 210 s, host awake throughout, no PM involved:

| clock | before | after | delta |
|---|---|---|---|
| REALTIME | 1787934805.516 | 1787935035.722 | +230.21 s |
| MONOTONIC | 36.092 | 266.298 | +230.21 s |
| BOOTTIME | 36.092 | 266.298 | +230.21 s |

Host window 210.06 s + 20 s settle = 230.2 s. **The guest counter runs while the vCPU
threads are stopped** — all three clocks advance by the full window. The 1 Hz logger
(`clocklog.py`) records it as a single 210.2 s gap.

Consequence, same run, with `WatchdogSec=3min` drop-ins:

```
systemd-journald.service: Watchdog timeout (limit 3min)!
systemd-journald.service: Killing process 829 (systemd-journal) with signal SIGABRT
```

## Two things this pins about the surrounding design

**The PL031 injection path is shadowed on arm64.** `timekeeping_resume()` states its own
preference order — "suspend-nonstop clocksource -> persistent clock -> rtc" — and tries
`clocksource_stop_suspend_timing()` first, falling to the persistent clock only in the
`else if`. `arm_arch_timer.c:941` sets `CLOCK_SOURCE_SUSPEND_NONSTOP` unless the DT
declares `arm,no-tick-in-suspend`, and libkrun's timer node
(`src/devices/src/fdt/aarch64.rs:294`) declares only `always-on`. So sleeptime comes from
the counter, never the RTC, and `timekeeping_rtc_skipresume()` suppresses `rtc_resume()`.
A counter-sourced injection under-measures by exactly the interval the vCPUs were stopped
outside the suspend window; an RTC-sourced one (host-anchored) would have been exact.
Guest-side confirmation: `current_clocksource` = `arch_sys_counter`.

**The quiesce oracle releases the ack before the vCPUs are parked.** `Vmm::is_quiesced`
reads virtio device status (`vmm/src/lib.rs:521`), which the guest clears in its `.suspend`
callbacks during `dpm_suspend()`. Timekeeping freezes later, inside `machine_suspend` —
after `dpm_suspend_late`/`_noirq` and after the `s2idle_enter` rendezvous in which *every*
vCPU must be scheduled to reach `tick_freeze()`. Measured on an idle host with all six
vCPUs promptly schedulable, that leg is **0.28 ms**:

```
dpm_suspend       [2] end    1254.623297   <- what is_quiesced observes
dpm_suspend_late  [2] end    1254.623475
dpm_suspend_noirq [2] end    1254.623577
machine_suspend   [1] begin  1254.623577   <- timekeeping freezes here
```

That number is a floor, not the exposure. The rendezvous is unbounded when vCPU threads
are not promptly scheduled — which is the condition at host sleep, as macOS winds down.
So the ack is released into a window that is sub-millisecond when the host is idle and
arbitrarily long when it is not: a race biased heavily toward winning, lost occasionally.
The tester's guest took **23 s** from suspend entry to device quiesce where this host took
47 ms, which is what the unfavourable regime looks like.

The consequence is that `is_quiesced` is the wrong release signal. The event that must
have happened is the rendezvous completing — observable host-side as every vCPU halted
with no vmexits, which works on a stock guest.

## Distro dependence

The mechanism is distro-independent; the *visible damage* is not. Fedora 44 (systemd 259)
ships **no** `WatchdogSec` on journald/udevd/logind — `WatchdogUSec=0`. Debian adds
`WatchdogSec=3min`. A Fedora guest therefore absorbs the same monotonic jump silently.
Reproducing the failure on Fedora needs the drop-ins this spike installs.

Killing journald alone is survivable. The cascade needs **logind**: its death orphans the
DRM master and input leases, every new `gnome-shell` then fails `/dev/dri/card0` with
`EBUSY` → `Failed to setup: No GPUs found` → gdm respawns forever, and NetworkManager
never receives `PrepareForSleep(false)` so the network stays down.

## Files

- `clocklog.py` / `clocklog.service` — 1 Hz REALTIME/MONOTONIC/BOOTTIME logger.
- `stop-window.sh <worker-pid> <secs> <ssh-port>` — the vehicle. Refuses to signal a
  worker whose command line does not name this spike's disk.
