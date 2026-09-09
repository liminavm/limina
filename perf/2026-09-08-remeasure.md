# Performance re-measurement — 2026-09-08

First pass since the renderer became **virglrs** (the Rust rewrite). Two pins were measured, both
on a `cp -c` clone of `Fedora-Workstation-44.enhanced.raw` booted through
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`, 4 vCPU / 4 GiB, display verified pinned
`Virtual-1 1280x800 scale=1.0`, guest `7.1.8-limina16k.4`, mesa `26.1.8-11.limina.fc44`, Firefox
150.0, `VN_PERF` unset:

- **profile fix** — limina `d27bc7cc` / virglrs `c299aae`
- **fence fix** — limina `095e2856` / virglrs `4afa3ef` / libkrun `7c4ada05`

**This pass is incomplete** — see *Not measured*.

## TL;DR

- **The renderer was being compiled `-O0`.** virglrs is a path dependency built into the debug
  worker, so every rig that boots `cargo xtask build` ran an unoptimized renderer — not slow the
  way debug code is usually slow, but slow *per guest command*. `d27bc7cc` adds
  `[profile.dev.package.virglrs] opt-level = 3`. **This dominates every graphics number below**,
  and it is the single most important finding of the day.
- **A second cost sat behind it: a `glFinish` of every context on every classic fence.** Worth a
  further 1.8x on the aquarium. Fixed in virglrs `4afa3ef`, which takes a `glFenceSync` on the
  context that queued the work and waits it off the worker thread.
- **Three of the four ledger workloads are at or above the 08-08 C-renderer baseline**, and vkmark
  is **+26%**. The ledger is flat across the fence fix, which is the correct answer: none of those
  four workloads depend on the classic fence path.
- **The mouse-pointer stutter is NOT fixed** by either change, and it is the symptom that motivated
  the fence work. Its cause is a *third* drain — `resource_sync_iosurface`, on the compositor's
  present. Not measured here; not yet fixed.
- A regression this memo previously reported as "`glmark2` −23%, unattributed" **was the build
  profile**. It is now +5% on the baseline.

## Ledger battery (n=3, medians)

| workload | 08-08 (C) | virglrs `-O0` | virglrs `-O3` | **`-O3` + fence fix** | vs 08-08 |
|---|---|---|---|---|---|
| `gl-replay-venus` (fps) | 47.60 | 56.91 | 56.91 | **56.94** | **+20%** |
| `gl-replay-llvmpipe` (CPU control) | 746 | 722.3 | 717.5 | **717.4** | −4% |
| `vk-replay-venus-headless` (fps) | 1974.7 | 1742.1 | 2224.3 | **2229.5** | **+13%** |
| `glmark2-wayland-venus` (score) | 2944 | 2268 | 3099 | **3160** | **+7%** |
| **vkmark** | 3151 | 3382 | 3981 | not re-run | **+26%** |

The `-O0` rows are kept and labelled in `ledger.csv`, because a trend file that silently drops a
bad measurement teaches nothing.

**The fence fix moves nothing here, and that is the point of running it** — flat is the prediction,
and what the battery buys is the absence of a regression hiding behind the aquarium's win.
`gl-replay-venus` agrees to within 0.16 fps across three runs (56.87 / 56.94 / 57.03), the tightest
agreement in this pass. `glmark2`'s +2% median sits inside its own spread (3103 / 3160 / 3177) and
is not a gain. The llvmpipe control has one low run (663.9 against 717 and 725) with an unmoved
median — a shared host, not a signal.

Two things the `-O0` → `-O3` step moved that are worth separating:

- **`vk-replay` gained 28% (1742 → 2224).** That is a pure venus path — no GL, no compositor — so
  the `-O0` renderer was costing the **venus** side too, not just vrend. Anyone reasoning about
  this as a vrend-only problem would be wrong.
- **`gl-replay-venus` did not move at all: 56.9 on every pin**, across nine runs. It runs
  `eglretrace --headless` (`perf-ledger.sh:120`), so it **never presents** — no page-flip, no
  scanout, nothing reaching `resource_sync_iosurface`. That makes it a clean control *for
  presentation*. It is a poor control for renderer cost: it runs zink→venus, and `vk-replay` — also
  venus — gained 28% from `-O3`, so its flatness is not a venus property. Something neither
  renderer optimization nor presentation touches binds it, most likely guest-side CPU in zink's
  GL→Vulkan translation.

The llvmpipe CPU control is down 4% across the day, so a few points of every graphics number are
the host rather than the stack.

## WebGL aquarium

1024×1024 canvas, seated session, fps read from the supervisor's frame capture. Crops in
`perf/evidence/2026-09-08/`. **Every cell here is a single capture, not n=3.**

| numFish | 08-08 (C) | virglrs `-O0` | virglrs `-O3` | `-O3`, fence drain off (knob) | **`-O3` + fence fix** |
|---|---|---|---|---|---|
| 25 000 | 42 | 4 | 19–20 | 35 | **36** |
| 30 000 | 39 | 3 | 19 | 34 | **29** |

**Flatness across fish counts is not a signal.** This memo previously read the flat 20 @ 25k /
19 @ 30k as evidence of a drain-bound workload. With the drain removed the pair reads 35 / 34 —
just as flat — and the C-era baseline was 42 / 39. It never discriminated anything and no argument
here rests on it.

Two costs, stacked, both ours, both now fixed:

1. **The renderer was compiled `-O0`** — worth 5–6x.
2. **A `glFinish` of every context on every classic fence** — worth a further **1.8x** at 25 000.

**The 30 000 cell is not attributed.** 29 against the knob arm's 34 would say the shipping fix
costs something the knob did not — plausibly its `glFenceSync` + `glFlush` forcing a batch submit
the bare drain-removal got for free — but both are single captures and the observed run-to-run
spread is at least that wide. It needs n=3 on both arms before anyone writes that story down.

**The residual against the C era is ~1.2x and remains unattributed.** It is a *subtraction, not a
measurement*: 36 against a 42 taken on a different host driver and a different guest mesa, both
single captures. It should not be quoted as "KosmicKrisp and mesa drift" until something scores it.

### The positive control for the fence fix

The variable was verified to have moved before any number was read: no `no context for the fence
waiter` in the worker log, the `virglrs-glwait` thread present, and `create_fence` → `finish_all`
absent from an idle sample and down to **7 samples under load** from **8827 (75% of the `gpu
worker` thread)** on the stock optimized build. On the stock build `submit_cmd` was 23% and
`sync_iosurface` → `finish_all` 0.1%; with the fence drain gone `submit_cmd` rises to 91% and
waiting primitives total ~200 of 11121 samples — **the thread does work rather than waiting, so the
GPU was never saturated and the wait was needless.**

Note the knob's meaning changed with the fix: **`VIRGLRS_FENCE_FINISH=1` now forces the old inline
finish back**, and is read once through a `OnceLock` at the first fence
(`third_party/virglrs/src/vrend/vrend.rs:628`), so it is fixed for the worker's life and cannot be
toggled within a boot.

### The pointer stutter is a separate defect, and it is still open

With the aquarium loaded the desktop pointer **still stutters as badly as before** — human-scored,
on the fence fix, with `finish_all` down to 7 samples. This is the symptom the fence work was aimed
at, and the fps win did not touch it.

It is not the single `gpu worker` thread's head-of-line blocking. One epoll loop on that thread
dispatches both the control and cursor queues, so a cursor event genuinely cannot be picked up
mid-batch — but the C renderer ran on that same thread with that same loop and did not stutter, so
head-of-line cannot be what changed.

The cause is a third drain, in the renderer: `resource_sync_iosurface`
(`third_party/virglrs/src/vrend/vrend.rs:659`) calls `finish_all()` — every context, every
sub-context — and it sits on `flush_resource`, which runs on the compositor's present. **Every
desktop repaint drains the aquarium's context.** That is how 36 fps at 25 000 coexists with a
pointer that has not improved: fps and pointer latency pay different tolls. The C finishes one or
two contexts there, never all of them. The fix is virglrs's and is not in any pin measured here.

That site read 0.1% and was dismissed in writing on the stock build, because the fence path drained
everything first; removing the fence drain promoted it from 12 samples to 460. **A cost measured
behind a larger cost is not measured** — the same trap as the `-O0` A/Bs below, from the other
side. Whoever profiles this next will be one drain further along and should distrust whatever is
then at 0.1%.

The stutter has no counter. Scoring it needs a booted VM and a human watching the pointer.

### Correctness is unscored, not passed

Every run here was on GNOME-on-GL, which masks the unordered-consumer hazard the fence guards
completely. No flashing and no cross-tab contents were seen, and that is **weak evidence, not a
pass**. The hazard wants a consumer as unordered as a Vulkan queue; virglrs has since written that
leg (a CPU read of an IOSurface through `IOSurfaceLock` after a GL render, built to fail if it
cannot make the needle move) and it had not been run when this was written.

### Every A/B below `-O3` is suspect, including four of mine

**An A/B taken on the `-O0` build measured the build profile, not its variable.** With the
unoptimized renderer as the bottleneck, a real effect behind it reads as a null — which is exactly
what happened to the fence: 4 vs 4 and 4 vs 3 at `-O0`, against 19 → 35 at `-O3`. Nothing about
such a result looks stale.

| elimination | build | status |
|---|---|---|
| classic-fence `glFinish` | `-O0` | **FALSIFIED** — it is 1.8x |
| the 09-06 multisample cap (`VREND_MAX_SAMPLES=4`) | `-O0` | **suspect, re-run owed — with a positive control** |
| the sampler-view cure (pre-cure `34ed41d`) | `-O0` | **suspect as a timing result** |
| vrend vs zink→venus glmark2 (4687 / 2157) | `-O0` + fence off | **suspect** — an unshipped config |
| Firefox's version (150.0, April) | n/a | stands — not a timing measurement |
| the command stream (VM-free replay, C ~2.12 s vs virglrs ~2.61 s) | `--release` | stands |

**A null needs a positive control to mean anything.** "No cost" is only concludable from a
measurement that *could* have shown one, so re-running the multisample cap as a bare timing A/B on
the fast build would buy a second null and more confidence in it, which is worse than the first.
What made the fence A/B trustworthy was `finish_all` visibly collapsing in the profile. The re-run
needs the equivalent.

The pre-cure leg's **zero poison** stands regardless — that is a behavioural observation, not a
timing one, and it still shows Firefox's canvas never takes the minted-`glTextureView` route.

**Ledger rows are not affected beyond today's.** The virglrs-era rows in `ledger.csv` are only
2026-09-08; everything from 08-09 and 08-27 predates the switch and ran on the C renderer. The
`-O0` rows are labelled in place.

## Not measured

- **The aquarium at n=3.** Every cell is one capture. A first 25 000 reading of 33 was discarded
  because the capture showed the GNOME Activities overview open — a different workload, and a side
  effect of asking a human to exercise the pointer during a measurement run. Ask for the
  interaction, or take the number; not both on one run.
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

**`third_party/virglrenderer` in this tree is stale and is not the reference.** limina stopped
pinning the C when virglrs took ownership of it (`third_party/manifest.toml`), and this checkout
predates virglrs's pin — it still shows the pre-fix `vrend_renderer_resource_sync_iosurface` that
finishes ctx0 only. Read the C through `third_party/virglrs/third_party/virglrenderer`, which is
the pinned one.

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

1. **Fix `resource_sync_iosurface`** to finish what the present actually needs, then score the
   pointer with a human watching. Owned by virglrs.
2. **The aquarium at n=3** on 25 000 and 30 000, on the pin that ships, before the 30 000 gap or
   the ~1.2x residual is attributed to anything.
3. **Re-measure the `IOAccelerator` ratchet** with a verified-running workload.
4. **Re-run the multisample cap** with a positive control.
5. Answer the zinkvenus launch failure as its own question.
6. Fix the three `aquarium-run.sh` defects; two fail silently.
7. Finish the owed legs: wakeups, boot, disk, memory floor.
