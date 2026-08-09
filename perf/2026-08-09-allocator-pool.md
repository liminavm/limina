# Allocator pool: A/B against the pre-pool baseline (2026-08-09)

The structural fix for the `IOAccelerator (graphics)` ratchet (kk `34a207e0405`, shared
type-segregated budget-retired command-allocator pool — see `spikes/vrend-region-leak/`) measured
against the commit immediately before it (`d1ce8f3fe7f`).

**Method.** One disk clone, used for both arms in sequence so guest state is identical; VM
rebooted between arms so the worker process (and therefore its VM regions) starts fresh. Display
pinned at **1280x800 scale 1.0** and `--verify`'d on each boot — an unpinned `match-host` boot
picks a fractional scale and invalidates the whole run (`perf/README.md`). 4 vCPU, 4096 MiB.
`scripts/perf/aquarium-run.sh`, extended fish sweep.

## Throughput — neutral

| fish | baseline | pool B=4 |
|---|---|---|
| 20 000 | 58 | 58 |
| 25 000 | 49 | 47 |
| 30 000 | 40 | 39 |

Identical at 20k, −2 and −1 at the GPU-bound tiers. `perf/README.md` puts run-to-run variance on
this workload at ±10%, so a 4% and 2.5% delta on single runs is **inside the noise floor** — this
says "no measurable cost", NOT "measurably 1 fps slower". Distinguishing those would need repeated
runs, which is not worth it unless something else motivates it.

5k/10k/15k are omitted deliberately: they sit at the **vsync ceiling (60)**, which hides arbitrary
headroom and cannot rank anything. Per `perf/README.md`, never read a 60 as throughput.

Evidence: `perf/evidence/aquarium-baseline/`, `perf/evidence/aquarium-pool-b4/` (frame crops).

## Memory — the reason the change exists

Worker state at the end of the identical sweep:

| | baseline | pool B=4 | |
|---|---|---|---|
| `IOAccelerator (graphics)` regions | **154 134** | **18 707** | −88% |
| `IOAccelerator (graphics)` bytes | **17.1 G** | **2.4 G** | −86% |
| physical footprint | 22.9 G | 8.4 G | −63% |
| **swapped** | **10.6 G** | **0 K** | — |

The swap row is the one that matters for how this feels: the baseline arm pushed 10.6 GB out to
swap running a browser benchmark, and the pooled arm swapped nothing at all. That is a
user-visible difference the fps numbers cannot show, and on a 32 GB host it is the difference
between "a VM is running" and "the machine is thrashing".


## Full battery vs a SAME-DAY pre-pool control (n=3 each)

Medians of three `scripts/perf-ledger.sh` runs per arm, same clone, same session, display pinned
and `--verify`'d on every boot.

| workload | pre-pool baseline | pool B=4 | delta | 2026-08-08 ledger | baseline vs 08-08 |
|---|---|---|---|---|---|
| `gl-replay-venus` (fps) | 44.4 | 44.3 | **−0.3%** | 47.6 | −6.7% |
| `gl-replay-llvmpipe` (fps, CPU control) | 750.8 | 747.2 | −0.5% | 746.0 | +0.6% |
| `vk-replay-venus-headless` (fps) | 1995.3 | 1962.6 | **−1.6%** | 1974.7 | +1.0% |
| `glmark2-wayland-venus` (score) | 2812 | 2808 | **−0.1%** | 2944 | −4.5% |

**The pool is throughput-neutral on every workload**, with the CPU control moving as much as the
graphics rows.

### ⚠ The same-day control caught a wrong attribution

Compared only against the **2026-08-08 ledger rows**, this run looks like a 4.5–6.7% regression on
the two GL-on-venus workloads. It is not the pool: **the pre-pool build measured today shows the
same drop** (44.4 vs 47.6; 2812 vs 2944). Whatever moved, moved before the pool and is unrelated
to it.

Note also why the CPU control did not catch this: `gl-replay-llvmpipe` is flat across all three
dates (746 → 750.8 → 747.2). "The control is flat, therefore the change is not environmental" is
an **unsound** inference — the shift affects the venus path and not the CPU path, so only an
arm-matched same-day control could separate them. This is the second time in this investigation
that comparing against historical numbers instead of a contemporaneous control nearly produced a
false attribution.

**Open, and NOT attributed here:** GL-on-venus is down ~5–7% between 2026-08-08 and 2026-08-09 in
a build with no pool. Candidate worth checking first: kk `b03e3bf4973` onward compiles the
allocator instrument in, so `mtl_begin_command_buffer` now calls `limina_kk_alloc_stats_on()` on
every begin even when the env gate is off. That is a cheap cached read but a non-inlined
cross-call on the hottest path. Untested.

### An instrumented boot is not a shipping configuration

The first battery run of the day was booted with `LIMINA_KK_ALLOC_STATS=4000` still set, so the
pending-tracker (mutex per begin/commit/completion, `allocatedSize` per reset) was live. Those
four rows are marked INVALID in `perf/ledger.csv` rather than deleted. Amusingly they read
*higher* than the clean run — which is how the confound was caught to be irrelevant rather than
flattering, and is a reminder that a single run's noise exceeds the instrument's cost.

## venus tier

| | | |
|---|---|---|
| aquarium 20k fish | **50** | (historical zink-on-venus reference: 48) |
| aquarium 30k fish | **38** | (reference: 38) |
| `IOAccelerator (graphics)` | **504.8 M / 2 572 regions** | |
| physical footprint | 5.5 G | |

venus throughput is unchanged, and its memory under the pool is an order of magnitude below the
vrend tier's (2 572 regions vs 18 707) — expected, since guest zink owns the VkDevice and tears
the whole thing down at workload exit, which the vrend path never does.


## Complete battery — every instrument we have

Same clone, same session, display pinned. `vkmark` n=3 (medians); the rest single runs.

| instrument | pre-pool | pool B=4 | delta | 08-08 | pre-pool vs 08-08 |
|---|---|---|---|---|---|
| `vkmark` (score, 1280x720) | 2969 | 2941 | −0.9% | 3151 | **−5.8%** |
| `glmark2-display` venus (800x600) | 2853 | 2823 | −1.1% | — | — |
| `glmark2-display` vrend (800x600) | 3918 | 3885 | −0.8% | — | — |
| aquarium vrend 25k | — | 48 | — | — | — |
| aquarium venus 25k | — | 40 | — | — | — |

Every pool-vs-control delta is ≤1.6% across **seven** instruments now. The pool is neutral.

### The 08-08 → 08-09 drop has a shape

`vkmark` measured on the **pre-pool** build today is 2969 against 3151 on 08-08 — the same ~6%
drop as the two GL-on-venus workloads, in a build with no pool in it. Collecting the pattern:

| path | 08-08 → 08-09 (pre-pool) |
|---|---|
| `gl-replay-venus` (Xwayland, composited) | −6.7% |
| `glmark2-wayland-venus` (composited) | −4.5% |
| `vkmark` (composited) | −5.8% |
| **`vk-replay-venus-headless` (no compositor)** | **+1.0%** |
| `gl-replay-llvmpipe` (CPU control) | +0.6% |

**Everything that goes through the compositor is down ~5–7%; the headless path and the CPU control
are flat.** That points at the present/scanout path rather than at command submission, and it
rules the pool out twice over — once because pre-pool shows it, once because the pool's cost would
land on submission, not on present. Still unattributed, and now worth its own bisect.

### ⚠ Discarded: the software-2D aquarium row

The software tier was run and **thrown away**. Its `sw-5000` crop reads `Number of Fish 30000` —
the label from the *previous* venus run. `LIBGL_ALWAYS_SOFTWARE=1` never got a context (the same
run's `glmark2` software arm produced no output at all), so Firefox never repainted and the
capture still held a stale venus frame. Had the crop not carried the fish count, "software-2D
does 39 fps at 5k" would have gone into the ledger as a real number. The pixel *is* the evidence,
and it contradicted its own label.

## Verdict

Throughput-neutral within noise, 86% less GPU address space, no swapping. Confirmed across the full battery against a same-day
control: −0.3% / −1.6% / −0.1% with a control at −0.5%. The budget ladder (B ∈ {2, 8, 16}) was
**not** run: it was queued to find the memory/perf trade-off, and with B=4 showing no measurable
perf cost there is no trade-off left to explore — B=4 already converges
(`spikes/vrend-region-leak/data/allocstats-pool.txt`). Revisit only if a workload appears where
the pool population misbehaves.

## Not covered


- **Aquarium arms are single runs**; only the ledger battery is n=3.
- **The 08-08 → 08-09 drop on composited paths is unattributed** and predates the pool. It
  deserves its own bisect; the shape above (composited down, headless flat) is the lead.
- **software-2D tier is unmeasured** — the run produced a stale capture and was discarded.
- **The per-allocator fixed overhead** of running ~261 live allocators at B=4 was not isolated;
  the footprint number bounds it in aggregate but does not attribute it.
