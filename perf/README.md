# Performance trend ledger

`ledger.csv` accumulates dated performance measurements of the tier-2 graphics stack,
appended by `scripts/perf-ledger.sh` (run it on demand — e.g. after a mesa/KK/venus
change — with the seated desktop up). The file is tracked in git so trends live in
history alongside the code that moved them.

**This is a trend ledger, NOT a pass/fail gate** (decision 2026-06-12): VM-on-dev-machine
variance (thermals, host load, what else is running) makes hard thresholds flaky. Humans
read trends from it; nothing in the test suite asserts on these numbers. Correctness is
the replay tests' job (`venus_replay`, `venus_vk_replay`).

Workloads (see `scripts/perf-ledger.sh` for the exact invocations):

- `gl-replay-venus` — deterministic apitrace replay of the glmark2-build fixture on
  zink→venus, fps. Runs through Xwayland, so it is ALSO the tracking number for the
  X11-present-slowness open thread (~50× slower than Wayland as of 2026-06-12).
- `gl-replay-llvmpipe` — same trace on llvmpipe, fps. CPU-side control: if this moves,
  the cause is not the venus stack.
- `vk-replay-venus-headless` — gfxreconstruct replay of the vkcube fixture on venus with
  `--wsi headless` (no present, no vsync cap), fps. The purest venus-pipeline
  throughput number of the three.
- `glmark2-wayland-venus` — live glmark2-es2-wayland build-scene score on zink→venus
  (the classic battery number, vsync-free offscreen-ish but composited).

Environment matters: measurements are taken in the **windowed seated boot**
(`spikes/venus-draw-probe/boot-seated-kk.sh`, KK ICD, present-copy on) — the same
vehicle as the historical knob-ledger numbers in memory `limina-profiling-playbook`.
Record anything unusual (host load, knob overrides) in the notes column.
