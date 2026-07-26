# Performance re-measurement — 2026-07-26

First full graphics re-measurement since **2026-06-25**. Host: M1 Max, 32 GB, macOS 26.5. Worker at
commit `183f1cb`, linked against `third_party/virgl-prefix` (verified with `otool -L`).

Raw rows in `perf/ledger.csv`; aquarium frames in `perf/evidence/aquarium-2026-07-26/`.

## TL;DR

- **One real regression: `gl-replay-venus` is down ~18%** (57.6 → 47.3 fps) and it is **host-side** —
  F43 (mesa 26.2.0-3) and F44 (mesa 26.1.4-3) return the *same* 47.3, so the guest driver is not the
  variable. `vk-replay-venus-headless` is down ~9% on F43. Prime suspects are the host changes since
  2026-06-25: KosmicKrisp `0012`/`0013` (timestamp queries now resolve on the CPU with explicit
  ordering — new sync points on every command buffer), virglrenderer `0041`, libkrun `0091`.
- **Everything that actually looks like a desktop got better.** WebGL aquarium improved on both GPU
  tiers: venus 60/57/45 → **71/61/46**, virgl 37/28/22 → **45/33/29**. The CPU floor is flat.
- **A methodology bug invalidated part of the old comparison, and is now fixed in the tooling** —
  the guest display geometry was uncontrolled (see below).
- **The old "virgl is 8× slower than llvmpipe" conclusion is retired** — that number was vsync, not
  throughput.
- **F43's enhanced image is unstable** on glmark2 (~20% spread run-to-run); **F44 is not** (<1%).
  Prefer F44 as the baseline going forward.

## Two measurement bugs found (both would have produced confident, wrong numbers)

### 1. Uncontrolled display geometry

`match-host` display mode shipped **2026-07-03**, after the last perf run. It drives the guest to
the host screen and GNOME picks a fractional scale — 2560x1440 @ 2.5 here, delivered to wayland
clients as **`buffer_scale 3`**. Consequences:

- glmark2's default 512x512 is not a multiple of 3 → the compositor **rejects the buffer**
  (`Buffer size (512x512) must be an integer multiple of the buffer_scale (3)`) and the score is
  garbage. First two runs produced **97** and **274** for the same build.
- Even when valid, a WxH request yields a 3W×3H buffer — ~**9× the pixels**, so nothing is
  comparable to a pre-2026-07-03 number.

A *stock* guest is not exempt: it came up at scale **1.3333** on its own.

**Fixed:** `scripts/perf/set-guest-display.py` pins mode+scale; `scripts/perf-ledger.sh` now
`--verify`s and **aborts** rather than measuring an unpinned display. All numbers below are at
**1280x800 @ scale 1.0**, 4 vCPU / 4 GiB — the geometry and envelope the 2026-06-25 rows used.

### 2. The "Keep changes?" dialog

Applying the mode over D-Bus pops GNOME's confirmation dialog, which **reverts after ~20 s if nobody
clicks**. It blocked one run outright and silently un-pinned the display during another. The
supported path is now `--write-config` (writes `monitors.xml`) + reboot, which applies at compositor
startup with **no dialog**, plus a read-only `--verify` at the top of each run.

## Enhanced tier (venus) — F43, apples-to-apples with 2026-06-25

| Workload | 2026-06-25 | 2026-07-26 | Δ |
|---|---|---|---|
| `gl-replay-venus` (fps) | 57.63 | 47.0–49.8 (med **47.2**) | **−18%** |
| `vk-replay-venus-headless` (fps) | 1601 | 1391–1473 (med **1450**) | **−9%** |
| `glmark2-wayland-venus` (512², score) | 1306 | 1048–1495 (med ~**1200**) | −8%, ±20% noise |
| `gl-replay-llvmpipe` (fps, CPU control) | 537 | 841–866 | **+60%** |

The CPU control moving +60% means the two environments differ in more than the venus stack, so the
venus deltas are not perfectly clean. The vehicle also changed: 2026-06-25 used `boot-seated-kk.sh`
(injected kernel, `selinux=0`) versus this run's EFI boot with SELinux **enforcing**. (Host agent CPU
load is *not* a confound — it was present for the historical runs too.)

What survives the confounds — and it survives on **both** venus workloads:

| | F43 (mesa 26.2.0-3) | F44 (mesa 26.1.4-3) | 2026-06-25 |
|---|---|---|---|
| `gl-replay-venus` | 47.2 | 47.3 | 57.6 |
| `vk-replay-venus-headless` | ~1450 | ~1399 | 1601 |

**Two different guest mesa builds land on the same reduced number on both the GL and the Vulkan
path.**

⚠ **Correction to an earlier over-claim in this document:** that does *not* prove a host-side cause.
The F43 `-3` and F44 `-3` mesa respins share **our** patch set (0014 ring-loss `DEVICE_LOST`
hardening, 0016, 0017 submit free-list), so a guest-side cause living in those common patches would
also hit both bases identically. Guest mesa is **not** excluded.

## KosmicKrisp A/B — KK is exonerated (2026-07-26)

Built KK at **`af708c37f69`** — the exact tip from 2026-06-25, before the ten commits that landed
since (custom_border_color, depth_clip_enable, TF clamp, timestamp queries `0008`, and the `0010`–
`0013` timestamp fixes) — and ran it against the same F44 guest, same envelope, same pinned display.

| Host KK | `gl-replay-venus` (fps) | `glmark2-512` |
|---|---|---|
| **old (`af708c37f69`, = 2026-06-25)** | 47.63 / 47.66 / 47.57 | 2120 / 2157 / 2154 |
| **current (`ebc07301baf`, 0013 tip)** | 47.29 / 47.31 / 47.43 | 2007 / 2056 / 2027 |
| 2026-06-25 reference | **57.6** | 1306 |

**The 2026-06-25 KosmicKrisp does not restore 57.6 fps — it gives 47.6.** So the timestamp-query
work (`0008`, `0010`–`0013`) is **not** the cause, and KK is cleared for this regression. (KK is
worth ~5% on glmark2, in the *old* build's favour, but that is a different, much smaller effect —
and note old KK cannot meet zink's base requirements, warning
`doesn't support base Zink requirements: have_EXT_custom_border_color`, so it is not a config we
could ship anyway.)

Two more candidates killed in the same session, both cheap:

| Hypothesis | Test | Result |
|---|---|---|
| `VN_PERF=no_fence_feedback` (forced in the 2026-06-25 runner, dropped as-shipped 2026-07-25) | set it back on the current stack | 48.0 vs 47.4 — **+1.3%, not it** |
| SELinux enforcing (the historical vehicle ran `selinux=0`) | `setenforce 0`, re-measure | 46.7–47.2 — **no gain, not it** |

**Investigation paused here by request.** Leading remaining hypothesis, from the user:
**the venus ring relax / park behaviour** (the idle/wake path — see `patches/virglrenderer` `0041`
and the wake-chain work, memories `limina-overhead-trim` / `limina-venus-wake-chain`). Other live
candidates: the rest of virglrenderer/libkrun since 2026-06-25, our guest mesa patches 0014/0016/0017,
and the possibility that the 57.6 reference itself is not reproducible on the current vehicle.

**To recreate the old-KK build** (worktree removed after the A/B to save space on the 16 GB
`mesa-cs` volume):
```bash
git -C /Volumes/mesa-cs/mesa worktree add /Volumes/mesa-cs/mesa-old af708c37f69 --detach
cd /Volumes/mesa-cs/mesa-old
PATH="/opt/homebrew/opt/llvm/bin:$PATH" meson setup build-kk-old --buildtype=debugoptimized \
  -Dvulkan-drivers=kosmickrisp -Dgallium-drivers= -Dplatforms=macos -Dopengl=false -Dprefix=/opt/homebrew
PATH="/opt/homebrew/opt/llvm/bin:$PATH" ninja -C build-kk-old
# then boot with LIMINA_KK_ICD=/Volumes/mesa-cs/mesa-old/build-kk-old/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
```
`-Dopengl=false` is load-bearing (otherwise meson demands an LLVM subproject), and `llvm-config`
must be on `PATH` — Homebrew keeps it at `/opt/homebrew/opt/llvm/bin`, off the default path.

## F44 enhanced — new baseline (no historical rows)

| Workload | Value |
|---|---|
| `gl-replay-venus` | 47.29 fps (repeats 47.31 / 47.43) |
| `gl-replay-llvmpipe` | 753 fps |
| `glmark2-wayland-venus` (512²) | 2007 / 2056 / 2027 |
| `glmark2-display-venus` (on-display 800×600) | 2013 / 2019 / 2030 |
| `vk-replay-venus-headless` | 1390 / 1399 / 1447 (med **1399**) |

`gfxrecon-replay` is not packaged and had to be built in-guest. On F44 the configure step fails
until `libX11-devel libxcb-devel wayland-devel jsoncpp-devel mesa-libGL-devel libXrandr-devel` are
installed — without them OpenXR's presentation backend aborts cmake.

**F44 is both faster and dramatically more stable than F43** on glmark2 — <1% spread versus F43's
~20%. F43's instability is itself worth chasing; it is not present on the newer image.

## Three-tier on-display glmark2 (800×600)

| Tier | 2026-06-25 | 2026-07-26 |
|---|---|---|
| venus (F44) | — | **2019** |
| venus (F43) | 2784 | **1790** (warm median of 3; cold first run 1487) |
| software-2D | 454 | **342** |
| virgl | 56 | **57** ⚠ vsync-limited |

### ⚠ The virgl number does not mean what the old note said

`docs/tiers.md` concluded virgl "underperforms the CPU floor (~8× slower than llvmpipe)" and blamed
its copy/transfer model. **That is unfounded.** A **64×64** build scene — ~150× less fill than
800×600 — returns *exactly the same 58 fps*. A score that does not move when you shrink the workload
150× is measuring the frame clock, not the renderer. virgl is pinned at display refresh here, while
venus (2019) and llvmpipe (342) are not, so the three are not commensurable. `vblank_mode=0` does
not lift it (GLX/DRI knob, ignored by the wayland winsys).

**Open question worth its own look:** why is the stock GL tier capped at ~58–60 fps regardless of
load, when the other tiers on the same compositor are not? That caps stock-tier GL at 60 fps no
matter the hardware.

## WebGL aquarium — the workload that does measure throughput

| numFish | software-2D | virgl | venus |
|---|---|---|---|
| 5 000 | 17 → **16** | 37 → **45** (+22%) | 60 → **71** (+18%) |
| 10 000 | — | 28 → **33** (+18%) | 57 → **61** (+7%) |
| 15 000 | — | 22 → **29** (+32%) | 45 → **46** (+2%) |

Both GPU tiers improved; the CPU floor is flat. venus at 5 000 now reads **71 fps, above the ~62 Hz
mode**, so it is no longer on the refresh ceiling the 2026-06-25 run called "vsync-capped".

**Now fully automated.** The aquarium prints fps only on screen; historically a human had to look.
`scripts/perf/aquarium-run.sh` drives Firefox per fish count, harvests the supervisor's own frame
dump (`LIMINA_WINDOW_CAPTURE`), and crops the counter via `scripts/perf/crop-fps.py`. It retries on
both real failure modes — a half-written PNG and a frame captured before the page painted — since
either otherwise reads as a legitimate data point.

## Suggested follow-ups

1. **`gl-replay-venus` −18%: accepted for now, investigation paused.** KK, `VN_PERF` and SELinux are
   ruled out (above). Next probe when picked back up: **the venus ring relax / park path**, then the
   rest of virglrenderer/libkrun, then guest mesa 0014/0016/0017. Worth also re-testing whether the
   57.6 reference reproduces at all on the current vehicle before chasing it further.
2. **Chase F43's ~20% run-to-run variance** on glmark2, absent on F44.
3. **Find virgl's ~58 fps ceiling** — compositor frame callbacks or vrend's present path. This one
   is arguably the biggest user-visible item here: it caps *all* stock-tier GL at 60 fps.
