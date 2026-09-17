# virglrs `deb095b..d833259`, leg by leg: nothing moved, the pin follows

The battery across the seven groups of virglrs commits after limina's pin `deb095b`, one boot pair
per leg, the baseline and the tip each measured twice. **No leg moves any instrument that can
resolve it.** virglrs is pinned at `d833259` and libkrun at `3fa4cd9b`, the rutabaga that follows
its API; the HVF suite on that pair ran 139/139 green (2026-09-17, 2193 s). Rows are in `virglrs-legs-2026-09-16/ledger.csv`, evidence under
`virglrs-legs-2026-09-16/evidence/<point>/`; the driver is `virglrs-legs-2026-09-16/point.sh`
and the leg list `legs.sh`.

## The legs

The split is virglrs-review's, each leg named by the virglrs tip it builds at, the range being
what it adds over the previous leg. Three legs change the crate's API, so each builds against
the libkrun whose rutabaga compiles with it, and the pin bump carries both libkrun commits.

| leg | virglrs | libkrun | what it adds | expected on |
|---|---|---|---|---|
| b0 | `deb095b` | `debc082c` | the pin | — |
| l0 | `f0fdc66` | `debc082c` + `libkrun-debc082c-surface-rename.patch` | resource-drop ownership, the surface rename, harness | nothing |
| l1 | `985d1da` | `bc045955` | trust-boundary refusals; a sampler unit marked dirty when its state is bound | classic, small |
| l2 | `bc4b864` | `3fa4cd9b` | venus: the command census no longer held across a batch, driver waits run outside the batch, timed waits sliced | vkmark under load |
| l3 | `1566e77` | `3fa4cd9b` | submit-stats instruments; the shader marked dirty on a rasterizer bind | classic, small |
| l4 | `ec5f1e9` | `3fa4cd9b` | the program reselected on dirt alone on BGRA targets | classic, null on replay |
| l5 | `8d8a3d4` | `3fa4cd9b` | a scatter list walked once per transfer, not once per row | classic transfers |
| l6 | `d833259` | `3fa4cd9b` | transfers staged through one kept buffer | classic transfers |

## The table

Control and replays are the range over three perf-ledger runs; idle vkmark three runs; the
contended arm two runs of vkmark with a 25 000-fish aquarium beside it, the aquarium's own fps
read off the proof capture; aquarium two runs each; Basemark one suite, second run scored.

| point | ctrl | vk-replay | glmark2 | vkmark | vkmark + aq25k (aq fps) | aq25k | aq30k | W1 | W2 | Shader | Draw | Geom | Canvas | SVG |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| b0 `deb095b` | 727–739 | 2130–2268 | 3057–3159 | 4160–4249 | 1309 / 1295 (50) | 48/51 | 38/39 | 3626 | 4516 | 1760 | 79.9 | 1724 | 1167 | 996 |
| l0 `f0fdc66` | 731–737 | 2007–2217 | 3125–3152 | 4195–4245 | 1635 / 1638 (unproven) | 45/45 | 43/38 | 3494 | 4285 | 1580 | 80.0 | 1729 | 1179 | 1002 |
| l1 `985d1da` | 719–730 | 2198–2215 | 3076–3136 | 4132–4154 | 1607 / 1601 (46) | 45/51 | 43/38 | 3497 | 4018 | 1551 | 81.4 | 1726 | 1184 | 987 |
| l2 `bc4b864` | 727–731 | 2180–2223 | 3050–3142 | 4055–4161 | 1612 / 1611 (44) | 45/50 | 43/38 | 3346 | 4052 | 1551 | 80.1 | 1727 | 1177 | 999 |
| l3 `1566e77` | 724–732 | 2076–2155 | 3124–3137 | 4063–4194 | 1144 / 1153 (50) | 42/50 | 38/38 | 3150 | 4044 | 1681 | 79.7 | 1732 | 1172 | 996 |
| l4 `ec5f1e9` | 725–731 | 2102–2234 | 2917–3132 | 4015–4198 | 1616 / 1621 (44) | 50/43 | 40/38 | 3533 | 3738 | 1748 | 80.0 | 1730 | 1167 | 996 |
| l5 `8d8a3d4` | 728–728 | 2091–2245 | 2916–3129 | 4177–4200 | 1320 / 1315 (50) | 47/46 | 42/38 | 3392 | 3894 | 1520 | 80.4 | 1736 | 1172 | 997 |
| l6 `d833259` | 726–733 | 2011–2203 | 3118–3149 | 4155–4240 | 1310 / 1294 (50) | 44/44 | 43/43 | 3390 | 3786 | 1565 | 79.3 | 1724 | 1186 | 997 |
| r0 `deb095b` | 725–735 | 2144–2237 | 2922–3074 | 4077–4201 | 1586 / 1580 (45) | 44/49 | 39/38 | 3234 | 3891 | 1465 | 80.1 | 1730 | 1170 | 988 |
| r6 `d833259` | 726–732 | 2111–2173 | 3092–3156 | 4160–4271 | 1624 / 1628 (45) | 46/45 | 43/43 | 3282 | 3809 | 1516 | 80.6 | 1720 | 1170 | 994 |

The control sat in 719–739 on every boot, so every point is readable. No context was poisoned
and no Basemark run stalled.

## Reading it

- **The classic legs (l1, l3, l4, l5, l6) are flat on the instruments that resolve under 1%.**
  Geometry Stress 1720–1736, Canvas 1167–1186, SVG 987–1002 and Draw-call 79.3–81.4 across all
  ten points, the baseline's two boots included. Aquarium spans 42–51 at 25k and 38–43 at 30k
  with no leg outside what the two baseline boots produced (44–51, 38–39). l5's scatter-list walk,
  the one change with a real guest-shape effect the replay could not show, is a real null on this
  guest: Canvas and aquarium both sit in band, and this vehicle's glmark2, gl-replay and vk-replay
  all run on zink→venus, so they never saw a vrend transfer.
- **The venus leg (l2) is flat on idle vkmark and vk-replay** — 4055–4161 against the baseline's
  4160–4249 and 4077–4201 across two boots; vkmark's between-boot floor is ~1%, and the l2 band
  overlaps r0's. The instrument that was to score it, vkmark under load, cannot — see below.
- **The tip repeats the baseline.** r6 against r0: Geometry 1720 / 1730, Canvas 1170 / 1170, SVG
  994 / 988, aquarium 46/45 / 44/49, idle vkmark 4160–4271 / 4077–4201, vk-replay 2111–2173 /
  2144–2237, Basemark WebGL 2.0 3809 / 3891.

## Two instruments learned something

**vkmark under a 25k aquarium is bimodal on the GPU split, and the mode is chosen per boot, not
per leg.** Across the nine proven boots it read either ~1300 (1295–1320, and l3's 1144–1153) or
~1600 (1580–1628), never between. Which mode a boot fell in tracked the aquarium's own fps beside
vkmark: 50 in every ~1300 boot (b0, l3, l5, l6), 44–46 in every ~1600 boot (l1, l2, l4, r0, r6).
The same pin read both modes: b0 1309 / 1295, r0 1586 / 1580. So the number is how the host GPU
was shared between two clients for that boot, and a leg's effect on contention is invisible under
it. l3's 1144 / 1153, the one reading that looked like a step, is the ~1300 mode with a lower
floor, between two ~1600 boots on neighbouring trees. The instrument as run cannot score leg 2;
one that could would need the split pinned (a frame-paced client instead of an unthrottled one,
or the two clients' fps read together) rather than more boots.
*The proof capture is part of the instrument:* l0's read empty (the supervisor rewrites the dump
in place and a plain `cp` caught it mid-write), so its 1635 / 1638 is unproven and excluded; from
l1 on the driver waits for a new dump and a crop with a counter, as `aquarium-run.sh` does.

**Basemark's two WebGL tests drifted with the host over the evening, not with the tree.** WebGL
2.0 fell 4516 → 3786 and WebGL 1.0.2 3626 → 3390 from b0 to l6 with no single step; the baseline
measured again at the end (r0, four and a half hours later) read 3891 and 3234, at the tip's
level. Read alone the sweep would have placed a ~15% WebGL 2.0 regression somewhere in l0..l4;
the end-of-run baseline is what says it was the host. This is the case for measuring the baseline
at both ends of any sweep that takes hours, on the tests that scatter (WebGL 2.0, WebGL 1.0.2,
Shader Pipeline) even more than on the ones that do not.

## Vehicle

limina `1d955098` against each virglrs + libkrun pair, `cargo xtask build` (debug worker,
renderer and its hot dependencies at opt-level 3), host mesa `limina-kk` at `bb3994fc` for the
whole run (both the zink prefix and the KosmicKrisp build; the manifest's `5e99e7c` is one commit
behind that checkout). 4 vCPU / 4 GiB, display pinned 1280x800 @ 1.0, fresh `cp -c` clones of
`Fedora-Workstation-44.enhanced.raw` (guest `7.1.8-limina16k.4`, mesa `26.1.8-11`) and
`Fedora-Workstation-44.stock.test.raw` (`6.19.10-300.fc44`, mesa `26.1.8-1`) per boot, guest
settled (uptime ≥ 200 s, load < 0.3) before measuring. Basemark through the frozen `d0416c9`
copy of virglrs's `harness/vm/client-basemark.sh`, unchanged at `d833259`. One boot pair per
point, ~32 minutes each; the whole run 19:29–00:52.
