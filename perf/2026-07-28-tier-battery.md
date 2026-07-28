# Tier battery: virgl (stock, post-zero-copy) vs venus (enhanced) — 2026-07-28

First same-day, same-geometry A/B of the two accelerated tiers, taken the day vrend
IOSurface zero-copy shipped (virgl 0053 + libkrun 0115 + kk 0014, commit 12e7fd9).
Method per the pinning discipline: both guests pinned 1280x800 @ 1.0 via monitors.xml +
reboot and **verified read-only** before measuring; 4 vCPU / 8 GiB; sequential boots
(GPU never shared); debug worker with the dev-profile present-path opt (representative
present cost). virgl leg = stock F44 `accessible` clone (kernel 6.19.10, 4 KiB, stock
mesa 26.0.3); venus leg = F44 `enhanced` clone (7.1.4-limina16k, 16 KiB,
mesa 26.1.4-3.limina).

| workload | stock/virgl | enhanced/venus | note |
|---|---|---|---|
| glmark2-es2-wayland 512x512 `build` (score) | **5258 / 5325 / 5378** | 2157 | virgl **~2.4x** — see below |
| glmark2-build trace replay, headless (fps) | 47.1 | 48.2 | parity — replay-bound, not GPU-bound |
| llvmpipe control, same trace (fps) | 753 | 762 | env sanity: legs comparable ~1% |
| vkmark -s 1280x720 (score) | — no Vulkan | 2140 / 2132 | venus-exclusive capability |
| WebGL aquarium 1000 fish (fps) | **60** | **60** | both at vsync ceiling |
| WebGL aquarium 5000 fish (fps) | **60** | **60** | both at vsync ceiling |
| WebGL aquarium 10000 fish (fps) | **57** | 53 | virgl ahead even here |

## Reading it

- **The glmark2 gap is real and large.** 2.4x is far beyond glmark2's ±10% between-boot
  variance (both legs additionally ±1% within-boot). At 5000+ fps the workload is
  per-frame-overhead-bound, and the two tiers pay very different per-frame taxes:
  venus-GL is guest zink → venus protocol → vkr → KK (every Vulkan command serialized
  through the ring, fences as ring round trips — the venus wake-chain/relax-ladder work
  fought for +7-8% here), while vrend decodes GL host-side straight into zink → KK.
  For tiny frames the venus chain dominates; the stock tier simply has less machinery
  per frame.
- **This does NOT crown virgl the faster tier overall.** The trace-replay parity shows
  both accelerated paths equal where the bottleneck is elsewhere; heavier-frame
  workloads converge toward GPU-bound (unmeasured today — aquarium/heavy scenes are the
  follow-up). And the enhanced tier's claim was never glmark2: it is **guest Vulkan at
  all** (vkmark has nothing to run against on stock), 16 KiB pages, and the rest of the
  enhanced feature set.
- **The stock tier is no longer the "slow" tier for GL desktop work.** Post-zero-copy,
  its present mechanics match venus (IOSurface + CALayer, no CPU pixel work) and its GL
  microbenchmark throughput *exceeds* venus-GL substantially. The old
  "slower-than-software-2D" impression is fully dead (that was the debug present
  convert, fixed same day — see docs/hardening-backlog.md §GPU/rendering perf).
- **vkmark caveat:** today's 2140/2132 came from a freshly dnf-installed vkmark, below
  yesterday's 2484 (two-lane journal row) measured with the image-baked binary —
  version/scene-set confound possible; do not read a regression from this row without
  re-running the exact historical binary.

## Heavy-frame leg (WebGL aquarium, added same day)

Firefox WebGL aquarium at a 1024x1024 canvas, fps read from window-capture crops
(`perf/evidence/aquarium-2026-07-28/`), same pinned geometry, sequential boots. Both
tiers hold the 60 fps vsync ceiling through 5000 fish. At 10000 fish — a genuinely
heavy frame — **virgl still leads: 57 vs 53 fps (~+8%)**. The expected convergence
("heavier frames go GPU-bound, so the tiers should meet") did *not* fully happen: the
venus chain's per-command overhead scales with the command stream too, not just frame
count, and aquarium's thousands of per-fish draws/uniform-updates each pay the
zink→venus-serialize→ring→vkr decode toll while vrend decodes the same GL calls
host-side in one batch.

**Verdict: the GL story now favors vrend at every measured point** — 2.4x on
small-frame microbenchmarks, parity at vsync, ~+8% on the heaviest frame. This makes
"**vrend for GL, venus for Vulkan**" a live option for the *enhanced* tier: keep venus
for guest Vulkan apps (its unique capability) but route the GL desktop through
virgl/vrend instead of zink-on-venus. That would be a guest configuration change
(Mesa driver selection), not an engineering project — worth a dedicated dogfood trial
before deciding, since desktop *feel* (present pacing, input latency, compositor
behavior) is not fully captured by fps counters, and vrend still lacks fence-accurate
present (sync ends in glFinish).

## Follow-ups

- Trial "vrend for GL, venus for Vulkan" on the enhanced tier (needs: verify the
  enhanced guest's mesa still ships the virgl gallium driver; check compositor feel;
  fence-accurate vrend present would remove the glFinish pacing caveat).
- If venus-GL's small-frame overhead matters for real desktop feel, the vn ring work
  has a documented backlog (limina-present-miss, limina-venus-wake-chain).
- Re-baseline vkmark against the historical binary before the next ladder comparison.
