# Game Mode throttle: can limina keep its worker out of it?

**Question.** While macOS Game Mode is on (a games-category app fullscreen and frontmost; on the
dogfood Mac that app is Moonlight), every thread of `limina-vmm` sits at priority 4 and guest
video and audio stutter. Can the worker or the supervisor opt out: with an `NSProcessInfo` activity
assertion, or by undoing the demotion itself?

**Answer: no, not from inside a user-domain process.** Game Mode clamps almost every user process
on the Mac: Finder, Safari, terminal shells, `ssh`, Tailscale and our control center all go to
priority 4 with it. No activity option lifted the clamp. The process's darwin role and its
external DARWIN_BG flag both read back as unset throughout, so there was no demotion for the
guards to undo. The levers left are on the game's side or outside the user domain (below).

## Vehicle

- `bait.swift` + `Info.plist`: a stand-in game (`public.app-category.games`,
  `LSSupportsGameMode=true`, the same keys Moonlight's Info.plist carries). It goes
  native-fullscreen, draws with Metal, and quits by itself after 15 s. `gamepolicyd` logs
  `Game mode enabled` about 0.4 s after launch every time.
- `probe.m`: the supervisor's shape (a Regular-policy AppKit app with a visible, non-key window)
  posix_spawns the worker's shape: a windowless child with four 10 ms sleeper threads (the
  vCPUs) and one `THREAD_TIME_CONSTRAINT_POLICY` thread with the band's defaults (60 Hz, 1 ms,
  2 ms). Once a second each process reports every thread's `pth_curpri`, the RT thread's and a
  sleeper's wake lateness, and `getpriority(PRIO_DARWIN_ROLE)` / `getpriority(PRIO_DARWIN_PROCESS)`.
- `run.sh <outdir> [arm...]`: per arm, 6 s baseline, then 15 s of bait, then about 11 s after.
  It warns if a boot suite or a `limina-vmm` is running, because Game Mode clamps those too.

Measured 2026-09-23 on the dev Mac (M1 Max, macOS 26.6.2) with the host otherwise idle; raw output
is in `results-activities.txt` and `results-guards.txt`.

## Results

| arm | what the processes hold | threads while Game Mode is on | RT wake late, p50 (steady) |
|---|---|---|---|
| none | nothing | parent + child all `4` (RT thread `97` → `4`) | 193–200 ms |
| user | child: `NSActivityUserInitiated` | all `4` | 193–200 ms |
| latency | child: `UserInitiated \| LatencyCritical` | all `4` | 193–200 ms |
| parent-latency | parent: `UserInitiated \| LatencyCritical` | all `4` | 193–200 ms |
| guard-role | both: reset own darwin role if it reads DARWIN_BG | all `4`; role read `0`, 0 resets | 193–200 ms |
| guard-bg | both: clear external DARWIN_BG if set | all `4`; bg read `0`, 0 resets | 193–200 ms |

Outside Game Mode every arm reads `31` / RT `97` with an RT p50 of 11–17 µs and 60 wakes a second.
Inside it every thread wakes 5–6 times a second, about 200 ms late. That is a quantised throttle,
not contention, because the host was idle. Priorities flip within a second of `enabled` and of
`disabled`.

**System-wide survey** (`ps -M -A` before and 6 s into a bait run): 317 processes had every thread
at `4` during Game Mode, and 45 of them had none at `4` before. The 45 are ordinary user-domain
processes: Finder, Safari, Spotlight, SystemUIServer, Tailscale, terminal `login`/`fish`, `ssh`, a
third-party app and its server, and the Limina control center. Unclamped: system-domain daemons,
and Apple's system UI (Dock, WindowManager, ControlCenter, NotificationCenter, loginwindow).

## Why nothing in the process can undo it

xnu applies Game Mode through the coalition's thread group (`task_coalition_thread_group_game_mode_update`
in `osfmk/kern/task_policy.c`), not through the task role or DARWIN_BG, and our readback shows
both untouched. Setting a task's game-mode flag needs the private `com.apple.private.set-game-mode`
entitlement (`proc_set_game_mode`, `bsd/kern/kern_resource.c`). An activity assertion only affects
App Nap and timer coalescing, and neither of those is what clamps here.

## What is left

1. **The game's side (works today):** turn Game Mode off from the menu-bar icon while the game
   is fullscreen (on the dogfood Mac the choice held when Moonlight went fullscreen again), or
   build Moonlight with `LSSupportsGameMode=false`. Neither is a limina change.
2. **Out of the user domain (untested):** system-domain daemons were not clamped. A root-launched
   VMM is the privileged-helper question, not something to take on for this alone.
3. **Degrade knowingly:** detect Game Mode (priority 4 on our own threads is a direct oracle) and
   tell the user why the VM stutters, instead of leaving it unexplained.

## Traps

- Game Mode clamps **the terminal and `ssh` sessions too**, so a probe driven over ssh while a game
  is fullscreen is itself throttled. Take measurements from threads that record their own
  timestamps (as the probe does), not from a poller.
- A boot suite running during a bait run is throttled with everything else. `run.sh` checks for one.
