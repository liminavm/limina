# zink's MoltenVK workarounds on KK — aquarium A/B

**Question.** limina-kk de950d67e16 applies zink's MoltenVK-only workarounds only when MoltenVK is
the running driver. On KosmicKrisp that turns on fbfetch (and with it
`KHR_blend_equation_advanced`) and dynamic vertex input stride. Does the stride change move
WebGL throughput on the vrend tier?

**Answer: no measurable change.** At 25k fish the arms average 46.8 (base) and 48.3 (as shipped);
at 30k, 40.3 and 39.8. The spread inside each arm (45–51 at 25k, 38–44 at 30k) is wider than the
gap between them, and the order flips between the two loads.

## Setup

Measured 2026-10-07. One build for both arms: host Mesa de950d67e16 (the shared `zink-kk-prefix`),
virglrs cd716cf, libkrun e3c140ad, limina 5328e111; guest `Fedora-Workstation-44.enhanced.raw`
(cloned per point), 4 vCPU / 4 GiB, display pinned 1280x800 @ 1.0, Firefox aquarium on the vrend
tier, 25k and 30k fish, two runs per point.

- **base** — `LIMINA_ZINK_MVK_WORKAROUNDS=1`: the workarounds apply on KK, as before the change.
- **mvk** — switch unset: the change as it ships.

Points alternate `b0 m0 b1 m1 b2`. Each point proves its arm from the worker's environment
(`evidence/<point>/arm-env.txt`), and every point ran clean (no device loss, no poisoned context).
fps is read by eye off `evidence/<point>/aquarium-r*/vrend-*-fps.png`; rows are in `ledger.csv`.

| point | arm  | r1 25k | r1 30k | r2 25k | r2 30k |
|-------|------|-------:|-------:|-------:|-------:|
| b0    | base | 45 | 39 | 45 | 39 |
| m0    | mvk  | 46 | 39 | 50 | 38 |
| b1    | base | 51 | 39 | 50 | 44 |
| m1    | mvk  | 46 | 39 | 51 | 43 |
| b2    | base | 45 | 43 | 45 | 38 |

## Reproduce

`perf/mvkgate-2026-10-07/legs.sh > perf/mvkgate-2026-10-07/legs.log 2>&1` (about 35 min), then
`aq-rows.sh <point> <r1-25k> <r1-30k> <r2-25k> <r2-30k>` per point after reading the crops.

The arms are a runtime switch in one build because selecting a host Mesa by prefix
(`LIMINA_HOST_GALLIUM=1` with `MESA_PREFIX`) fails EGL init in the worker; see
`docs/hardening-backlog.md`.
