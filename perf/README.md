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
- `glmark2-wayland-venus` — live glmark2-es2-wayland build-scene score (the classic battery
  number, vsync-free offscreen-ish but composited). ⚠ **The name lies after 2026-08-04.** The
  workload measured zink→venus up to the GL default flip; since then the identical command runs
  on **vrend** (`docs/graphics.md` §3.2). Rows either side of that date are not comparable. The
  name was left alone deliberately — renaming it would misdescribe the earlier half instead.

- `glmark2-display-*` — the on-display three-tier comparison (venus / software-2D / virgl),
  `-b build -b shading -b texture` at 800x600 through the full compositor. **The `virgl` row is
  vsync-limited, not throughput-limited** (a 64x64 scene scores the same) — do not rank tiers with
  it; use `aquarium-*`. See `docs/graphics.md` §6.
- `aquarium-*` — WebGL aquarium fps, on-display, the workload that actually measures throughput
  on all three tiers. Fully automated by `scripts/perf/aquarium-run.sh` (drives Firefox, harvests
  the supervisor frame dump, crops the counter) — no human needed.
  **The default sweep is 5k/10k/15k/20k/25k/30k fish since 2026-08-08.** The old 5k/10k/15k set
  stopped discriminating on the GPU tiers: both GL paths now return a flat 60 across all three,
  which is the **vsync ceiling, not throughput** — a 60 hides arbitrary headroom exactly the way
  the `glmark2-display-virgl` row does. Separation appears at **20 000 fish** (vrend 60 vs
  zink-on-venus 48); at 25k/30k both are GPU-bound and converge (42/42, 39/38). Read any cell
  reading 60 as "≥60, capped", and never rank tiers on one. The low counts stay in the sweep for
  the software-2D tier and for continuity with the historical rows.
  ⚠ Numbers from the **unpaced-present era** (through 2026-07-26, e.g. venus 71 @5k) are not
  commensurable with post-fence-accurate-present rows — a counter above the mode is the tell.

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
