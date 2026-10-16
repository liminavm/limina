# Why venus is slower than in September

**Subject:** the venus arm (Vulkan on the venus ICD, GL through zink on venus) reads well below its
September rows: vkmark ~4100 then, ~2100 on 2026-10-15. This pass splits the drop by cause.
- **Host:** macOS 27.0.1, M1 Max, quiet (no other VM; a gate held each point until no process but
  WindowServer used over 30% CPU for two samples a minute apart).
- **Guest:** `Fedora-Workstation-44.metal.test.raw`, 4 vCPUs, 4 GiB, 1280x800 @ 1.0, 60 Hz.
- **Bundles**, booted straight from the parked `.dmg`s (`LIMINA_BIN`, `LIMINA_VMM_BIN`,
  `LIMINA_FIRMWARE`, `LIMINA_KK_ICD`, `MESA_PREFIX`):
  - **September:** `2026-09-10-8` — limina `6b0af433`, virglrs `d0416c9`, limina-kk `351a1b5e`.
  - **Today:** `2026-10-15-1` — limina `817dfc7a`, virglrs `2efa024`, limina-kk `2e1d2485`.

Vehicles, rows and evidence are in `perf/venus-regression-2026-10-15/`; nothing went to
`perf/ledger.csv`.

## Results

| workload | September bundle | today's bundle | change | cause |
|---|---|---|---|---|
| vkmark | 3511 | 2198 | −37% | **ring-fence ordering, all of it** |
| glmark2-wayland (zink) | 2877 | 2334 | −19% | ring-fence ordering ≈ half; the rest unattributed |
| gl-replay (zink, headless) | 56.6 | 50.0 | −12% | the vCPU band's `arm_cap()` |
| vk-replay headless, warm cache | 2080 | 1819 | −12.5% | **open** |

Bundle A/B, alternated a b a b a (`legs.sh`, one `point.sh` per point: perf-ledger once, vkmark
three times). vkmark per point: a0 3540/3587/3492, a1 3392/3408/3477, a2 3606/3501/3602; b0 and b1
2198 median. glmark2 2888/2866 against 2336/2331; gl-replay 56.7/56.6 against 49.9/50.1.

- **Ring-fence ordering.** Since virglrs ordered venus ring fences (the stale-frame fix,
  `spikes/stale-frame-repro/FINDING-venus-ring-fence.md`), each fence is an empty `vkQueueSubmit` on
  the ring's queue plus a `VkFence` a per-ring thread waits on. There is one per presented frame
  (the WSI sync_file), and it costs ~215 µs from arrival to retire: submit 2 µs, queued 7–8 µs, the
  GPU completing the signal submit ~205–215 µs. A blocking and a polling wait cost the same.
  `VIRGLRS_RING_FENCE_UNORDERED=1` (in `virglrs-ring-fence-probe.patch`, debug worker) retires
  fences on arrival, as before the fix:

  | today's stack, debug worker | vkmark | glmark2 | gl-replay |
  |---|---|---|---|
  | ordered (shipped) | 2131–2181 | 2255 / 2264 | 49.7 / 49.9 |
  | unordered | 3298 / 3394 / 3374 | 2531 / 2522 | 50.0 / 49.9 |

  Unordered retirement brings back the stale frames, so it is a measurement seam, not a fix.
- **The band cap.** `arm_cap()` lets one vCPU at a time into the real-time band (half the
  efficiency cores, so 1 on the M1 Max), the host-panic fix. `perf/2026-09-21-remeasure.md`
  measured static `LIMINA_VCPU_SCHED=rt` restoring gl-replay to 56.2–56.9. Not re-run here: static
  `rt` is the configuration that panicked the shared host. `rt+dyn` (every vCPU registered, the cap
  still arming one) changes nothing: vkmark 2195/2204/2200, glmark2 2354.
- **vk-replay** is band-insensitive, headless and unexplained. The window to bisect is limina
  `6b0af433..e21205a7` (virglrs `5582aa3` → `901c0d0`).

## Not the cause

- **KosmicKrisp.** September's KK and zink-on-KK under today's worker: gl-replay 49.9, glmark2 2281,
  vkmark 2135 — today's numbers.
- **KK advertising geometry shaders** (guest zink then reports GLES 3.2).
  `LIMINA_KK_GEOMETRY_SHADER=0` (GLES 3.1): gl-replay 49.8, glmark2 2262, vkmark 2179.
- **#15** (`glFinish` slow on zink): its loop does not go through ring fences. `vtxbench.c`'s empty
  frame reads 0.36–0.60 ms ordered and unordered alike.

## Reading these numbers

- **The September bundle reads 3511 on macOS 27, against ~4100 in September on macOS 26.6.** The OS
  moved as well; compare bundles only within one day's run.
- **vk-replay reads the shader cache cold on a bundle's first point.** `point.sh` keeps one
  `MESA_SHADER_CACHE_DIR` per bundle: the first points read 649 (b0) and 879 (a1), the warm ones
  1819 and 2080. Only warm points are compared.
- **`#N` in `LIMINA_VCPU_SCHED` limits which vCPUs register, not how many are armed.** Only static
  `rt` gets past `arm_cap()`.
- **#17** is untested: the only lever that would separate it is the same static `rt` run.

## What would recover it

- **vkmark and half of glmark2: GPU-side ordering.** Retire the ring fence on arrival and make the
  consumer of the frame (the present path) wait on the GPU, e.g. on an `MTLSharedEvent` signalled by
  the guest's submit, instead of the guest waiting a host round trip. A guest that reads mapped
  memory on the CPU right after a fence still needs today's CPU wait, so that case has to be
  distinguished before the wait is dropped anywhere.
- **gl-replay:** only by revisiting the band cap, a safety trade made deliberately.
- **vk-replay:** bisect first.
