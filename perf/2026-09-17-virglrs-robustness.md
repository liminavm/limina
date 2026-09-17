# virglrs `d833259..af70f7c` with libkrun `02498445`: the robustness commits cost nothing

The eight commits virglrs added after limina's pin `d833259` — replay-span and journal refusals,
the `PlaneLayouts` surface value, stream depth counted on `Handlers`, and the typed-witness
renderer/context resolution — measured against the pin, five points alternating
baseline/candidate with the baseline at both ends. **No instrument that can resolve a change
separates the two.** The pin bump carries two revs: libkrun `02498445` drops `is_classic` and the
per-renderer journal/replay pairs and compiles only against the new virglrs.

Rows are in `virglrs-robustness-2026-09-17/ledger.csv`, evidence under
`virglrs-robustness-2026-09-17/evidence/<point>/`; the driver is
`virglrs-robustness-2026-09-17/point.sh` and the point list `legs.sh`.

## What the robustness commits claim, and what actually checks it

These commits harden paths that a throughput battery is not the oracle for. The affirmative
evidence is elsewhere in this run, and it is clean:

- **Ten guests booted, zero poisoned-context lines** — five enhanced (venus) and five stock
  (vrend), every one reporting `0 poisoned-context lines` in its worker log.
- **No guard fired across the whole 2.5-hour run**: no `WATCHDOG`, `WEDGED`, `POISONED`,
  `INVALID`, `UNPROVEN` or `REFUSING` line. Every Basemark suite scored; none stalled.
- **The HVF suite on the candidate pair ran 138/139**, the single failure being
  `venus_desktop_pixel_verifies_through_host_capture` timing out waiting for the guest's sshd
  (240 s) while virglrs-review ran four cargo-test cycles plus a 12k-command vrend replay across
  the exact window; the run took 2481.8 s against a 2282 s norm. Re-run alone it passed with its
  real oracle firing — 30,772 distinct colours, dominant 0.36 — so the pixel path was never in
  question. A load flake with a named external cause, not a regression.

The battery below is the null it ought to be.

## The table

Control and replays are the range over three perf-ledger runs; idle vkmark three runs; the
contended arm two runs of vkmark with a 25 000-fish aquarium beside it, the aquarium's own fps
read off the proof capture; aquarium two runs each, read off the fps crops; Basemark one suite,
second run scored. `b` points are the pin, `n` points the candidate.

| point | ctrl | vk-replay | glmark2 | vkmark | vkmark + aq25k (aq fps) | aq25k | aq30k | W1 | W2 | Shader | Draw | Geom | Canvas | SVG |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| b0 `d833259` | 728–729 | 2221–2259 | 3108–3138 | 4045–4183 | 1514 / 1526 (43) | 49/50 | 42/41 | 3287 | 3886 | 1570 | 81.6 | 1730 | 1178 | 998 |
| n0 `af70f7c` | 724–727 | 2172–2267 | 2870–3109 | 3925–4120 | 1512 / 1492 (46) | 45/49 | 37/43 | 3352 | 3842 | 1556 | 79.6 | 1738 | 1178 | 990 |
| b1 `d833259` | 724–736 | 2071–2251 | 3102–3151 | 3922–4152 | 1552 / 1526 (45) | 47/44 | 39/39 | 3475 | 3849 | 1529 | 80.8 | 1721 | 1178 | 986 |
| n1 `af70f7c` | 727–729 | 2197–2242 | 2860–3133 | 4160–4290 | 1004 / 1190 (49) | 45/43 | 38/42 | 3205 | 3843 | 1594 | 80.3 | 1670 | 1182 | 992 |
| b2 `d833259` | 725–735 | 2098–2132 | 2874–3137 | 4064–4121 | 1674 / 1656 (42) | 45/49 | 43/43 | 3355 | 3982 | 1575 | 78.7 | 1613 | 1186 | 994 |

The control sat in 724–736 on every boot, so every point is readable and none is a discard.

## Reading it

- **The sub-1% Basemark tests are flat.** Canvas 1178–1186, SVG 986–998, Draw-call 78.7–81.6
  across all five points, with both candidate points inside the three baselines' span on each.
  These are the tests that place a step, and they place none.
- **Idle vkmark and vk-replay do not separate.** The candidate's vkmark bands (3925–4120,
  4160–4290) straddle the baselines' (4045–4183, 3922–4152, 4064–4121) — n1 is the highest point
  of the run. vk-replay is the same picture with the drift below subtracted.
- **`gl-replay-venus` reads 56.74–56.84 everywhere**, as it has for every change ever made to
  this stack. It is in the table because it is in the ledger, not because it means anything.
- **The aquarium spans 43–50 at 25k and 37–43 at 30k**, with no candidate run outside what the
  three baseline boots produced (44–50, 41–43).

## Two instruments, read carefully

**Geometry Stress and vk-replay drifted with the host, and only the closing baseline says so.**
Geometry fell 1730 → 1738 → 1721 → 1670 → 1613 in time order and vk-replay 2245 → 2119
baseline-to-baseline; both low points are `b2`, *the pin*. Read one-ended, this run would have
booked a ~3.5% Geometry regression on the candidate. Geometry's recorded between-boot spread is
under 1%, so a tight instrument is no protection against a multi-hour sweep — and the llvmpipe
control saw none of it (724–736 throughout), because it is CPU-bound inside the guest while this
is host-renderer speed. The control proves contention; only the repeated baseline proves drift.

**vkmark under a 25k aquarium still tracks the competitor, not the tree.** Ordered by the
aquarium's own fps: 42 → 1665, 43 → 1520, 45 → 1539, 46 → 1502, 49 → 1097. The direction of the
2026-09-16 correlation reproduces on independent data; the levels do not (that pass read ~1600 at
44–46 fps and ~1300 at 50, against ~1500–1540 and ~1100 here), so the clusters are not fixed
landmarks. `n1`'s two runs also disagree with each other by 18% (1004 / 1190) where every other
pair in both passes agrees within 2%, and a single proof capture covers both runs, so the
competitor may have moved between them — unexplained, and noted rather than absorbed. **The
instrument as run cannot score this change**, and `n1`'s low pair is not evidence of one.

**glmark2 ran bimodally on both arms.** Every run landed either ~3100–3151 or ~2860–2874, nothing
between: 1 of 9 baseline runs in the low mode, 3 of 6 candidate runs. The same split appears on
the old pin in the 09-16 pass, glmark2's documented between-boot floor is ±10%, and the low mode
here is 8% down — so the counts are recorded without a verdict, not resolved.

## Vehicle

limina `9125bdd3` against each virglrs + libkrun pair, `cargo xtask build` (debug worker,
renderer and its hot dependencies at opt-level 3), host mesa `limina-kk` at `bb3994fc` for the
whole run. 4 vCPU / 4 GiB, display pinned 1280x800 @ 1.0, fresh `cp -c` clones of
`Fedora-Workstation-44.enhanced.raw` and `Fedora-Workstation-44.stock.test.raw` per boot, guest
settled (uptime ≥ 200 s, load < 0.3) before measuring. Basemark through the frozen `d0416c9` copy
of virglrs's `harness/vm/client-basemark.sh`. One boot pair per point, ~32 minutes each; the
whole run 11:53–14:34.
