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

- `glmark2-display-*` — the on-display three-tier comparison (venus / software-2D / virgl),
  `-b build -b shading -b texture` at 800x600 through the full compositor. **The `virgl` row is
  vsync-limited, not throughput-limited** (a 64x64 scene scores the same) — do not rank tiers with
  it; use `aquarium-*`. See `docs/tiers.md` §Performance.
- `aquarium-*` — WebGL aquarium fps at 5k/10k/15k fish, on-display. This is the workload that
  actually measures throughput on all three tiers. Fully automated by `scripts/perf/aquarium-run.sh`
  (drives Firefox, harvests the supervisor frame dump, crops the counter) — no human needed.

## Before you measure ANYTHING: pin the display

`match-host` (default since 2026-07-03) drives the guest to the host screen, and GNOME then picks a
**fractional** scale — 2560x1440 @ 2.5 on the M1 Max, which reaches wayland clients as
`buffer_scale 3`. That makes a WxH window a 3W x 3H buffer (~9x the pixels), and makes glmark2's
512x512 default a **wl protocol error** whose score is garbage (it produced 97 and 274 on
back-to-back identical runs, 2026-07-26). A *stock* guest defaults to scale 1.3333 — nobody is
exempt.

```bash
limina --display-resolution 1280x800 …                          # supervisor drives the mode
ssh guest '~/bin/set-guest-display.py --write-config 1280x800 1.0' && reboot the guest
```

Use `--write-config` + reboot, **not** the live D-Bus apply: the live path pops GNOME's "Keep
changes?" dialog, which reverts in ~20 s if nobody clicks — it will block an unattended run or
un-pin the display mid-measurement. `perf-ledger.sh` `--verify`s (read-only) and aborts if the
display is not pinned.

Environment matters. Rows through 2026-06-25 were taken in the windowed seated boot
(`spikes/venus-draw-probe/boot-seated-kk.sh`, injected kernel, `selinux=0`, 4 vCPU / 4 GiB). From
**2026-07-26** the vehicle is the current default — `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`
(EFI boot of the guest's own kernel, SELinux **enforcing**) — still at 4 vCPU / 4 GiB and
1280x800 @ 1.0 so the numbers stay comparable. Rows through 2026-06-25 also carried
`VN_PERF=no_*_feedback`, which was **retired from the shipped guest env 2026-07-25**; rows from
2026-07-26 are as-shipped without it (measured worth ~1.3% on `gl-replay-venus`). Record anything
unusual (knob overrides, concurrent work) in the notes column.
