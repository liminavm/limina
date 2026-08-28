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

**The quiesce oracle fires one PM stage early.** `Vmm::is_quiesced` reads virtio device
status (`vmm/src/lib.rs:521`), which the guest resets to `INIT` in its `.suspend`
callbacks during `dpm_suspend()`. `timekeeping_suspend()` runs later, in
`syscore_suspend()`. So `power.rs` releases the macOS sleep ack while the guest still has
syscore ahead of it, and any time macOS takes to actually stop the vCPUs after that lands
in monotonic. This is structural, not a lost race.

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
