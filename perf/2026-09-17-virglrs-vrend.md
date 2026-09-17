# virglrs `af70f7c..9041fa2` with libkrun `217ba058`: the vrend commits cost nothing

Six vrend commits — a typed transfer direction and compressed-copy rule, texture views taken only
of a driver-witnessed immutable texture, a texture's storage folded into one field, a blit's views
released from one scope, and the classic replay span refusing a journal fed outside it — measured
against the pin, five points alternating baseline/candidate with the baseline at both ends.
**No instrument that can resolve a change separates the two.** One fork commit covers all six:
only `130d7a1` is visible to rutabaga, because it typed the signature rutabaga calls.

Rows are in `virglrs-vrend-2026-09-17/ledger.csv`, evidence under
`virglrs-vrend-2026-09-17/evidence/<point>/`; driver `virglrs-vrend-2026-09-17/point.sh`, point
list `legs.sh`.

## Most of this battery cannot see this pass

Every commit under test is vrend. On the enhanced guest, `perf-ledger` and `vkmark` run
zink→venus and never enter vrend, so **their flatness is arithmetic, not evidence.** The arms that
reach vrend are the aquarium and Basemark on the stock guest. They are the ones to read; the venus
arms are here to catch collateral damage, and caught none.

Two commits deserve naming separately:

- **`14ba091`** (the minted image and exported descriptor folded into one `Pixels` field) risks a
  wrong or unfilled scanout — a pixel defect, not a throughput one. A battery would run at full
  speed showing black. It is scored below by the suite's pixel oracles, not by any row here.
- **`e81bc17`** (a blit's views released from one scope) changes only the refused-destination
  branch. No healthy workload takes it, the steady path gained no GL call and no allocation, and
  no oracle we have observes a leaked GL name. Flat is the correct and expected result here, and
  this battery does not score it.

## The pixel evidence, which the battery cannot supply

Run 2026-09-17 on `9041fa2` + `217ba058` with output uncaptured
(`virglrs-vrend-2026-09-17/oracle-pixel-run.txt`). **A plain suite run cannot establish any of
this:** nextest captures a passing test's output and all three tests return early — as a PASS —
when the KosmicKrisp ICD or a disk image is absent, so a green tick cannot be told from a test
that declined to run. No `SKIPPED` line appeared, so all three genuinely executed.

| oracle | reading |
|---|---|
| `classic_vrend_world_survives_snapshot_restore` | live baseline **471** distinct quantized colors, post-restore **437** (floor 200; gate post×4 ≥ pre) |
| | `scanout_rejects=0 submit_rejects=0 submits=+76 errs=+0` over 6 ticks |
| `stock_guest_vulkan_client_composites_its_own_pixels` | vkcube changed **262 077** pixels, **0** pure black (0.0%) |
| `classic_virgl_gbm_buffer_imports_into_venus` | `ALIAS OK` + `CLASSICIMPORT PASS` for both classes (scanout, rendering) |

Its own premise guards fired too, which is what makes the first row mean anything:
`no-virgl-hits=0` with `Created gbm renderer for /dev/dri/card0` (the seated shell really was on
classic virgl, not an llvmpipe fallback), and a pre-suspend `submits=+40` baselining the submit
oracle on the live world before the post-restore reading was trusted.

The HVF suite on the pair ran **139/139, 0 skipped, 2216.8 s** against a 2282 s norm.

## The table

Control and replays are the range over three perf-ledger runs; idle vkmark three runs; the
contended arm two runs of vkmark with a 25 000-fish aquarium beside it, the aquarium's own fps
read off the proof capture; aquarium two runs each, read off the fps crops; Basemark one suite,
second run scored. `b` points are the pin, `n` points the candidate.

| point | ctrl | vk-replay | glmark2 | vkmark | vkmark + aq25k (aq fps) | aq25k | aq30k | W1 | W2 | Shader | Draw | Geom | Canvas | SVG |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| b0 `af70f7c` | 730–731* | 2190–2288* | 3073–3151 | 3988–4015 | 1523 / 1524 (44) | 51/45 | 40/38 | 3322 | 3876 | 1614 | 78.5 | 1638 | 1187 | 995 |
| n0 `9041fa2` | 722–730 | 2047–2286 | 3027–3138 | 4048–4206 | 1122 / 1147 (46) | 50/51 | 39/38 | 3308 | 3969 | 1624 | 81.1 | 1730 | 1175 | 992 |
| b1 `af70f7c` | 723–727 | 2187–2224 | 3108–3125 | 4095–4212 | 938 / 891 (45) | 44/50 | 38/38 | 3563 | 3922 | 1629 | 81.8 | 1721 | 1172 | 987 |
| n1 `9041fa2` | 723–728 | 2207–2272 | 3118–3148 | 4115–4140 | 1402 / 1419 (41) | 44/45 | 43/38 | 3396 | 3824 | 1644 | 79.7 | 1602 | 1171 | 990 |
| b2 `af70f7c` | 715–736 | 2210–2266 | 3071–3124 | 4101–4197 | 1125 / 1116 (50) | 45/46 | 38/38 | 3134 | 3923 | 1486 | 79.2 | 1578 | 1180 | 983 |

\* **b0's first perf-ledger run is discarded, not labelled.** Its control read 687.3, below the
healthy 717–734, and `gl-replay-venus` simultaneously read 57.69 against the 56.8 it has held
across every pin for months — two needles off in one window, taken immediately after that boot's
two aquarium sessions. Runs 2 and 3 recovered to 729.5 and 731.0. A low control means discard:
contention distorts the shape of a comparison, not only its level. It falls on a **baseline**, so
it cannot manufacture a pass.

## Reading it

- **Basemark, the vrend instrument, shows no step.** On all seven tests the candidate sits inside
  the three baselines' span or within ~1% of it, **and the excursions go both ways**: n0's WebGL
  2.0 is 3969 against a baseline ceiling of 3923, n1's is 3824 against a floor of 3876. Two-sided
  excursions of that size are the instrument, not the tree. The sub-1% tests are the ones that
  place a step and they place none — Canvas 1171–1187, SVG 983–995, Draw-call 78.5–81.8 across all
  five points, candidates interleaved with baselines throughout.
- **The aquarium shows no step.** 25k spans 44–51 on baselines and 44–51 on candidates; 30k spans
  38–40 on baselines and 38–43 on candidates. The single excursion, n1's 43 at 30k, is *above* the
  baseline ceiling.
- **The venus arms found no collateral damage.** Idle vkmark, vk-replay and glmark2 all overlap
  across arms. They could not have scored these commits and did not.
- **Ten guests, zero poisoned-context lines**, no `WATCHDOG`/`WEDGED`/`POISONED`/`INVALID`/
  `UNPROVEN`/`REFUSING` line, and every Basemark suite scored.

## vkmark under a 25k aquarium: the 2026-09-16 correlation does not reproduce

That pass reported the arm clustering by the competing aquarium's fps — ~1600 at 44–46 fps, ~1300
at 50 — and the 09-17 robustness pass saw the direction hold. **This run falsifies the
cluster-level claim.** Ordered by competitor fps: 41 → 1410, 44 → 1523, 45 → 915, 46 → 1135,
50 → 1120. `b0` and `b1` are the same pin with near-identical competitor fps (44 and 45) and
differ by 1.7× (1523 vs 915); the three baselines alone span 891–1524.

What survives is only this: **the arm's per-boot spread exceeds any effect it could be asked to
resolve, and no account of that spread has replicated.** Naming it after the competitor's fps was
naming an unexplained effect after a mechanism, which is how it would have entered the next
reader's noise model as fact. The instrument is kept for continuity and scores nothing; a version
that could would need the competitor frame-paced rather than unthrottled, or both clients' fps
read together.

## Vehicle

limina `84024d28` against each virglrs + libkrun pair, `cargo xtask build` (debug worker, renderer
and its hot dependencies at opt-level 3), host mesa `limina-kk` at `bb3994fc` for the whole run.
4 vCPU / 4 GiB, display pinned 1280x800 @ 1.0, fresh `cp -c` clones of
`Fedora-Workstation-44.enhanced.raw` and `Fedora-Workstation-44.stock.test.raw` per boot, guest
settled (uptime ≥ 200 s, load < 0.3) before measuring. Basemark through the frozen `d0416c9` copy
of virglrs's `harness/vm/client-basemark.sh`. One boot pair per point, ~32 minutes each; the whole
run 15:53–18:36.
