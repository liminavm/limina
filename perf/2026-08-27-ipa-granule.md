# A 4 KiB stage-2 granule costs ~4–8% on CPU-bound work, nothing when GPU-bound (2026-08-27)

`--ipa-granule 4k` (`hv_vm_config_set_ipa_granule`, macOS 26+) is what lets a 4 KiB-page guest map
its virtio-gpu host-visible blobs at all — without it a stock Fedora guest has no Vulkan
(`spikes/hv-ipa-granule/RESULTS.md`). It is a **whole-VM** setting, so the question this answers is
what it costs a guest that does *not* need it.

**Method.** One CoW clone of `Fedora-Workstation-44.enhanced.raw` (16 KiB guest — the tier that
gains nothing from the fine granule), both arms in sequence, VM rebooted between arms so the worker
starts fresh. 4 vCPU / 4096 MiB, display pinned 1280x800 @ 1.0 and `--verify`'d on each
`perf-ledger.sh` run. Medians of n=3 per arm; `perf/README.md` puts variance on these workloads
around ±10%, so the ranges are given — three of the four do not overlap at all.

## Throughput

| workload | 16 KiB (default) | 4 KiB | Δ |
|---|---|---|---|
| `gl-replay-venus` fps | **49.25** [48.77–50.63] | **45.28** [45.06–45.57] | −8.1% |
| `gl-replay-llvmpipe` fps *(CPU control)* | **743.1** [733.8–746.2] | **713.9** [711.5–716.2] | −3.9% |
| `vk-replay-venus-headless` fps | **1898.9** [1892.3–1968.2] | **1933.2** [1897.2–1963.3] | +1.8% |
| `glmark2-wayland-venus` score *(vrend)* | **2829** [2826–2861] | **2668** [2649–2702] | −5.7% |

| aquarium (fps, on-display) | 16 KiB | 4 KiB |
|---|---|---|
| 20 000 fish | 60 *(vsync ceiling — ≥60, unquantified)* | 54 |
| 25 000 fish | 48 | 44 |
| 30 000 fish | 42 | 42 |

## What the control says

**`gl-replay-llvmpipe` moved.** That workload never touches the GPU stack — `perf/README.md` keeps
it precisely so that a move there means the cause is not venus. A ~4% loss on pure guest CPU work
is the signature of the finer granule itself: more stage-2 entries and smaller block descriptors,
therefore more TLB pressure on everything the guest executes.

So the cost is **a general guest-execution tax, not a graphics one**, and the GL numbers are that
same tax landing on paths with CPU work in them. Where the workload is genuinely GPU-bound it
vanishes: `vk-replay-venus-headless` overlaps completely between arms, and aquarium at 30 000 fish
is identical. The 20 000-fish row cannot be quantified — the 16 KiB arm sits on the vsync ceiling,
which hides arbitrary headroom (never rank on a 60).

## Memory: no penalty

Worker state at the end of the identical sweep:

| | 16 KiB | 4 KiB |
|---|---|---|
| physical footprint | 7.1 G (peak 8.5 G) | 6.2 G (peak 7.8 G) |
| `IOAccelerator (graphics)` regions | 3 188 | 3 214 |

The footprint difference runs the *wrong* way for a cost and is well inside what a browser workload
varies by between runs; read this row as "no measurable penalty", not as a saving.

## The decision these numbers bought

**4 KiB is the default** (`[hardware] ipa_granule` in `vm.toml`, `--ipa-granule`), and **16 KiB is
the informed opt-in**, offered as *Memory pages* in the control center's Configure sheet.

The granule is fixed at VM creation, so a guest's page size cannot be discovered in time to choose
it on the first boot of an unknown image — the default has to be the one that is merely *slower*
when wrong, never the one that is *broken* when wrong. 4-8% is what a guest pays for being
understood without being configured; a 16 KiB guest whose owner says so gets it back. For scale:
Asahi's 4 KiB kernels were measured around 15% against their 16 KiB ones, so this seam is cheap by
comparison.

Existing definitions carry no key and therefore move to 4 KiB with this change — a knowing
regression for the enhanced tier, taken because the compatibility floor is what a default is for.
Auto-selecting 16 KiB from an observed guest page size is a later refinement, not a prerequisite.
