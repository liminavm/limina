# Local gnome-shell-rs rig (dev-mac) — prep for the 2026-07-29 regression hunt

2026-07-29. The compositor side's §21/§22 (dogfood-guest
`~/Projects/gnome-shell-rs/docs/fork/present-misses.md`) reports regressions on
the VMM deployed to the dogfood that day. Rather than ping-ponging measurement
requests to their side — whose host also runs other workloads, so their runs
carry interference we can't control — we run the *same* driver + scorer on a
local VM on the dev Mac (dev-mac), which is otherwise idle.

## What their report gives us (read 2026-07-29)

Three quantified factors from one day of controlled A/Bs, all against the §19
post-0051 baseline, plus a wedge:

1. **VMM present path ~5x worse** (binary+scale held): miss rate 1.11% → 5.77%
   at matched scale, and the *same binary's* gpu p50 went 5.53 → 7.34 ms
   (+33%). Their first ask: verify the deployed build actually contains virgl
   0049 (relax ladder) + 0051 (two-lane journal) — read-only oracle on dogfood-mac:
   `sample <worker-pid> 1 | grep vkr-journal` (thread exists only on 0051+).
   The whole shape is consistent with a build that predates both.
2. **Scale-1 anomaly**: same binary, same boot, one scale flip — scale 1 misses
   3-4x MORE on *cheaper* frames than 1.5/2. Only guest-side difference is the
   identity logical→physical mapping. Smells like the host present path taking
   a different branch when logical == physical (direct-scanout vs composite?).
3. **Episodes**: under a stationary guest workload, 10-30 s stretches where gpu
   p90 inflates 3-5 ms and misses jump ~8-9% → 35-50%, separated by 20-40 s of
   calm; nothing guest-side accumulates (RSS/draws/elements flat). What
   host-side activity has a 10-30 s duty cycle? (DVFS, journal consumer,
   BO-cache trim, CA housekeeping…)
4. **§22 wedge (a real VMM bug, severity ceiling of the same disease):** a
   context that dies with an in-flight fence leaves that fence UNSIGNALED
   forever — the guest's pending atomic flip (`IN_FENCE_FD` under
   `NIRI_VK_ASYNC_SCANOUT=1`) waits in `commit_tail` forever, serializing all
   of KMS behind it: greeter black, VT switching dead, only a VM reboot
   recovers. vkr context destruction must retire/signal all in-flight fences.
   (They will also harden their logout path, but the dma-fence contract is
   ours to honor.)

Timeline for their cells: guest boot `a8a1fbce`, journal 14:26-15:34; run
ledger = their `present-misses-runs.md`.

## The rig

- **Repo**: cloned at `third_party/gnome-shell-rs` (gitignored), from
  `user@dogfood-guest:Projects/gnome-shell-rs`, at `283985c8` (their main incl.
  §21/§22 and both driver scripts). Update with `git pull` (host has the
  tailnet route; guests don't — push code INTO the guest over the forwarded
  ssh port).
- **Image**: `nirirepro.raw` (42 GiB, 2026-07-11 clone of the user's Dev VM
  lineage with the gsrs test session installed). It predates their current
  main by ~3 weeks.
- **Bring-up** (after the streamlining wrap-up):
  1. Clone the image (boot-in-place mutates); boot EFI+venus with `--net`.
  2. rsync the host clone into the guest gsrs checkout; `cargo build` (their
     session drop-in runs `target/debug` directly — `PROFILE=release` for the
     release build; rebuild+relogin is the whole iterate loop).
  3. If the drop-in is missing/stale in the image:
     `sudo scripts/install-test-session.sh`.
  4. Log the gsrs user into the GNOME session (GDM user switch on the VM
     window), then drive over ssh:
     `scripts/drive-workload.sh <SEAT_UID> <WORKSPACE> heavy` with
     `NIRI_FRAME_LOG=all,gpu` (+ `NIRI_VK_ASYNC_SCANOUT=1` to match their
     cells), score with `scripts/correlate-frame-log.py` on the aim-1 tag.
     The scorer REFUSES arms whose element/bake counts disagree — keep it
     that way; it's what catches contaminated A/Bs.
  5. Match their constants where relevant: display 3840x2160@59.996 (pin via
     monitors.xml / display-modes; the scale-1 vs 1.5/2 axis is a live
     gsettings flip), fence-present default, no VN_PERF.
- **Matching their arms**: their binary axis is `b808c5bb` (the §19 baseline,
  smithay pin `e1c10415`) vs `d4c7a61d` (main). Both are in the clone's
  history; build either.

## Order of attack (queued behind the streamlining wrap-up)

1. Read-only on dogfood-mac: confirm what the deployed VMM build contains (their
   ask #1) — if it predates 0049/0051 the "5x" may mostly be a stale-build
   artifact, which reshapes everything else.
2. Fix the §22 fence leak (vkr context destroy → retire in-flight fences) —
   RED-first; likely reproducible with an L2 test that kills a venus client
   holding an in-flight fence and asserts the fence still retires.
3. Local rig up; reproduce their §21.2 table shape on dev-mac (no interference)
   before chasing the scale-1 and episode signals.
