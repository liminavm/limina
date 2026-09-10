# Remeasure at the parked-classic-present pin, and the Basemark baseline

**Subject:** virglrs `4bfdefe` → `4654a34` and libkrun `0dba6b9c` → `bae5de4a`, on limina `9fc8eea4`.
Classic vrend scanouts now present by parking on a fence instead of blocking the GPU worker.
Evidence in `perf/evidence/2026-09-10/`. HVF suite **138/138** at this tree.

## Two arms, because the enhanced guest cannot see this change

The ledger's four workloads run the **enhanced** guest — venus/zink. The change is to **classic
vrend** scanouts, which is the **stock** guest's path. So the enhanced arm is continuity, and the
stock-guest Basemark arm is the one that exercises the diff. The probe confirms it:
`renderer=virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))`, not llvmpipe.

## Enhanced arm: four workloads flat, vkmark down

n=5, quiet host, guest settled, display **verified** 1280x800 @ 1.0, guest `7.1.8-limina16k.4` /
mesa `26.1.8-11`, debug worker with the dev `opt-level = 3` overrides.

| workload | this pin | `4bfdefe` (09-09b) | verdict |
|---|---|---|---|
| `gl-replay-venus` | 56.92 – 56.98 | 56.88 – 57.27 | overlap |
| `gl-replay-llvmpipe` (control) | 721.1 – 726.5 | 720.4 – 735.5 | overlap |
| `vk-replay-venus-headless` | 2118 – 2206 | 2111 – 2268 | overlap |
| `glmark2-wayland-venus` | 2831 – 3021 | 2901 – 3118 | overlap |
| `aquarium-25000-vrend` | 43 / 45 / 52 | 44 / 45 / 50 | overlap |
| `aquarium-30000-vrend` | 39 / 42 / 43 | 37 / 39 / 39 | overlap |
| **`vkmark-default-venus`** | **3998 – 4086** (2 boots) | **4212 – 4366** (2 boots) | **DOWN ~4-7%** |

**vkmark does not overlap and it reproduces across two boots**, while every coarse workload is
flat. That is the expected signature of a small real cost: vkmark is the only fine instrument here
([[limina-perf-instruments]]), and the four ledger workloads average it away. It is **not
attributed** — the window also contains the `4bfdefe → 4654a34` virglrs delta, which includes two
classic-fence correctness fixes, one of which makes a classic fence sync *every* sub-context of its
context plus ctx0 rather than whichever happened to be current. More syncing per fence is a
plausible cost, and it is a lead, not a finding. Its ctx0 half is priced in
`2026-09-10-ctx0.md`: skipping the ctx0 sync moves vkmark by under 2%, so it is not the cause. The
every-sub-context half is not priced, but it cannot bite a context holding a single
sub-context — for one, the coverage fix's only delta *is* the ctx0 sync. So if the compositor's
contexts each hold one sub-context (unverified), the drop is park-present's own price.
**Do not test that with `LIMINA_FENCE_PRESENT=0`:** it also disables venus blob parking, which
predates this change, and would over-attribute. The clean vehicle is a build — libkrun at
`0dba6b9c` with current virglrs — not a knob.

## Stock arm: the Basemark baseline

**First Basemark rows in this ledger — a baseline, not a comparison.** Vehicle is virglrs's
`harness/vm/client-basemark.sh` over Marionette, called in place (limina does not copy it).
Graphics suite (`suite=2`) asserted from the console configuration block, not from the URL.
Gecko 150.0, display resolution **1280x800 read off the result page**. Two boots; the suite runs
twice per boot and the **second** is scored.

| test | boot 1 | boot 2 | between-boot spread |
|---|---|---|---|
| WebGL 1.0.2 | 3316.65 | 2985.92 | **11%** |
| WebGL 2.0 | 3871.44 | 3717.99 | 4% |
| Shader Pipeline | 1520.00 | 1680.77 | **10.6%** |
| Draw-call Stress | 80.34 | 79.55 | 1% |
| Geometry Stress | 1725.34 | 1736.05 | 0.6% |
| Canvas | 1174.18 | 1167.58 | 0.6% |
| SVG | 982.11 | 986.93 | 0.5% |

**Set per-test bands, never one band.** The spread is not uniform and not close to uniform:
WebGL 1.0.2 and Shader Pipeline move ~11% boot to boot while Geometry, Canvas, SVG and Draw-call
move under 1%. A 5% move on Geometry Stress would be a result; the same move on WebGL 1.0.2 is
noise.

**Warm-up is large and one-directional here:** WebGL 1.0.2 read 2004.92 on the first suite run of
boot 1 and 3316.65 on the second, +65%. Scoring the first run of a session measures warm-up.

**Do not compare these to the virglrs session's rig numbers.** Those were taken at 2560x1440;
these are 1280x800. Different resolution is a different workload, so the two sets are not
commensurable in either direction.

**This arm cannot answer the WebGL 2.0 question it was partly run for.** The virglrs session sees
WebGL 2.0 ~10% down against its own previous build. There is no Basemark row at the previous pin
in this vehicle, so nothing here confirms or refutes that. Settling it needs a Basemark run at
`4bfdefe`/`0dba6b9c`.

## Not measured

**Nothing here measures latency through the new shared FIFO waiter.** The fence waiter is now one
thread carrying both ordinary fences and presents, at a measured 976 fences/s under texture churn,
so a present can queue behind unrelated work. Every instrument in this battery scores
**throughput**, which absorbs a late present as a small average cost while the visible symptom is
judder. Basemark separates the frame-paced and texture-churn *cost* regimes — genuinely more than
the aquarium does — but separating cost regimes is not measuring tail latency.

**So a clean result here is not evidence the stutter mode is absent.** The instrument for it does
not exist yet; the shape is park-request to fence-retire for a present-ring fence, reported as a
distribution rather than a mean, and virglrs has booked it.

## Method

**The llvmpipe control gates readability, and a quiet-looking host does not.** Four runs were
**discarded** before any row was kept: the control read 660-675 against its 717-735 band while
Activity Monitor and screen sharing were running on the host. With those stopped it returned to
733 and every workload returned to band. `VIRGLRS_SUBMIT_STATS` was ruled out as the cause by a
same-tree A/B first — it changed nothing.

**A freshly booted guest is not a settled guest.** `gnome-software`, `dnf5daemon` and
`flatpak-system-helper` run for the first minutes and depress every number. Wait for uptime ≥ 3-4
minutes *and* those processes quiet; a load average read at 0-1 minutes is measured before the
update machinery starts, not after it finishes.
