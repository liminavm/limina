# Asynchronous hardware decode: collateral check

**Subject:** virglrs `9c5b3e2..d57be22` + libkrun `f393db7f..32cc3776` (decode on a thread per
codec, settle barriers at every read, `settle_video` before a snapshot). Rows, evidence and the
driver: `perf/async-decode-2026-09-23/`; nothing went to `perf/ledger.csv`.

## What this pass can and cannot see

No arm decodes video. What the battery can see is the cost of the barriers when nothing is in
flight -- one atomic load per resource lookup, draw, transfer and fence -- on the instruments that
enter vrend (Basemark on the stock guest, the WebGL aquarium) and, as collateral, on the
zink->venus arms. The decode win itself is measured elsewhere: `l2_video_decode_off_thread`, and
the poke run's `vrend video:` lines (0.03 ms/frame and 0.2 ms worst command at 25 fps VP9), and
`spikes/flush-latency/RESULTS.md`.

**The host was not quiet.** Other builds ran during the sweep. The `gl-replay-llvmpipe` control is
in its band (717-757) at b0, n0 and b1, low at n1 (667, 684, 724) and far out at b2 (719, 370,
468). n1 and b2 are contended points; treat anything that rests on them as unscored.

## Results

Order b0 n0 b1 n1 b2, one boot each; b = `9c5b3e2`+`f393db7f`, n = `d57be22`+`32cc3776`.

| workload | b0 | n0 | b1 | n1 | b2 |
|---|---|---|---|---|---|
| gl-replay-llvmpipe (control) | 723 740 738 | 719 726 730 | 721 733 735 | 667 684 724 | 719 370 468 |
| gl-replay-venus | 46.6 46.3 46.4 | 45.5 46.3 45.9 | 46.0 45.8 45.5 | 46.1 45.5 46.5 | 45.2 45.9 46.5 |
| glmark2-wayland-venus | 2500 2511 2495 | 2501 2528 2509 | 2490 2491 2525 | 2477 2471 2490 | 2509 2435 2466 |
| vk-replay-venus-headless | 1802 1920 1914 | 1796 1808 1864 | 1800 1887 1824 | 1770 1830 1733 | 1887 1768 1759 |
| vkmark-default-venus | 2375 2373 2384 | 2194 2092 2221 | 2188 2238 2185 | 2228 2232 2163 | 1816 2053 1955 |
| aquarium 25k (vrend) | 43 44 | 48 50 | 44 43 | 46 5 | 49 45 |
| aquarium 30k (vrend) | 40 39 | 39 38 | 34 42 | 33 39 | 36 40 |
| basemark webgl 1.0.2 | -- | 3346 | 3570 | 3144 | 2824 |
| basemark webgl 2.0 | -- | 4378 | 4257 | 4501 | 3365 |
| basemark shader pipeline | -- | 1243 | 1314 | 1311 | 1570 |
| basemark geometry stress | -- | 1743 | 1740 | 1733 | 1732 |
| basemark canvas | -- | 1169 | 1173 | 1169 | 1135 |
| basemark svg | -- | 985 | 998 | 984 | 981 |
| basemark draw-call stress | -- | 81.1 | 82.8 | 168.8 | 80.3 |

**No regression is visible at this battery's resolution.** Every venus arm overlaps across the
pins. On the vrend arms, the quiet-control comparison is n0 against b0/b1: aquarium 25k reads
48/50 against 43-44, 30k 38-39 against 34-42, and Basemark's sub-1% tests (geometry, canvas, SVG)
agree to within 1% between n0 and b1. That is consistent with an atomic load per lookup costing
nothing measurable, and it is not evidence of a speed-up: the aquarium's floor is ~15%.

**vkmark idle drifted, not the treatment.** b0 read 2375-2384 and every later point 1816-2238,
baseline and treatment alike, falling toward the contended end.

**Outliers, not findings:** aquarium 25k at 5 fps (n1, run 2) is one instantaneous counter read
on a contended point; draw-call stress at 168.8 (n1) is double the other three, on the same
contended point. Basemark at b0 stalled inside Shader Pipeline on the baseline tree (`REFUSING to
report: run 2 yielded no scores`) -- the stall recorded against other trees in
`perf/2026-09-11-trend-d5889ef.md`, not this change.

A rerun on a quiet host would tighten the vrend comparison; nothing here asks for one.

## Vehicle

Debug worker with the dev `opt-level = 3` overrides, built per point by `point.sh`; EFI+venus
(`boot-enhanced-efi-kk.sh`), 4 vCPU / 4 GiB, display pinned 1280x800 by `--display-resolution`;
enhanced image `Fedora-Workstation-44.enhanced.raw`, stock `Fedora-Workstation-44.stock.test.raw`;
Basemark harness frozen at virglrs `d0416c9`. No watchdog (poisoned-context) marker at any point.
