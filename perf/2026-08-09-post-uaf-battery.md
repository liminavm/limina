# Full performance battery after the discharge UAF fix (2026-08-09)

kk `38b801cbdd6` (discharge payload allocated before commit; `no_destroy` pin retired) on top of
`f2216e9dc29` (allocator retirement). Display pinned **1280x800 scale 1.0**, `--verify`'d on every
run. 4 vCPU, 4096 MiB, one `Fedora-Workstation-44.enhanced.raw` clone.

## Results

| instrument | value | vs pre-UAF-fix (same policy) | vs 2026-08-08 |
|---|---|---|---|
| `gl-replay-venus` (fps, n=3) | **46.55** | 46.80 → −0.5% | 47.6 → −2.2% |
| `gl-replay-llvmpipe` (fps, CPU control, n=3) | **752.7** | 752.4 → +0.0% | 746.0 → +0.9% |
| `vk-replay-venus-headless` (fps, n=3) | **1959.1** | 1991.2 → −1.6% | 1974.7 → −0.8% |
| `glmark2-wayland-venus` (score, n=3) | **2947** | 2924 → +0.8% | 2944 → **+0.1%** |
| `vkmark` (score, 1280x720, n=3) | **3073** | — | 3151 → −2.5% |
| `glmark2` vrend tier (800x600, n=2) | **3374 / 3378** | — | — |
| aquarium vrend 20k fish | **60** ⚠ vsync ceiling | — | 60 |
| aquarium vrend 25k fish | **47** | 48 | 42 → +12% |
| aquarium vrend 30k fish | **43** | — | 39 → +10% |

**The UAF fix is throughput-neutral**, which is what a reordering of an allocation ought to be.
Every delta sits inside the ±10% run-to-run variance `perf/README.md` records for these workloads,
and the CPU control moved as much as the graphics rows.

⚠ Aquarium at 20 000 fish reads **60 = the vsync ceiling**, not throughput. Per `perf/README.md`,
never read a 60 as a rate. 25k/30k are the load-bearing rows.

## ⚠ Two arms that were NOT two arms — discarded

I ran `glmark2` twice intending a venus-vs-vrend split, with
`MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu GALLIUM_DRIVER=virgl` as the differential, and got 3374 vs
3378. Near-identical A/B results mean the differential is not reaching the system under test, so I
checked the renderer instead of publishing the pair:

```
default arm:  GL_RENDERER: virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))
"vrend" arm:  GL_RENDERER: virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))
```

Identical. The default arm was **already** the vrend tier, so the override was a no-op and both
runs measured one tier. Reported above as a single vrend number (n=2), not a tier comparison. The
genuine venus GL number is the ledger's `glmark2-wayland-venus`, which selects the tier properly
through the session's `environment.d` rather than through ad-hoc env on a non-login shell.

## The 08-08 → 08-09 "composited-path regression" does not reproduce

`perf/2026-08-09-allocator-pool.md` recorded a ~5–7% drop on every composited path, measured on a
*pre-pool* build, and pointed at present/scanout. On today's clean rerun:

| | 08-08 | 08-09 morning (pre-pool) | now |
|---|---|---|---|
| `glmark2-wayland-venus` | 2944 | 2812 (−4.5%) | **2947 (+0.1%)** |
| `gl-replay-venus` | 47.6 | 44.4 (−6.7%) | **46.55 (−2.2%)** |
| `vkmark` | 3151 | 2969 (−5.8%) | **3073 (−2.5%)** |

`glmark2-wayland-venus` is back at parity and the other two are within ~2.5%. **The regression is
most likely not in the code.** The morning runs and these differ in at least two uncontrolled
ways — a fresh disk clone, and a guest rebooted mid-session for display pinning — so this does not
identify a cause, and it does not fully exonerate the code either. What it does do is remove the
motivation for a bisect: there is no stable signal left to bisect against.

Recommended: **close the bisect lead** and reopen only if a composited-path drop reproduces on a
controlled A/B. Chasing a 5% delta that vanishes on reboot costs more than it returns.

## Not covered

- `glmark2` on the venus tier at 800x600 specifically (the ledger's row is the wayland variant at
  its own size), so the two `glmark2` figures above are not directly comparable to each other.
- Aquarium was run on the vrend tier only this round; no venus arm.
- The floor/decay defaults (8 per class, 2000 ms) remain unswept — see
  `perf/2026-08-09-allocator-destroy.md`.
