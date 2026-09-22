# Remeasure after the vCPU-band host-panic fix

**Subject:** libkrun `87d3c985` + virglrs `901c0d0` on limina `e21205a7`.
Last clean baseline: **2026-09-10 `6b0af433`**. Ledger rows carry `e21205a7`.

This pass exists because the band changed underneath us. The 2026-09-21 host panic was fixed
by capping how many vCPUs may hold the real-time band at once ([[limina-vcpu-band-host-panic]]),
and `arm_cap()` evaluates to **1** on this M1 Max — `(2 efficiency cores / 2).max(1)`. The
baselines were taken when every idle vCPU could hold it. That is a change of band *reach*, not
of default, and it is not what the `#N` suffix does.

## Two regressions, and only one of them is the band

| workload | 09-10 baseline | shipped default | `rt` (all 4 banded) | verdict |
|---|---|---|---|---|
| `gl-replay-venus` | 56.67 – 56.80 | 44.8 – 49.4 | **56.2 – 56.9** | band reach, **restored** |
| `vk-replay-venus-headless` | 2110.8 – 2267.7 | 1780.8 – 1886.4 | 1586.8 – 1647.3 | **not band** (worse with it) |
| `glmark2-wayland-venus` | 2901 – 3118 | 2437 – 2456 | 2374 – 2442 | **not band** (unmoved) |
| `gl-replay-llvmpipe` (control) | 717 – 734 | 729.8 – 742.3 | 746.9 – 757.0 | see drift, below |

**`gl-replay-venus` is entirely band reach.** The static band lands on the baseline, >6 points
clear of every dynamic reading in the pass. Nothing else in this sweep separates that widely.

**The other two are a second, independent regression** of 16–18%, in the window
`6b0af433 → e21205a7`. They are flat across `rt+dyn#1` and `rt+dyn`, and the static band makes
`vk-replay` *worse*, so no band configuration reaches them. Attribution needs a bisect of that
window; nothing here narrows it.

**The two band-sensitive instruments disagree in sign.** Banding all four vCPUs restores the
latency-bound Xwayland replay and costs ~12% on headless Vulkan throughput. "More band" is not
uniformly better, so raising `arm_cap` is a trade to measure per workload, not a win to claim.

## What this pass cannot say

**Whether the `#1` default costs anything on top of the cap.** It looked like ~10% early —
45.3 against 50.3, non-overlapping — and the closing bracket dissolved it: re-measuring the
default *after* the legs gave 45.7 / 46.8 / **49.4**, whose top nearly meets `rt+dyn`'s 49.9.
The bands touch. Unresolved, and it needs its own alternated A/B rather than more samples.

**Anything small.** The `gl-replay-llvmpipe` control drifted **730 → 757 → 738** across ~35
minutes as the host quieted — ending *above* its historical band. A sweep whose host speeds up
under it resolves large effects only; that is this battery's standing resolution floor
([[limina-perf-instruments]]), not a fault of this run.

## Method notes

**Measure the baseline at both ends of a long sweep** — the rule from the 09-16 pass, and it
overturned a conclusion here rather than confirming one. A single opening leg would have
shipped the `#1` claim.

**The suffix is not the cap, and a test that varies neither says nothing about either.** The
first A/B ran `rt+dyn` against `rt+dyn#1` intending to test the cap. Both legs logged `at most
1 armed at once`: `#N` limits which vCPUs *register*, `arm_cap()` limits how many are *armed*.
Only `LIMINA_VCPU_SCHED=rt` — the static path, no sampler, no cap, no guard — reaches the
pre-cap reach, and it needs no code change. Read the policy back out of the worker log before
believing a leg tested what it was meant to.

**`vk-replay` runs `--wsi headless`.** It presents nothing, which is what exonerated the
present-fence work early: that change could not explain a drop on an instrument with no present
in it.

## Vehicle

Debug worker with the dev `opt-level = 3` overrides; EFI+venus (`boot-enhanced-efi-kk.sh`);
4 vCPU / 4 GiB; display **verified** 1280x800 @ 1.0; guest `7.1.8-limina16k.4` / mesa
`26.1.8-11`; quiet host, no other VM. Legs: `rt+dyn#1` n=5, `rt+dyn` n=3, `rt` n=3,
`rt+dyn#1` n=3 closing. One boot per leg, policy confirmed from the worker log each time.

The band itself was verified **held** for the first time in this pass: the build measured here
takes it once and keeps it at idle, where the build that shipped a few hours earlier held it
3.3% of the time (464 arm/disarm pairs in 103 s). Rows taken between those two builds would
have measured a band that was not there.
