# Local gnome-shell-rs rig (dev-mac) — prep for the 2026-07-29 regression hunt

> **Status 2026-07-30: KEPT as a standing dev tool.** `nirirepro.tear.raw` is the
> rig image (user's call): gsrs autologin (password `gg`), current gsrs main +
> smithay synced under `/home/claude/{gnome-shell-rs,smithay}` (rsync from the
> host clones + `cargo build` to update), test-session drop-in installed, grim +
> vulkan-loader-devel present. Boot:
> `LIMINA_DISK=$PWD/nirirepro.tear.raw LIMINA_NET=1 LIMINA_RAM_MIB=8192 LIMINA_EXTRA_ARGS="--display-resolution 3840x2160" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`
> (ssh claude@127.0.0.1, auto-port from 2222). Quick compositor checks:
> `cargo test -p niri-vk explicit_sync_bridge` in-guest;
> `scripts/drive-workload.sh $(id -u gsrs) 1 mixed|heavy` against the seat.

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

## 2026-07-29 evening: ask #1 ANSWERED — the deployed KK was the instrumented build

Read-only strings check on dogfood-mac's `/Applications/Limina.app` (deployed 12:18):

- `libvirglrenderer.1.dylib` **has** `vkr-journal` + the `q_peak` gauge —
  0049/0051/0052 are in; the stale-build theory is dead.
- `libvulkan_kosmickrisp.dylib` **carries the uncommitted PBO-hunt
  instrumentation** (`LIMINA_KK_DRAWPROBE`, `LIMINA_KK_BUFVIEW_FROM_MAP`,
  `[KKTRACE] desc-sampled NULL-VIEW`): the bundle was built while the mesa-cs
  tree still had it applied. Measured on the 10k-draw storm, the per-draw
  getenv alone is ~10% of wall on the encode path — sitting exactly between
  "flip queued early" and "render fence signals". **Their §21 "VMM axis" A/B
  was old-VMM-with-clean-KK vs new-VMM-with-instrumented-KK.**

**Clean bundle built: `target/Limina.app`** — KK rebuilt pristine at the 0016
commit (deliberately WITHOUT kk 0017 threaded submit, to avoid stacking a
second present-path change into their A/B; verified 0 instrumentation strings),
virgl/worker at current repo HEAD (delta vs the morning deploy: libkrun 0116 +
virgl 0057 vrend fence honesty — inert for a venus-only guest). Deploying to
dogfood-mac is the user's step. After it lands, ask the compositor side to re-run the
§21.2 matched-scale cell; scale-1 + episodes only stay on the list if they
survive the clean build.

## 2026-07-29 late: tearing report → fence truthfulness MEASURED (spike venus-fence-truth)

The user saw background bleeding through windows mid-overview-animation on the
dogfood re-run (mixed animation frames in bands = tearing). Under
`NIRI_VK_ASYNC_SCANOUT=1` that means the buffer hit glass before its render
finished. Measured (spikes/venus-fence-truth/): the venus **fence sync_file is
truthful** (exported pending at 0.02 ms, signals at GPU completion), and the
flip/release side is engineered honest (guest kernel fences blob-scanout flush;
host holds it to the CA-latch ack). Two host-side suspects ELIMINATED. Also
found: `vn_GetSemaphoreFdKHR` **CPU-blocks until GPU completion** on our stack
(no implicit fencing → `vn_wsi_sync_wait`) — if gsrs exports the render
*semaphore* per frame for IN_FENCE, their compositor thread serializes on the
GPU every frame (latency/misses, not tearing); the *fence* export is the async
path. Tearing suspects remaining: gsrs buffer/fence pairing or damage in the
overview animation (incl. syncobj wait-for-submit handing back a null/signaled
fence), and the scale-1 identity-mapping present branch (§21.2 — the artifact
appeared in exactly that cell). Both are rig work below.

## RESOLVED 2026-07-29 night: the tearing + bridge-test failure = kk 0017 shipped by accident

The 16:42 "clean" bundle's KK dylib was NOT the 0016 build it was declared to be:
`strings` on the deployed artifact shows `LIMINA_KK_SUBMIT_THREAD` and the git
stamp `git-70ead9445f` (the 0017 commit). **kk 0017 threaded submit rode the
dogfood deploy with the knob default-ON** — the exact stacking the deploy-caution
note forbade; the earlier "verified 0 SUBMIT_THREAD strings" check was wrong.

Local rig A/B (same repo-HEAD worker/virgl both arms, only the KK env flipped),
via the compositor side's own `cargo test -p niri-vk explicit_sync_bridge`:

- `LIMINA_KK_SUBMIT_THREAD=0`: PASS — fence→sync_file pipelined, export 0.05 ms,
  waits track the ~200 ms busy-work. Matches their healthy baseline AND our
  fdtruth probe (venus fence chain honest).
- knob default ON (= dogfood): **FAIL** — fence exported unsignaled then
  signaled in 0.1 ms with ~200 ms of GPU work queued. **The fence lies early
  under threaded submit.** Mechanism STILL OPEN: the obvious theory (empty
  QueueSubmit(fence) not ordering behind prior submissions) is DISPROVEN —
  `spikes/venus-fence-truth/emptysub.c` and fdtruth are both GREEN knob-ON;
  only the bridge test's multi-stage sequence triggers it (see the RED
  inventory in spikes/venus-fence-truth/RESULTS.md). Early flip fences under
  zero-copy scanout = the overview-animation tearing.
  Also their §-report "export blocks ~240 ms, emulated/serialized" (the ~240 ms
  is their calibrated busy-work D seen through the always-blocking semaphore
  export — see finding below).

This also exonerates libkrun 0116 / virgl 0057 (identical in both arms).

**Remediation:** true-0016 bundle rebuilt from a fresh worktree
(`git-ac5fccbe84`, 0 SUBMIT_THREAD/instrumentation strings verified ON THE
BUNDLE ARTIFACT). The root cause was then found and fixed as kk 0018
(recycled-binary-fence reset raced the in-flight GPU signal — see
`patches/kosmickrisp/README.md`), and the full battery passed 2026-07-30
(bridge test knob-ON, crossmark hashes, drawstorm A/B, seated eyeball, HVF
suite 72/72), so 0017+0018 threaded KK is now the shipping configuration.
`build-app.sh`'s tripwire was retargeted: it refuses a dylib carrying
SUBMIT_THREAD *without* the 0018 fix (marker: `LIMINA_KK_SYNCTRACE`) — the
exact build that tore on dogfood — and still refuses debug instrumentation
(`LIMINA_ALLOW_THREADED_KK` gate removed).

Standing guest-mesa finding (pre-existing, NOT deploy-related):
`vn_GetSemaphoreFdKHR` CPU-blocks until GPU completion (`vn_wsi_sync_wait`; no
implicit fencing on our renderer). For per-frame IN_FENCE export the FENCE
export is the pipelined path — their spike already encodes this.

## Order of attack (queued behind the streamlining wrap-up)

1. ~~Read-only on dogfood-mac: confirm what the deployed build contains~~ DONE, see
   above — deployed KK was instrumented; clean bundle awaiting deploy.
2. Fix the §22 fence leak (vkr context destroy → retire in-flight fences) —
   RED-first; likely reproducible with an L2 test that kills a venus client
   holding an in-flight fence and asserts the fence still retires.
3. Local rig up; reproduce their §21.2 table shape on dev-mac (no interference)
   before chasing the scale-1 and episode signals.

## 2026-07-30: #18 verdict — NO virgl regression for the real workload; the "regression" was kitty + a contaminated baseline

Rig A/B campaign (measure-arm.sh, their driver + correlator, 4K@59.996):

| arm | workload | overall miss rate |
|---|---|---|
| virgl 0052 / 0055 / current | ptyxis windows, idle GPU | 0.01% / — / 0.06% |
| virgl 0052 / 0055 / current | ptyxis + 3×drawstorm hogs | 0.01% / 0.03% / **4.43%** |
| virgl 0052 / current / current−0056 | **kitty** windows, no hog | 5.21% / ~5.4% / ~5.6% |

- **The dogfood "present regression" table does not indict the VMM delta.** With the
  real trigger (kitty — a GPU-rendered terminal; the user isolated it: kitty sluggish,
  gnome-terminal fast), §19-era virgl scores the SAME ~5% as current. Their comparison
  baseline ("§23 clean deploy" 0.04%) was the accidental kk-0017 build: threaded submit
  is genuinely faster AND its early fences fake punctuality. Honest-vs-honest, nothing
  regressed for real sessions.
- **kitty performance on this stack is its own real issue**: gpu p50 4.5ms/max 10.7ms at
  4K for terminal windows drives honest ~5% miss rates during animations. Likely what
  §21's "episodes" measured all along (bursty GPU terminals in the seat). Follow-up
  candidate: why kitty frames cost so much through venus.
- **UNRESOLVED anomaly**: under 3×drawstorm contention, current virgl scored 608 misses
  vs 0055's 4 and 0052's 1 — implicating 0056 (lookup cache) — but current−0056 did NOT
  fix the kitty case, and the drawstorm arms' draws-coverage diverged (130-200-draw
  windows only in the bad arm). Needs a repeat with pinned comparability before trusting;
  parked in task #18.
- Protocol lesson for both sides: **pin the terminal app** in workload arms — the
  element/bake comparability guard does not catch terminal-type divergence.
- Rig traps: replace prefix dylibs with `rm` + `cp` (in-place overwrite of a signed dylib
  → the worker dies by SIGKILL at load: stale vnode signature cache); wait ~25 s after
  guest `poweroff` before relaunching (teardown race kills the new worker); pull guest
  /tmp logs IMMEDIATELY (tmpfs, wiped per boot — two arms' logs were lost).

## 2026-07-30: the dogfood-mac A/B arms (their §25/§26 — client-independent misses, mouse-motion masking)

Their §25 (client A/B: 8× gnome-terminal shm-only misses MORE than 8× kitty, identical
all-queued-early signature) and §26 (fresh-boot shm-only 7.17%; mouse motion MASKS the lag →
suspect = completion-feedback delivery, episodic) overturned the kitty-as-mechanism reading:
kitty is only a frame generator plus a second symptom (dmabuf commits gate on fence polls).
The rig does not reproduce their magnitude (ptyxis 0.06% vs their shm 7-16%), so the A/B
moves to dogfood-mac: we build, the user deploys/tests, then we diff dogfood-mac-vs-dev-mac directly.

Two artifact-verified arms under `target/ab-bundles/` (user deploys; score with their
sustained heavy protocol + the §26 park-the-mouse oracle):

- **`Limina-0727-baseline.app`** — the §19-era punctual deploy, rebuilt from repo `8b22694`
  (07-27: fence-present 0110-0114, virgl @0052, KK @kk-0013 `git-ebc07301ba`, worker built in
  the `limina-0727` worktree from era-vendored sources). Brackets the ENTIRE 07-27→07-29 delta.
  If it scores like current → no regression ever; the honest costs were always there and the
  §23 punctuality was the kk-0017 lie. If it's punctual → real regression inside the delta
  (then bisect: kk 0015/0016, libkrun 0115, virgl 0053-0057 — 0116 unlikely, see below).
- **`Limina-no0116.app`** — current deploy minus libkrun 0116 only (KK = true-0016
  `git-ac5fccbe84`, virgl @0057). Sanity arm: 0116's fence routing only arms on a vrend
  (non-venus) context (`vrend_ctx_seen`), and a seated niri desktop creates none (rig oracle,
  debug logs) — so this arm SHOULD measure identical to current. If it doesn't, the
  inertness reading is wrong and 0116 is the bug.

**RESULT 2026-07-30 morning — the baseline arm KILLS the regression theory.** The user
deployed `Limina-0727-baseline.app` on dogfood-mac, booted dogfood-guest, and reported the sluggish
workspace switch visually unchanged. Scored run in the dogfood-guest gsrs session (their §26
protocol: 8 gnome-terminals, heavy ×2, their correlate-frame-log.py): **15.11%** overall
(2146/14200 aim-1 flips, 28 qualifying windows, draws p50 161, gpu p50 10.07 ms) — the same
band as their current-deploy runs (§25 16.28%, §26 7.17%; ~2× run-to-run variance). The
§19-era VMM build cannot reproduce the §19-era punctuality, so **nothing in the 07-27→07-29
VMM delta caused this** (and `Limina-no0116.app` is moot — 0116 was a subset of what this
arm reverted — no need to run it). What the §19-vs-now comparison still spans: the COMPOSITOR build and the
workload/scorer themselves evolved since 07-27 — their side's ledger, flagged to them.
Secondary structure in the run: gpu-p50 12 ms+ windows miss 65.4%, 6-12 ms miss 10.9%,
draws 200+ miss 34.6% — frame cost is the dominant lever (rho elements +0.845, draws +0.780).
Prime dogfood-mac-vs-rig suspect: the dogfood seat runs 4K at **fractional scale 1.5** (the
`limina-perf-display-pinning` trap: fractional scale multiplies the render workload), where
the rig ran integer scale — likely why dogfood-mac's compositor frames sit at 10-12 ms against a
16.7 ms budget while the rig idles at ~2 ms. Next: reproduce the band on the rig by pinning
4K + scale 1.5 (task #19), then attack venus per-frame cost, not a phantom regression.

**2026-07-30 late morning — the rig reproduces the dogfood-mac band; the story closes.** Two rig
corrections first: (a) the rig had been booting the `build-kk` ICD rebuilt at the kk 0017
commit, whose threaded submit **defaults ON** (`debug_get_bool_option(..., true)`) — the user
saw tearing live in the first 4K run, and every rig number since that rebuild was
fence-lie-flattered (the virgl A/B *deltas* stand — all arms shared the KK — but absolutes
were low). Rig boots now pin
`LIMINA_KK_ICD=/Volumes/mesa-cs/build-kk-0016/.../kosmickrisp_0016_icd.json` (hand-written
ICD json; the 0016 build dir has no devenv json) — verify with `lsof -p <worker> | grep
kosmickrisp`. Tearing gone (user eyeball). (b) The rig display had never been pinned to the
dogfood shape; at `--display-resolution 3840x2160` niri auto-picks scale **2.25** (no output
stanza in the synced config — worth asking how dogfood-guest gets 1.5); pinned via an
`output "Virtual-1" { mode "3840x2160@59.996"; scale 1.5 }` stanza (niri live-reloads config,
**NOTE: the stanza lives only in booted clones, NOT in `nirirepro.tear.raw` itself** — every
fresh clone comes up at auto-scale 2.25 and needs the stanza re-appended before measuring,
no mutter dialog trap; gnome-terminal installed for terminal-pinning parity).

Honest run (0016 KK, 4K @ 1.5, 8 gnome-terminals, heavy ×2, their scorer): **31.93%**
(4074/12761 aim-1 flips), perfectly bimodal — draws 90-130 / gpu ≤12 ms windows **1.43%**;
draws 200+ / gpu-p50 12 ms+ windows **92.06%** (gpu p50 6.25 ms, max 16.96 ms). Dogfood-mac same
protocol scored 15.11%. The rig (M1 Max) overshoots dogfood-mac (M4 Pro) once the workload shape
matches — pure GPU frame cost against the 16.7 ms budget. VERDICT for their §25/§26: no
delivery bug required; honest fences + 4K-fractional-scale frame cost + episodic contention
explain the misses, the mouse-motion masking, and the old dogfood-mac-vs-rig split (earlier rig
runs = smaller effective workload + the lying KK). Next lever: venus per-frame GPU cost of
the compositor render at scale 1.5 (12-17 ms for 8 terminal windows in a debug niri is the
thing to attack) — and their side may want to reconsider render-scale choices. Trap repeat:
the knob-ON scale-1.5 log was lost to the guest-/tmp-tmpfs wipe on reboot (pull immediately).

## 2026-07-30 evening: the host A/B (task #23) — the machine axis is REAL and INVERTED

One artifact (the 0118 bundle: libkrun 0117+0118, virgl @0057, KK true-0016 `git-ac5fccbe84`,
artifact-verified), one image (fresh clones of `nirirepro.tear.raw` on each host), one protocol
(4K@1.5 pinned, 8 gnome-terminals on ws 1, heavy ×2, their correlate-frame-log.py). Dogfood-mac ran
/tmp-confined (image + bundle shipped to `/tmp`, dogfood-guest untouched), quiesced (the user's
disk-move finished + external unplugged before the drive; verified via iostat).

| arm | overall | cheap band | draws-200+ band | gpu p50 min–max |
|---|---|---|---|---|
| dev-mac (M1 Max, idle) | **28.73%** (3626/12619) | 0.02% | **83.31%** | 3.42–**17.19 ms** |
| dogfood-mac (M4 Pro, daily driver) | **16.28%** (2326/14291) | 0.00% | **38.40%** | 5.76–**10.71 ms** |

- **The rig transplant is faithful: dogfood-mac-rig 16.28% ≈ dogfood-guest's own §25/§26 numbers**
  (15.11% / 16.28% / 7.17%). The dogfood miss rate is a property of the machine + stack at this
  workload, not of dogfood-guest's guest, their compositor build, or GNOME-vs-niri.
- **The "daily-driven M4 vs idle M1" theory is DEAD — inverted.** The idle M1 Max misses ~1.8×
  MORE. The lever is raw GPU throughput: the M4 Pro keeps the expensive transition frames at
  ≤10.7 ms (under the 16.7 ms budget, misses when compounded), while the M1 Max pushes the same
  frames to 17.2 ms (over budget → 83–85% missing). No load/interference residual left to
  attribute to daily-driving; dogfood-mac idle already reproduces its dogfood band.
- Confirms **#21 (venus per-frame GPU cost) as the one lever that matters** — and explains why
  dev-mac historically "couldn't reproduce" the dogfood feel: earlier rig runs used a smaller
  effective workload + the lying KK; at the matched workload dev-mac is *worse*, not better.
- **Comparability caveat, recorded honestly:** the cheap-phase draw counts differ (p50 76 on
  dev-mac vs 193 on dogfood-mac) at identical element counts (p50 91/90) — same scene, different
  damage/draw splitting, mechanism unexplained. The headline comparison leans on the overlapping
  heavy band (draws 200+, both arms span to ~360) and the gpu-time story, which agree.
- Journals: `/tmp/rig-ab-{dev-mac,dogfood-mac}-journal.log` on dev-mac (transient scratch).

Arm verification performed on the artifacts (the fd03b33 lesson): KK git stamp + zero
`LIMINA_KK_SUBMIT_THREAD`/instrumentation strings; worker grep for the 0116 oracle string
(absent in both arms) and fence-present (present in both). `scripts/build-app.sh` now takes
`LIMINA_KK_BUILD` to pin the KK build dir per arm. Era build recipes: mesa worktree
`/Volumes/mesa-cs/mesa-0013` + `build-kk-0013` (meson needs `/opt/homebrew/opt/llvm/bin` on
PATH — llvm-ar is baked into the ninja rules); limina worktree `limina-0727` vendored via the
three apply scripts directly (`cargo xtask vendor` chicken-and-eggs on the missing imago path;
`mkdir third_party` first, clone libkrun/virgl locally from the main checkouts).

## 2026-07-30 night: the build × scanout matrix (task #21 opening) — ASYNC-SCANOUT SCORES ARE LIES

Kicking off #21 (attack the 12-17 ms venus compositor frames) with a debug-vs-release axis
exposed something bigger. All cells: same 0118 bundle, fresh `nirirepro.tear.raw` clones on
dev-mac (M1 Max), 4K@1.5 pinned, 8 gnome-terminals, heavy ×2 (eyeball cell = heavy ×1), their
scorer. Build flipped via the session drop-in ExecStart (`target/{debug,release}/niri`),
scanout via `NIRI_VK_ASYNC_SCANOUT` in the override.conf.

| cell | tearing (human oracle) | overall miss | draws-200+ band | gpu p50 range |
|---|---|---|---|---|
| debug + async (morning) | "none" (unwatched?) | 28.73% | 83% | 3.4–17.2 ms |
| debug + async (eyeball, ×1) | **YES** | **6.63%** | — | — |
| release + async | **YES (heavy)** | 13.10% | 29% | 5.5–18.8 ms |
| **debug + sync** | no | **32.00%** | **99.53%** | 6.1–21.3 ms |
| **release + sync** | no | **28.58%** | **95.25%** | 6.1–21.2 ms |

- **The async-scanout fence race is real, guest-side, and poisons the metric itself.** Same
  binary tears with `NIRI_VK_ASYNC_SCANOUT=1`, is clean with 0; venus fence export is measured
  truthful (spikes/venus-fence-truth) — so the bad fence is minted in gsrs (their §21 syncobj /
  buffer-pairing suspect). A flip queued with a pre-signaled fence tears AND lands "on time",
  so an async arms score tracks **lie frequency, not punctuality**: the two debug+async runs
  scored 28.73% and 6.63% on the same day/machine/config — the watched one tore. The race fires
  in debug AND release (user eyeballs both; release tears more), and **on dogfood-mac too** (release
  gsrs + async on the freshly-deployed app) — machine-independent. Repro recipe: release build,
  async=1, heavy driver; flip async=0 → clean.
- **Every historical async-arm number carries unknown flattery** — todays host A/B absolutes
  (28.73/16.28), their §19 "1.11%" baseline, §25/§26 (7-16%). Qualitative structure survives
  (client-independence, frame-cost dominance — the sync cells show the same gpu-band cliff),
  but cross-era magnitude comparisons through async cells are void. Sync cells are the
  only honest instrument until their fence pairing is fixed.
- **The honest #21 baseline (M1 Max)**: debug 32.00% / release 28.58% — the build axis is real
  but small (~3.4 points). The dominant fact is unchanged and now clean: heavy transition
  windows run **12–21 ms of GPU** against the 16.7 ms budget and miss ~95-100%; the cheap-band
  floor is ~6 ms. The GPU cost of compositing 8 terminals at 4K@1.5 is THE lever. Beautiful
  reproducibility across sync cells (gpu ranges 6.07–21.16 vs 6.08–21.33).
- Sync-cell caveat: sync gating roughly halves the flip count (5.8k vs 14k aim-1 per ×2 run) —
  windows are coarser, rates hold.
- Journals on dev-mac /tmp: `rig-21-{release,release-sync,debug-sync,debug-async-eyeball}-journal.log`
  (transient; scores + tables are the record).

## 2026-07-30 late: #24 off-glass ack gating — the fix, the probe, and an A/B that measured its cost as zero

The zero-copy reuse tear got its mechanism fix. Ground truth first
(`spikes/present-pacing/`, host-only probes, no VM): **WindowServer holds an IOSurface
use count on plain `CALayer.contents` surfaces, and the surface a frame REPLACES stays
in use p50 17.1 ms / p90 24.3 ms / max 32.9 ms past the CATransaction completion
block** (in-use at completion-block time in 19/20 single-shot rounds). Our shown-ack
fired from that completion block, and the worker completes the guest's held flush
fence on the ack — so the guest got flip-completion ~one refresh before WindowServer
stopped sampling the buffer it was now free to repaint. That window is the tear.

Fix (supervisor-only, commits c569129 + 6c28e76): the ack message now carries the
surface each frame replaced; the dedicated ack-sender thread holds `shown <id>` until
`IOSurfaceIsInUse(replaced)` clears (500 µs poll, 50 ms cap; worker's 150 ms fallback
behind it). Kill switches: `LIMINA_ACK_ONGLASS=0` (run-scoped) and
`touch /tmp/limina-ack-latch` (live, 500 ms-cached stat). Engagement oracle: first
gated ack logs `off-glass ack gating ENGAGED` at INFO.

Rig validation (new bundle, KK true-0016 vendored + verified, same rig-21.raw seat,
release niri + async=1 + 4K@1.5 + 8 gnome-terminals, heavy ×2 per arm, marker as the
within-boot lever; marker efficacy PROVEN by `sample`: gate-on = ack thread ~100% in
the poll sleep, marker-on = parked in recv):

| arm | overall miss | cheap band | draws-200+ band | gpu p50 range |
|---|---|---|---|---|
| gate ON (off-glass) | 30.38% (3840/12639) | 0.11% | 86.71% | 6.10–17.11 ms |
| gate OFF (latch-only) | 32.07% (4083/12731) | 0.07% | 89.90% | 6.11–17.35 ms |

- **The gate costs nothing measurable**: flip counts, gpu ranges, and miss rates are
  arm-identical (Δ1.7 pts ≈ run noise). The +~17 ms ack delay does NOT reduce guest
  frame throughput — niri's pacing isn't serialized on our flip-completion at 60 Hz.
- **Both arms score in the honest sync band** (28.58–32.00%), i.e. no async flattery
  was present in this boot at all — and correspondingly the tear did NOT reproduce
  today even in the latch-only arm (user eyeball: gate-on clean, gate-off clean).
  Yesterday's tearing/flattered cells could not be re-provoked, so the eyeball A/B is
  one-sided; the conviction rests on the probe + the June copy-mode A/B (copy kills
  the tear ⇒ reuse-while-sampled is the mechanism, and the gate closes exactly that
  window). The async-score instability (13.1%/6.6%/28.7% across matrix cells) remains
  episodic — consistent with the tear/flattery firing only under WindowServer timing
  states that today's boot didn't enter.
- Logs on dev-mac /tmp (transient): `rig-24-arm{A-onglass,B-latchonly}.log`,
  `rig-21-onglass-supervisor.log`, `sample-gate{ON,OFF}.txt`.

Follow-ups: watch the dogfood after the next bundle deploy (dogfood-guest's niri miss
counters now include the true off-glass wait — expect no step, per the A/B); their
doc §27 correction still owed (their syncobj pairing was the wrong half — the lie was
our release pacing; the gate closes it).
