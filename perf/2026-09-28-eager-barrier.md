# A/B: host zink's eager end-of-pass barrier

**Subject:** limina-kk `d84192b397e`. When a render pass ends, zink issues the attachment-write →
shader-read barrier for each attachment it wrote, so a later texture bind does not split the next
pass (`spikes/wildbrush-stall/RESULTS.md`). Both arms ran one build: host mesa `07d6cad3274`,
virglrs `26e91e1`, libkrun `b68f3686`, limina `7deda6ac`. The off arm set
`LIMINA_ZINK_NO_EAGER_RP_BARRIER=1`, and both arms set `LIMINA_ZINK_RP_STATS=1`. The rows, the
evidence and the driver are in `perf/eager-barrier-2026-09-28/`. Nothing went to
`perf/ledger.csv`.

## What this pass can and cannot see

The barrier changes the host zink path, so only the vrend rows can move: the aquarium on the
enhanced guest and Basemark on the stock guest. The venus rows (gl-replay-venus, glmark2,
vk-replay, vkmark) are the control, and `gl-replay-llvmpipe` shows whether the host was quiet.

**The arms really differed.** `point.sh` counts, from each worker log, the passes resumed after a
texture bind (`arm-check.txt`):

| guest | b0 off | n0 on | b1 off | n1 on | b2 off |
|---|---|---|---|---|---|
| enhanced | 5685 | 2 | 4954 | 2 | 6134 |
| stock | 10411 | 133 | 9811 | 214 | 8071 |

**The enhanced guest has little for the barrier to remove.** Its vrend passes end at a flush:
about 1.04 passes per submit, and texture-bind splits are about 0.6% of its 850k-970k passes per
point. The aquarium rows therefore test only that the barrier costs nothing there.

**Display: 1280x800 at 59.97 Hz, scale 1.0, pinned in every guest** by `point.sh`.

**The host was quiet.** `gl-replay-llvmpipe` reads 697-739, and the one low reading (b1 run 2,
697) coincides with dips in glmark2 and vk-replay in the same boot.

## Results

The points ran in the order b0 n0 b1 n1 b2, one boot each, measured 2026-09-28.

| workload | b0 off | n0 on | b1 off | n1 on | b2 off |
|---|---|---|---|---|---|
| gl-replay-llvmpipe (control) | 735 738 739 | 729 710 734 | 737 697 726 | 729 736 739 | 727 733 737 |
| gl-replay-venus | 46.2 46.1 46.4 | 46.3 46.2 46.2 | 46.2 47.3 47.2 | 46.6 45.9 46.1 | 46.4 46.6 46.7 |
| glmark2-wayland-venus | 2725 2712 2718 | 2734 2723 2727 | 2724 2639 2730 | 2727 2722 2722 | 2701 2738 2730 |
| vk-replay-venus-headless | 1835 1836 1853 | 1806 1832 1756 | 1838 1841 1690 | 1819 1771 1789 | 1770 1837 1819 |
| vkmark-default-venus | 2629 2631 2634 | 2630 2623 2626 | 2564 2627 2626 | 2625 2637 2630 | 2630 2633 2639 |
| vkmark under aquarium 25k | 1371 1369 | 1428 1429 | 1430 1426 | 1163 1166 | 1164 1140 |
| (aquarium fps beside it) | 47 | 44 | 45 | 51 | 50 |
| aquarium 25k (vrend) | 49 45 | 45 45 | 47 45 | 51 49 | 50 49 |
| aquarium 30k (vrend) | 38 40 | 38 43 | 40 38 | 38 38 | 42 43 |
| basemark webgl 1.0.2 | 3683 | -- | 3734 | 3911 | -- |
| basemark webgl 2.0 | 4271 | -- | 4088 | 4678 | -- |
| basemark shader pipeline | 1511 | -- | 1547 | 1520 | -- |
| basemark geometry stress | 1718 | -- | 1729 | 1744 | -- |
| basemark canvas | 1337 | -- | 1350 | 1341 | -- |
| basemark svg | 990 | -- | 989 | 992 | -- |
| basemark draw-call stress | 79.8 | -- | 81.1 | 78.7 | -- |

**No change at this battery's resolution, in either direction.**

- The aquarium overlaps completely: 25k reads 45-50 with the barrier off and 45-51 with it on; 30k
  reads 38-43 in both arms.
- The venus control rows agree across the arms.
- The contended arm measures the boot, not the arm. It reads about 1370-1430 in the first three
  boots and about 1140-1166 in the last two, one of each arm. The aquarium beside it runs fastest
  (50-51 fps) in exactly those two, the inverse pairing earlier passes recorded.
- **Basemark cannot resolve it.** Only one on-arm point scored. n1's WebGL 2.0 (4678) is the
  highest reading, 10-14% above b0 and b1, but it is a single sample with no b2 to bracket it. The
  low-noise tests (geometry, canvas, SVG) agree within 1.5%, and draw-call stress within 3%.

**Basemark's second run stalled on n0 and b2,** one point of each arm. Both reached
`shader_pipeline_test` and never the result page (`REFUSING to report: run 2 yielded no scores
either`). This is the harness stall recorded in earlier passes, and it is not arm-specific. Their
first runs did score (`evidence/<label>/basemark.txt`, "scores (run 1)"). The harness discards run 1
as warm-up, and it is noisy here: draw-call stress reads 66.7 and 72.7 on b0 and b1, 52.4 and 52.2
on n0 and n1, and 50.4 on b2.

## What the stock guest spends its passes on

On the stock guest, one Basemark context runs about 20,000 passes a second, roughly 455 per
submit. The pass ends at `zink_synchronization.cpp:676`, classed "other, attachment: read after
write". This happens at the same rate in both arms (about 100k per 5 s window).

What varies is how long that context stays hot in the first run: 3 windows on b0 and b1, 12-13 on
n1 and b2. That tracks the low run-1 draw-call scores, and it happens in both arms. It accounts
for 0.43M-1.52M passes per point, against about 10k texture-bind splits. This split, not the one
the barrier removes, is where the stock tier's pass count goes.

## Vehicle

- Worker: a debug build with the dev `opt-level = 3` overrides, built by `point.sh` at every point
  from the same tree. The arms differ only in environment.
- Boot: EFI+venus (`boot-enhanced-efi-kk.sh`), 4 vCPU / 4 GiB, `--display-resolution 1280x800`,
  with the guest display pinned as above.
- Enhanced image: `Fedora-Workstation-44.enhanced.raw` (kernel `7.1.8-limina16k.4`,
  `mesa-dri-drivers-26.1.8-11.limina`).
- Stock image: `Fedora-Workstation-44.stock.test.raw` (kernel `6.19.10-300.fc44`, mesa
  `26.1.8-1.fc44`).
- Basemark harness: frozen at virglrs `d0416c9`.
- No watchdog (poisoned-context) marker appeared in any worker log.
