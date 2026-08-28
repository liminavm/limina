# What the vCPU band costs the battery

Measured 2026-08-28 on the M1 Max, on battery, display on and brightness fixed, host otherwise
idle, under `caffeinate -di`. `battery-cost.sh` boots one guest per arm (the policy is read once,
at vCPU start), samples the pack every 5 s for six minutes, and prints each block's mean draw;
`pm-align.py` reads a root `powermetrics` log against those same block windows. Arms are
interleaved, never A-then-B — the pack's voltage sags as it drains.

Every block verifies the differential reached the guest by counting `[VCPU-RT]` lines in the
worker log: 8 on a banded block, 0 on an unbanded one. Both are printed in the result files.

## The pack cannot see the band at all

| arm | mean W | 
|---|---|
| no VM at all | 5.27, 5.09 |
| idle guest, unbanded | 5.12, 4.87 |
| idle guest, `rt+dyn` | 5.30, 5.15 |
| vkcube, unbanded | 7.85, 7.19 |
| vkcube, `rt+dyn` | 7.88, 7.66 |

Banded minus unbanded is +0.22 W idle and +0.25 W presenting, against a within-arm block spread of
up to 0.66 W. The difference is inside the noise, so this measures a **bound** — under ~0.3 W idle
— and not an effect. Two things make the pack a blunt instrument here: the display dominates the
total, and `AppleRawCurrentCapacity` is a fuel-gauge *estimate* that re-fits itself, once rising
34 mAh during a six-minute discharge. Integrated watts are the only sound reading; the mAh column
is kept for the record and should not be differenced.

## Package power can, and it says the same thing

| arm | package | CPU | GPU |
|---|---|---|---|
| no VM at all | 98 mW | 94 | 4 |
| idle guest, unbanded | 111 | 108 | 3 |
| idle guest, `rt+dyn` | 128, 104 | 124, 101 | 4, 3 |
| vkcube, unbanded | 883, 795 | 634, 560 | 249, 234 |
| vkcube, `rt+dyn` | 989, 997 | 713, 720 | 277, 277 |

Idle, the band and its 200 ms sampler cost **under ~20 mW** — the two banded blocks differ from
each other by more than either differs from unbanded, and an idle guest costs ~13 mW over an empty
host. Presenting, banded draws **+154 mW (+18%)**, spread across CPU (+20%) and GPU (+12%).

## The presenting cost is frames, not overhead

Same image, same policies, MangoHud over 20 s:

| arm | avg FPS | p50 | p90 |
|---|---|---|---|
| unbanded | 47.7, 49.6 | 17.43, 17.26 ms | 33.40, 32.68 |
| `rt+dyn` | 58.9, 59.8 | 16.69, 16.67 | 18.23, 18.05 |

+21% frames for +18% package power: energy per frame is flat to marginally better. The unbanded
p50/p90 pair — one vblank period, then two — is the missed-flip quantisation the whole
investigation is about, and it is gone under the band.

## Guest prerequisite, and a false negative it caused

`fpsrun.sh` needs **mangohud in the guest, and the F44 enhanced image does not ship it** (the only
implicit layer present is `VkLayer_MESA_device_select.json`). An in-block FPS pass reported
`NO LOG` twice and read as a parsing bug; what settled it was the user looking at the window and
seeing no HUD, which says the Vulkan layer never loaded and MangoHud never ran. `dnf install
mangohud` into the disposable clone fixed it. Two lessons, both already project rules: the human
watching the screen is an oracle no log replaces, and an empty result is a claim about the
harness until proven otherwise.
