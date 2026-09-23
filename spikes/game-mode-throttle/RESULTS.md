# Game Mode throttle: what it clamps, and how a worker escapes it

**Question.** While macOS Game Mode is on (a games-category app fullscreen and frontmost; on the
dogfood Mac that app is Moonlight), every thread of `limina-vmm` sits at priority 4 and guest
video and audio stutter. What exactly is throttled, and can limina keep its worker out of it?

**Answer.** Game Mode clamps **processes that belong to an app's process tree**, and it clamps them
hard. A process that launchd starts outside any app is not touched. The worker is spawned by the
supervisor, an AppKit app, so it inherits the clamp. The same code run as a gui-domain LaunchAgent
(`ProcessType=Interactive`) stays at priority 31 throughout Game Mode, with an unbroken CoreAudio
render cadence. Nothing done *inside* a clamped process lifts the clamp.

## Vehicle

- `bait.swift` + `Info.plist`: a stand-in game (`public.app-category.games`,
  `LSSupportsGameMode=true`, the keys Moonlight's Info.plist carries). It goes native-fullscreen,
  draws with Metal, and quits by itself (`bait [secs] [burn N] [fifo PATH]`). `burn N` spins N
  game-side threads, and `fifo` writes `mach_absolute_time` into a FIFO every 5 ms.
  `gamepolicyd` logs `Game mode enabled` within about 0.4 s of launch every time.
- `probe.m`: a Regular-policy AppKit parent with a visible window (the supervisor's shape) that
  posix_spawns a windowless child (the worker's shape): four 10 ms sleepers and one
  `THREAD_TIME_CONSTRAINT_POLICY` thread with the band's defaults. With `--child-exe` it spawns
  `wake` instead. It reports thread priorities (`pth_curpri`), the darwin role and DARWIN_BG, and
  can hold `NSActivity` assertions or run guards that reset role/DARWIN_BG.
- `wake.m`: a windowless process that measures every way a thread can be woken or run. `fifo` is an
  event wake from the bait (recv − sent). `plain` is `mach_wait_until`, 10 ms. `kqcrit` is a kqueue
  timer with `NOTE_CRITICAL`. `dstrict` is a dispatch timer with `DISPATCH_TIMER_STRICT` and zero
  leeway. `busy` is a spinning thread's CPU share and its longest stretch off-CPU. `au` is the
  interval between AUHAL render callbacks, playing silence.
- `run.sh <outdir> [arm...]`: 6 s baseline, 15 s of bait, then the tail. `summarize-wake.py`
  reduces a `wake` log to Game-Mode-window (t = 8–20 s) vs outside: the median of per-second p50s
  and the worst per-second p99, in µs.

Measured 2026-09-23 on the dev Mac (M1 Max, macOS 26.6.2). Raw output: `results-activities.txt`,
`results-guards.txt` (probe arms), `results-lineage.txt`, `results-lineage-audio.txt` (wake arms).

## Results

**1. Nothing in-process lifts the clamp** (probe arms). With no assertion, with
`NSActivityUserInitiated` or `UserInitiated|LatencyCritical` in the child or the parent, and with
guards resetting the darwin role or DARWIN_BG, every thread of parent and child went to priority
4 within a second of `Game mode enabled` (the RT thread from `97`). Role and DARWIN_BG read back
unset throughout, so the guards never fired. xnu applies Game Mode through the coalition's thread
group (`task_coalition_thread_group_game_mode_update`, `osfmk/kern/task_policy.c`). Setting a
task's game-mode flag needs the private `com.apple.private.set-game-mode` entitlement
(`proc_set_game_mode`, `bsd/kern/kern_resource.c`).

**2. What the clamp does** (`wake`, spawned by an AppKit parent, Game Mode on, no game load):

| wake kind | clamped: p50 / worst p99 | started from the shell, Game Mode on (unclamped) |
|---|---|---|
| plain `mach_wait_until` | 100 ms / 127 ms late | 2.5 ms / 2.5 ms |
| event (FIFO) | 1.1 ms / 40 ms | 0.05 ms / 1.1 ms |
| kqueue `NOTE_CRITICAL` | 1.3 ms / 43 ms | 0.05 ms / 3.4 ms |
| dispatch strict, 0 leeway | 1.2 ms / 41 ms | 0.06 ms / 0.2 ms |
| busy thread | 55% CPU, 93 ms off-CPU at worst | 100%, 3.5 ms |

Two mechanisms, then. Timer coalescing stretches plain sleeps to about 100 ms
(`kern.timer_coalesce_bg_ns_max` is 100 ms), and strict/critical timers or event wakes avoid that.
On top of it the process is denied CPU, which no wake style avoids. With AUHAL on in the clamped
process, the render callback (10.67 ms nominal) went **1.09 s** without running at worst, and the busy
thread went 1.9 s. Audio buffering on our side cannot bridge that. (The first probe arms saw
threads wake ~200 ms late at 5–6 wakes a second; that was the same clamp, measured on plain timers.)

**3. Lineage decides who is clamped** (Game Mode on in every row):

| how `wake` was started | priority | busy CPU | audio callback p50 / worst p99 |
|---|---|---|---|
| spawned by an AppKit app (the worker's shape) | 4 | 55% | 10.67 ms / **1094 ms** |
| gui-domain LaunchAgent, `ProcessType=Interactive` | 31 | 94% | 10.67 ms / 10.72 ms |
| from a shell whose tree descends from a launchd job, not an app | 31 | 100% | 10.67 ms / 10.81 ms |
| `launchctl submit` job | 20, with or without Game Mode | 100% | (not measured) |

The `launchctl submit` job is never clamped, but it runs at priority 20 with plain timers ~40 ms
late all the time: the default job type is not an interactive one. Declaring
`ProcessType=Interactive` is what makes the LaunchAgent row normal.

A system-wide `ps -M -A` survey during Game Mode agrees: Finder, Safari, Spotlight,
SystemUIServer, Tailscale, the Limina control center, and the `login`/`fish`/`ssh` processes
under a terminal app all went to 4. System daemons, Apple's system UI (Dock, WindowManager,
ControlCenter, NotificationCenter, loginwindow), and processes descended from non-app launchd jobs
did not.

**4. Game-side load then competes on priority alone.** Unclamped, with the bait spinning 8 threads
on this 10-core host, the worker's event wakes went to 1.2 ms p50 / 26 ms p99 and the busy thread
to 87% CPU. That is ordinary contention, and the audio callback stayed at 10.67 / 10.72 ms.

## What this means for limina

- **The fix is where the worker runs, not what it does.** Launch the worker as a LaunchAgent (or
  anything launchd starts outside the app's tree) with `ProcessType=Interactive`, rather than
  posix_spawning it from the supervisor. Costs to weigh: launchd does not pass file descriptors, so
  the supervisor⇄worker channels need a rendezvous (booked separately as moving the control sockets
  to Mach ports). Registering an agent via `SMAppService` shows the user a Login Items/background
  approval. TCC attribution (mic, camera) moves to the worker's own identity.
- **The supervisor stays clamped** under Game Mode, because it is the app with the window. Its
  present path into that window will still run late. Measure how much that alone costs once
  the worker is out.
- Short of that: strict or critical timers remove the ~100 ms timer slack but not the CPU denial,
  so they help while the clamp lasts and do not fix it.
- The game-side levers still work: the menu-bar Game Mode toggle (it held across re-fullscreens on
  the dogfood Mac), or `LSSupportsGameMode=false` in apps the user builds.

## Traps

- Game Mode clamps **terminal sessions and `ssh`** too, so a probe driven from a terminal app is
  itself clamped. The measurements here come from a shell outside any app's tree, and every
  number is self-timed by the thread it describes.
- A `launchctl submit` job is `KeepAlive` by default and restarts when it exits; remove it by
  label (`run.sh` does).
- A host under memory pressure distorts every number here. Check `vm.swapusage` and
  `memory_pressure` before a run; one batch was discarded for that reason.
- A boot suite running during a bait run is clamped with everything else. `run.sh` warns.
