# Performance re-measurement — 2026-09-08

First pass since the renderer became **virglrs** (the Rust rewrite), measured at limina `d27bc7cc`
/ virglrs `c299aae`. Vehicle: `cp -c` clone of `Fedora-Workstation-44.enhanced.raw` through
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`, 4 vCPU / 4 GiB, display verified pinned
`Virtual-1 1280x800 scale=1.0`. Guest `7.1.8-limina16k.4`, mesa `26.1.8-11.limina.fc44`, Firefox
150.0, `VN_PERF` unset.

**This pass is incomplete** — see *Not measured*.

## TL;DR

- **The renderer was being compiled `-O0`.** virglrs is a path dependency built into the debug
  worker, so every rig that boots `cargo xtask build` ran an unoptimized renderer — not slow the
  way debug code is usually slow, but slow *per guest command*. `d27bc7cc` adds
  `[profile.dev.package.virglrs] opt-level = 3`. **This dominates every graphics number below**,
  and it is the single most important finding of the day.
- **Three of the four ledger workloads are now at or above the 08-08 C-renderer baseline**, and
  vkmark is **+26%**.
- **The WebGL aquarium is still ~2x short** of the baseline at the ceiling-free counts. That
  residual is real and unexplained; the virglrs session is profiling it.
- A regression this memo previously reported as "`glmark2` −23%, unattributed" **was the build
  profile**. It is now +5% on the baseline.

## Ledger battery (n=3, medians)

| workload | 08-08 (C) | virglrs `-O0` | **virglrs `-O3`** | vs 08-08 |
|---|---|---|---|---|
| `gl-replay-venus` (fps) | 47.60 | 56.91 | **56.91** | **+20%** |
| `gl-replay-llvmpipe` (CPU control) | 746 | 722.3 | **717.5** | −4% |
| `vk-replay-venus-headless` (fps) | 1974.7 | 1742.1 | **2224.3** | **+13%** |
| `glmark2-wayland-venus` (score) | 2944 | 2268 | **3099** | **+5%** |
| **vkmark** | 3151 | 3382 | **3981** | **+26%** |

Rows under `virglrs c299aae optimized dev profile run N`. The `-O0` rows are kept and labelled in
`ledger.csv`, because a trend file that silently drops a bad measurement teaches nothing.

Two things the optimization moved that are worth separating:

- **`vk-replay` gained 28% (1742 → 2224).** That is a pure venus path — no GL, no compositor — so
  the `-O0` renderer was costing the **venus** side too, not just vrend. Anyone reasoning about
  this as a vrend-only problem would be wrong.
- **`gl-replay-venus` did not move at all: 56.91 both ways**, to three decimals across six runs.
  It runs `eglretrace --headless` (`perf-ledger.sh:120`), so it **never presents** — no page-flip,
  no scanout, nothing reaching `resource_sync_iosurface`. That makes it a clean control *for
  presentation*. It is a poor control for renderer cost: it runs zink→venus, and `vk-replay` — also
  venus — gained 28% from `-O3`, so its flatness is not a venus property. Something neither
  renderer optimization nor presentation touches binds it, most likely guest-side CPU in zink's
  GL→Vulkan translation.

The llvmpipe CPU control is down 4% across the day, so a few points of every graphics number are
the host rather than the stack.

## WebGL aquarium — the fence drain

1024×1024 canvas, seated session, fps read from the supervisor's frame capture. Crops in
`perf/evidence/2026-09-08/`. **Every cell here is a single capture, not n=3** — the 1.8x below is
solid at that granularity, the individual numbers are not.

**Flatness across fish counts is not a signal.** This memo previously read the flat 20 @ 25k /
19 @ 30k as evidence of a drain-bound workload. With the drain removed the pair reads 35 / 34 —
just as flat — and the C-era baseline was 42 / 39, also fairly flat. It never discriminated
anything and no argument here rests on it.

| numFish | 08-08 (C) | virglrs `-O0` | virglrs `-O3` | **`-O3`, `VIRGLRS_FENCE_FINISH=0`** |
|---|---|---|---|---|
| 25 000 | 42 | 4 | 19–20 | **35** |
| 30 000 | 39 | 3 | 19 | — |

Two costs, stacked, and both are ours:

1. **The renderer was compiled `-O0`** — worth 5–6x.
2. **A `glFinish` of every context on every classic fence** — worth a further **1.8x**
   (19 → 35 at 25 000, measured by the virglrs session on the optimized build).

That leaves **~1.2x unattributed** against the C-era 42. That figure is a *subtraction, not a
measurement*: 35 against a 42 taken on a different host driver and a different guest mesa, both
single captures. Nothing has scored it, and it should not be quoted as "KosmicKrisp and mesa
drift" until something does.

It will also need recomputing rather than inheriting. `VIRGLRS_FENCE_FINISH=0` is not the fix, and
a `glFenceSync` that actually waits is not free — the shipping number will land somewhere between
19 and 35, and the residual is whatever remains against *that*.

The `create_fence` → `finish_all` path is **75%** of the `gpu worker` thread's samples on the
stock optimized build (11838 samples; `submit_cmd` 23%, `sync_iosurface` → `finish_all` 0.1%).
With the finish off it falls to 0.2% and `submit_cmd` rises to 91%, with waiting primitives
totalling ~200 of 11121 samples — **the thread does work rather than waiting elsewhere, so the
GPU was never saturated and the wait was needless.**

`VIRGLRS_FENCE_FINISH=0` is **not shippable**: the finish exists because Metal orders no queue
against another, so an early fence hands a venus compositor a buffer whose renders have not run.
The knob is the measurement; the fix is the `glFenceSync` the code comment already names, taken
where the work is queued and waited on without holding the renderer.

### Serialization is a separate defect from throughput

One `gpu worker` thread services virtio-gpu commands for **every** context and holds the renderer
lock while parked in `glFinish`. Cursor updates queue behind an aquarium frame drain, which is why
loading the aquarium slows the guest's **mouse pointer** — a human-visible latency and fairness
defect that no throughput number here would reveal.

### Every A/B below `-O3` is suspect, including four of mine

**An A/B taken on the `-O0` build measured the build profile, not its variable.** With the
unoptimized renderer as the bottleneck, a real effect behind it reads as a null — which is exactly
what happened to the fence: 4 vs 4 and 4 vs 3 at `-O0`, against 19 → 35 at `-O3`. Nothing about
such a result looks stale.

| elimination | build | status |
|---|---|---|
| classic-fence `glFinish` (`VIRGLRS_FENCE_FINISH=0`) | `-O0` | **FALSIFIED** — it is 1.8x |
| the 09-06 multisample cap (`VREND_MAX_SAMPLES=4`) | `-O0` | **suspect, re-run owed — with a positive control** |
| the sampler-view cure (pre-cure `34ed41d`) | `-O0` | **suspect as a timing result** |
| vrend vs zink→venus glmark2 (4687 / 2157) | `-O0` + fence off | **suspect** — an unshipped config |
| Firefox's version (150.0, April) | n/a | stands — not a timing measurement |
| the command stream (VM-free replay, C ~2.12 s vs virglrs ~2.61 s) | `--release` | stands |

**A null needs a positive control to mean anything.** "No cost" is only concludable from a
measurement that *could* have shown one, so re-running the multisample cap as a bare timing A/B on
the fast build would buy a second null and more confidence in it, which is worse than the first.
What made the fence A/B trustworthy was `finish_all` going 8827 → 30 in the profile — the variable
was visibly moved. The re-run needs the equivalent.

The pre-cure leg's **zero poison** stands regardless — that is a behavioural observation, not a
timing one, and it still shows Firefox's canvas never takes the minted-`glTextureView` route.

**Ledger rows are not affected beyond today's.** The virglrs-era rows in `ledger.csv` are only
2026-09-08; everything from 08-09 and 08-27 predates the switch and ran on the C renderer. The
`-O0` rows are labelled in place.

## Not measured

- **Aquarium `zinkvenus` arm.** Firefox does not launch under the zink environment in the benchmark
  unit — three failures across two boots, always an idle-desktop capture. Whether that is a harness
  defect or a real failure to get a GL context on zink→venus is **unanswered**, and it would be a
  tier-2 bug. Probe it with `eglinfo` under exactly that unit's environment, before Firefox is
  involved.
- **`IOAccelerator (graphics)` closed-to-closed ratchet.** The open/close cycle ran with no Firefox
  at all, so the identical readings measure nothing. **The 08-08 regression is neither confirmed nor
  cleared.**
- **Host wakeups**, **boot**, **disk (fio)**, **memory floor**.
- The 08-08 aquarium counts below 25 000 sat at the 60 fps vsync ceiling and are not a measurement;
  only the two ceiling-free counts are compared here.

## Traps found

**The build profile is the big one.** A renderer compiled into the worker inherits the worker's
profile, and nothing in any output says so. It cost this pass a full re-measurement and produced a
confident, wrong "tenfold regression" write-up first.

Reaching for `target/release/limina-vmm` to check a perf number fails twice over: it is
**symbol-stripped** (useless to profile, and it wastes a boot silently), and it **loses the
hypervisor entitlement** — `cargo xtask app` signs only the in-bundle copy, so a bare release
worker fails with `build_microvm: Internal(Vm(VmSetup(VmCreate)))`, which names nothing about
codesign and reads exactly like a VM bug. Re-run `cargo xtask sign --release`.

**A timed wrapper that builds on first use** measures the build once and the work every time
after, saying nothing about which you got. This produced a spurious 12x before a repeat disagreed
with it.

**`pgrep -f '[l]imina-vmm'` selects the SUPERVISOR, not the worker** — the supervisor's argv
carries `--vmm-bin target/debug/limina-vmm` and the disk path, and it sorts first. Use
`pgrep -f '[l]imina-vmm --cpus'`. Anything built on the first shape — `ps eww` env checks, `sample`,
`vmmap` — has been inspecting the parent. Better still, **prefer a process printing its own
configuration** (`vrend max_samples ceiling = 4` in the worker log) over any external inspection
of it.

`aquarium-run.sh` has three defects, two silent:

- It **cannot run two arms per invocation** — the second `systemd-run` fails on a lingering
  `ff-bench.service` and the script *continues*, capturing a stale frame. One fish count per
  invocation; clear the unit between.
- **A capture with no fps counter in it is reported as a successful measurement.** The tell is an
  identical `bright_frac` across arms.
- `pkill -f firefox` **self-matches** the ssh command carrying that string. Use `-x`.

A Firefox crash dialog blocks every later launch and voids arms silently; a human noticing it on
screen is what recovered this session.

## Follow-ups

1. **Reconcile 20 vs 23 fps at 25 000** between this rig and the virglrs session's before treating
   the residual as a single quantity.
2. **Profile the residual** — in progress on the virglrs side, looking at whether the renderer
   thread is blocked or compute-bound. `gl-replay-venus` at 56.91 across six runs is a control: a
   workload that does not move between `-O0` and `-O3` is not spending its time in the renderer.
3. **Re-measure the `IOAccelerator` ratchet** with a verified-running workload.
4. Answer the zinkvenus launch failure as its own question.
5. Fix the three `aquarium-run.sh` defects; two fail silently.
6. Finish the owed legs: wakeups, boot, disk, memory floor.
