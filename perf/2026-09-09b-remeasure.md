# Remeasure at the virglrs Linux-port pin

**Subject:** virglrs `d30b8ce` → `4bfdefe` on limina `55f392d1`, libkrun `0dba6b9c`.
Ledger rows carry commit `55f392d1`; evidence in `perf/evidence/2026-09-09b/`.

This is a **regression gate, not attribution**. The four ledger workloads catch large
regressions and cannot attribute small ones ([[limina-perf-instruments]]), which is the right
instrument for "did ~40 commits of virglrs regress macOS" and the wrong one for anything finer.

## What the gate covers

Two stacked deltas, neither separable here:

1. `d30b8ce → feef548` — limina `2d6dbc92`, **never perf-measured**.
2. `feef548 → 4bfdefe` — the pin gap to virglrs `origin/main` plus the Linux port.

If a later pass finds a regression in this window, the first bisection arm is `feef548`.

**The measured pin is HVF-ungraded.** The 138/138 suite ran at `bc1937a`; the pin is four
commits and +263/−10 past it, including a new cursor-readback path in `vrend.rs` and
`transfer.rs`. Those commits are Linux-port work and are not expected to reach macOS, but no
HVF run stands behind `4bfdefe` itself.

## Result: no regression on any guest-idle instrument

Every guest-idle band overlaps its `d30b8ce` counterpart. n=5 unless noted; **quiet host**, both
virglrs sessions holding VMs and GPU replays; display **verified** 1280x800 @ 1.0; guest
`7.1.8-limina16k.4` / mesa `26.1.8-11`; debug worker with the dev `opt-level = 3` overrides —
the same vehicle every prior ledger row used.

| workload | this pin | `d30b8ce` (09-09) | verdict |
|---|---|---|---|
| `gl-replay-venus` | 56.88 – 57.27 | 56.86 – 57.43 | overlap |
| `gl-replay-llvmpipe` (control) | 720.4 – 735.5 | 717.4 – 733.6 | overlap |
| `vk-replay-venus-headless` | 2110.8 – 2267.7 | 1906.7 – 2193.6 | overlap |
| `glmark2-wayland-venus` | 2901 – 3118 | 3023 – 3065 | overlap |
| `vkmark-default-venus` | 4212 – 4366 (2 boots) | 4146 – 4215 | overlap, barely |
| `aquarium-25000-vrend` | 44 / 45 / 50 | 43 / 45 / 51 | overlap |
| `aquarium-30000-vrend` | 37 / 39 / 39 | 38 / 39 / 43 | overlap |

**The llvmpipe control gates readability, and a quiet host does not.** The 09-09 pass discarded
a run whose control read 630.6 against a 717–734 band with **no VM up** — replays alone did it.
All five runs here sit in band, so all five are readable.

## The one number that moved: the contended arm

| | this pin | `d30b8ce` (09-09) |
|---|---|---|
| `vkmark-under-aquarium25k-venus` | 1247 / 1261 (boot 1), 1231 / 1208 (boot 2) | 1450 / 1439 |

**The bands do not overlap, it is ~14% lower, and it reproduces across two boots.** It is the
only adverse result in the pass and it is not dismissible as noise.

**What it is not: settled.** The baseline is a **single-boot pair** taken when this workload was
new, so it has exactly the weakness this pass corrected for on its own side. A two-boot
measurement against a one-boot baseline can separate because the baseline never sampled its own
between-boot spread. **What would settle it is a re-measure of this workload at `d30b8ce`**, not
more samples at this pin.

The uncontended instruments are flat, so whatever this is does not show without a competing GPU
client.

## Method notes that changed an answer

**A within-boot triple measures the wrong variance component**, and here it mattered rather than
being a formality. `vkmark` on boot 1 alone read 4242 / 4313 / 4348 and cleared the 09-09 band
outright — an apparent improvement. Boot 2 read 4212 / 4366 / 4258 and collapsed the separation
to a 3-point overlap. **Report vkmark across boots or do not report it.**

**A debug worker is the correct vehicle**, and switching to release would break comparability
with every prior row. `[profile.dev.package.*]` in the root `Cargo.toml` puts virglrs,
`rustc-hash`, `bumpalo` and `bytemuck` at `opt-level = 3`, so a debug worker runs an optimized
renderer; `boot-enhanced-efi-kk.sh` passes `--vmm-bin target/debug/limina-vmm` literally.
Confirmed empirically: the CPU-bound llvmpipe control agrees across pins, which two different
worker profiles could not produce.

`limina-vmm`'s own code is at `-O0` on both sides, so it cancels for the differential — but it
sits in the path as a constant. **That is a reason to distrust a null here, not the vehicle:** a
small renderer regression can hide behind a large constant.

## Not measured

**The port's one known macOS behaviour change is invisible to this battery by construction.**
`switch_ctx0()` now rebinds after a sub-context is destroyed instead of short-circuiting on a
stale shadow, so real KK/zink teardown happens at a different moment. Nothing in this set
destroys sub-contexts mid-measurement. Seeing it needs a context-churn workload, which does not
exist. **Not measured — never read as no impact.** The HVF suite scored that change's
*behaviour* (green at `bc1937a`, including the compositor-restore test at 104.5 s); nothing has
scored its *cost*.

Still owed from the concluded arc: the multisample cap with a positive control; the
`IOAccelerator (graphics)` ratchet; wakeups; boot; disk; memory floor; the zinkvenus aquarium
arm; a `krun_rutabaga_gfx` opt-level A/B with a positive control.
