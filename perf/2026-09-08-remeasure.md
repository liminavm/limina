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

## WebGL aquarium — the residual

1024×1024 canvas, seated session, fps read from the supervisor's frame capture. Crops in
`perf/evidence/2026-09-08/`.

| numFish | 08-08 (C) | virglrs `-O0` | **virglrs `-O3`** |
|---|---|---|---|
| 25 000 | 42 | 4 | **20** |
| 30 000 | 39 | 3 | **19** |

The build profile was **5–6x** of the gap. A **~2x residual** remains and is not explained.

Two cautions on these numbers. Each is a single instantaneous read of the page's own counter from
one scanout capture, not an average over a window. And 20 at 25 000 against 19 at 30 000 is
suspiciously flat for a 20% workload increase — either the workload is not GPU-bound at these
counts on this build, or the counter is read before it settles. The virglrs session measures 23 at
25 000 in the same nominal configuration; 15% apart is more than "where the counter is read"
comfortably explains, and the two should be reconciled before anyone declares the residual closed.

### Candidates eliminated for the residual

Each a single-variable A/B, ceiling-free rows, same guest and build:

| candidate | test | result |
|---|---|---|
| the 09-06 multisample cap | `VREND_MAX_SAMPLES=4` | no effect |
| virglrs's classic-fence `glFinish` | `VIRGLRS_FENCE_FINISH=0` | no effect, in the VM **and** VM-free — but see below |
| virglrs `42008bb` (the sampler-view cure) | boot at pre-cure `34ed41d` | no effect, **and zero poison** |
| Firefox itself | version in the guest | 150.0, installed 2026-04-22, unchanged |
| the command stream | VM-free replay of this workload, both renderers, n=5 | C ~2.12 s vs virglrs ~2.61 s — **23%, not tenfold** |

The pre-cure leg is doubly informative: same fps *and no poison*, so Firefox's WebGL canvas never
takes the minted-`glTextureView` route, and the cure was never in this path in either direction.

The VM-free replay (the virglrs session's `harness/replay/vrend-replay.sh`) is what places the
residual **outside the command stream** — in presentation, scanout, the IOSurface a compositor
samples, and the cadence around them, which a replay structurally cannot exercise.

**It is also not vrend's GL.** Same session, one variable: glmark2 scores **4687** on the shipped
vrend path against **2157** on zink→venus. vrend's windowed GL is more than twice the venus path.

### The fence A/B eliminates one site, not the idea

`VIRGLRS_FENCE_FINISH` gates only `finish_classic_for_fence` (`renderer.rs:1102`).
`resource_sync_iosurface` calls the same `finish_all()` **unconditionally**
(`vrend/vrend.rs:564`), and `finish_all` walks every context and sub-context doing
`make_current` + `glFinish` on each, with the renderer's single mutex held.

So the A/B above measures that the *fence* site costs nothing and leaves the
`flush_resource` → `sync_iosurface` page-flip site **untested**. The serialization
hypothesis therefore survives it: every virtio-gpu command from every context — cursor
updates included — queues behind a drain of the aquarium's GPU-bound frame once per
page-flip.

Both rigs now agree at 25 000 fish — **20 fps here, 19 on the virglrs session's**, same method and
same verified-pinned display — so the residual is **~2.2x**, not the 1.7x quoted from an earlier
reading that did not reproduce.

The supporting observation is not from this rig: loading the aquarium to 25 000 fish slows
down *everything else in the guest, including the mouse pointer*, which is a serialization
signature rather than a cost. This rig's `iosurface scanout: 1280x800 B8G8R8X8_UNORM …
renders land in the surface directly` confirms the scanout is a vrend resource here too, so
`ctx_id == 0` holds and the path is live on these measurements. **"The guest is on venus" is
true of Vulkan and not of the scanout.**

That also reframes the flat 20 @ 25k vs 19 @ 30k: if presents serialize behind a full drain,
fps stops tracking fish count and starts tracking the drain. Those two rows may be evidence
rather than an artefact.

**The measurement that would settle it** is the same treatment the fence site already has — an
env gate around the `finish_all()` in `resource_sync_iosurface`, turning the hypothesis into a
one-boot A/B on an unchanged build. Not a shipping configuration; a knob.

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
