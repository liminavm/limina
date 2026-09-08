# Performance re-measurement — 2026-09-08

First pass since the renderer became **virglrs** (the Rust rewrite), measured at limina
`b095253d` / virglrs `42008bb`. Vehicle: `cp -c` clone of `Fedora-Workstation-44.enhanced.raw`
through `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`, 4 vCPU / 4 GiB, display verified pinned
`Virtual-1 1280x800 scale=1.0`. Guest `7.1.8-limina16k.4`, mesa `26.1.8-11.limina.fc44`,
`VN_PERF` unset.

**This pass is incomplete.** What was measured is below; what was not is in *Not measured*. The
graphics comparison against 2026-08-08 also spans a renderer replacement, a KosmicKrisp bump, a
guest mesa bump and a kernel bump, so nothing here is attributed to any one of them.

## TL;DR

- **The ledger battery is stable and `gl-replay-venus` is up 20% on 08-08** — 56.9 fps against
  47.60, recovering the 57.6 of 2026-06-25 that the 08-08 memo left open.
- **vkmark 3382**, +7% on 08-08's 3151. This leg was impossible before today: it was the
  reproducer for the sampler-view poison (`spikes/virglrs-samplerview-poison/`), fixed in virglrs
  `42008bb` during this pass.
- **`glmark2-wayland-venus` is down 23%** — 2268 against 2944. Unattributed.
- **The WebGL aquarium is down roughly 3–12x and that is the headline.** 5 000 fish reads **18
  fps** where 08-08 read 60. Reproduced on a clean profile, and **not** caused by the multisample
  mitigation (A/B below).
- **The 08-08 `IOAccelerator (graphics)` ratchet was not re-tested** — the measurement was void
  and is owed.

## Ledger battery (n=3, medians)

| workload | 08-08 | 08-27 arm A | **09-08** | vs 08-08 |
|---|---|---|---|---|
| `gl-replay-venus` (fps) | 47.60 | 49.25 | **56.91** (56.91/56.91/57.39) | **+20%** |
| `gl-replay-llvmpipe` (CPU control) | 746 | 743 | **722.3** (710.8/722.3/728.6) | −3% |
| `vk-replay-venus-headless` (fps) | 1974.7 | 1898.9 | **1742.1** (1679/1742/1853) | −12% |
| `glmark2-wayland-venus` (score) | 2944 | 2829 | **2268** (2261/2268/2277) | −23% |

Rows in `perf/ledger.csv` under `virglrs 42008bb full battery run N`. The spread is tight on every
row except `vk-replay`, which has always been the noisiest (±5%).

The CPU control moved −3%, so a few points of every graphics number may be the host rather than
the stack.

## vkmark (n=3)

**3382** (3382 / 3136 / 3418) against 08-08's **3151** — +7%, like-for-like on the distro binary.
Zero `refused: vrend` across all three runs, which is also the repeat-verification of the poison
fix.

Do **not** compare this to the 3903 and 3404 recorded earlier in the day: those were taken with
the compositor already poisoned, i.e. with no compositor load competing at all.

## WebGL aquarium — a large regression, cause not identified

1024×1024 canvas, seated session, fps read from the supervisor's frame capture. Crops in
`perf/evidence/2026-09-08/aquarium-vrend/`.

| numFish | 08-08 vrend (shipped) | **09-08 vrend (shipped)** |
|---|---|---|
| 5 000 | 60 (vsync ceiling) | **17** (18 on a re-run) |
| 10 000 | 60 (ceiling) | **10** |
| 15 000 | 60 (ceiling) | **7** |
| 20 000 | 60 (ceiling) | **5** |
| 25 000 | 42 | **4** |
| 30 000 | 39 | **3** |

08-08's low counts were pinned at the 60 fps vsync ceiling with unknown headroom, so the true
ratio at 5 000–20 000 is *at least* what the table shows. The clean comparisons are the two counts
where 08-08 was already below its ceiling: **42 → 4** at 25 000 and **39 → 3** at 30 000, both
about a factor of ten.

**Verified, not assumed:**

- **Not a stale capture.** The supervisor wrote a frame about once a second throughout the sweep
  (a dump needs a presented frame; 274 of 289 gaps in that window were ≤2 s), and each crop shows
  its own fish count.
- **Not a software fallback.** `GL_RENDERER` in the guest is
  `virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))` — the shipped vrend path.
- **Not host contention.** One VM on the machine, load average 1.86.
- **Not a degraded Firefox.** Firefox crashed later in the session and its restored tab reported
  WebGL disabled, which would have explained everything — so the sweep was repeated on a **clean
  profile**: 5 000 fish read **18** against the original **17**.
- **Not the multisample mitigation.** `775da3e0` capped `VREND_MAX_SAMPLES=1` on 2026-09-06, after
  the 08-08 baseline, and it targets exactly WebGL. A boot with `VREND_MAX_SAMPLES=4` (ceiling
  confirmed in the worker log) read **18 fps at 5 000 fish — identical**. The mitigation costs
  nothing on this workload and is exonerated.
- **No GPU device loss.** No fault, no `DEVICE_LOST`, no Mesa error in the worker log across the
  sweep; the only match is a benign zink copy-box perf warning.

### Four candidates tested and eliminated

Each is a single-variable A/B on one build, ceiling-free rows, same guest:

| candidate | test | result |
|---|---|---|
| the 09-06 multisample cap | `VREND_MAX_SAMPLES=4` | 18 fps vs 18 — **no effect** |
| virglrs's classic-fence `glFinish` | `VIRGLRS_FENCE_FINISH=0` | 25k: 4 vs 4; 30k: 4 vs 3 — **no effect** |
| virglrs `42008bb` (the sampler-view cure) | boot at pre-cure `34ed41d` | 25k: **4 — identical**, and **zero poison** |
| Firefox itself | version in the guest | 150.0, installed 2026-04-22 — **unchanged since the baseline** |

The pre-cure leg is doubly informative. It came back at the same 4 fps *and never poisoned*, which
means Firefox's WebGL canvas does not take the minted-`glTextureView` route at all — so the cure
was never in this path, in either direction. That also matches the shape of the commit: it only
ever moved routes from *refused* to *working*, never from cheap to expensive.

### It is not vrend's GL either

Same session, one variable:

| GL path | `GL_RENDERER` | glmark2 |
|---|---|---|
| vrend (shipped default) | `virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))` | **4687** |
| zink → venus (ledger env) | `zink Vulkan 1.4(Virtio-GPU Venus …)` | 2157 |

**vrend's windowed GL is more than twice the venus path** and far above the ledger row. So the
regression is not vrend throughput, not its present path, and not compositing in general — it is
specific to Firefox's WebGL on top of it.

### What is left

The KosmicKrisp bump (`fe305ee4`) and guest mesa `26.1.5-7 → 26.1.8-11`. There is independent
evidence the host driver moved: a timestamp probe run against KosmicKrisp with no renderer in the
path reports `timestampPeriod` 41.666668 and resolves two stamps either side of a 16 MiB fill to
the same tick, which is not the driver the 08-08 numbers were taken on.

### The command stream is not where the tenfold is

The virglrs session replayed a 1 GB recording of this exact workload against both renderers on the
host with no VM (`harness/replay/vrend-replay.sh`, n=5 each):

| renderer | median |
|---|---|
| C virglrenderer | ~2.12 s |
| virglrs | ~2.61 s |
| virglrs, `VIRGLRS_FENCE_FINISH=0` | ~2.59 s |

**virglrs is ~23% slower than the C on the command stream — not tenfold**, and the fence finish is
worth nothing there either, corroborating the in-VM A/B above from a second direction.

So the tenfold lives in what a VM-free replay cannot exercise: **presentation, scanout, the
IOSurface a compositor samples, and the fence/present cadence around them.** The command side is
eliminated; the present path is what is left, alongside the KosmicKrisp and guest mesa drift.

The remaining experiment is the same sweep against a C-renderer bundle **in the VM** — the replay
holds the guest constant by removing it, which is exactly why it cannot see a present-path
difference. Assigned to the virglrs session, which holds a C bundle. Not run here.

**Trap, if anyone times that replay script:** it runs `cargo build --release` for the Rust prefix,
so a single timed run measures the build the first time and the replay every time after, and the
output does not say which you got. It produced a spurious 12x before a repeat disagreed with it.
Warm it first, or discard run one.

Note the shape: `gl-replay-venus` is *up* 20% and `glmark2` down 23%, while the aquarium is down
several-fold. Whatever this is, it hurts a composited on-display browser workload far more than a
direct GL one, which is the case that matters most for a desktop.

## Not measured

- **Aquarium `zinkvenus` arm.** Firefox does not launch under the zink environment in the
  benchmark unit — reproduced three times across two boots, always an idle-desktop capture. Whether
  that is a harness defect or a real failure of Firefox to get a GL context on zink→venus is itself
  unanswered, and worth answering: it would be a tier-2 bug.
- **`IOAccelerator (graphics)` closed-to-closed ratchet.** The open/close cycle silently ran with
  no Firefox at all, so the identical before/after readings (401.7 MiB, 1402 regions) measure
  nothing. **The 08-08 regression is neither confirmed nor cleared.**
- **Host wakeups**, **boot**, **disk (fio)**, **memory floor**.

## Harness defects found

- **`aquarium-run.sh` cannot run two arms in one invocation.** The second `systemd-run` fails with
  `Unit ff-bench.service was already loaded`, and the script continues, capturing a stale frame
  instead of aborting. Every affected crop had an identical `bright_frac` — the tell, and the only
  reason this was caught.
- **A capture with no counter in it is reported as a successful measurement.** The script writes
  and announces the crop either way; only reading the PNG reveals an empty desktop. It should
  refuse a crop whose fps region does not parse.
- **`pkill -f firefox` self-matches** the ssh command carrying that string — the 08-08 memo
  recorded this for `pgrep` and it bit again here through `pkill`. Use `-x`.
- A Firefox crash dialog blocks every subsequent launch, silently voiding arms. A human noticing
  the dialog on screen is what recovered the session.

## Follow-ups

1. **Attribute the aquarium regression** — same sweep against the pre-virglrs host commit on this
   guest. Highest value in this memo.
2. **Re-measure the `IOAccelerator` ratchet** with a verified-running workload.
3. Fix the three `aquarium-run.sh` defects above before the next pass; two of them fail silently.
4. Finish the owed legs: zinkvenus arm, wakeups, boot, disk, memory floor.
