# Performance re-measurement — 2026-08-08

Full pass, first since **2026-08-03** (which measured only the kernel-fence A/B) and the first
broad graphics re-baseline since **2026-07-26**. Host: M1 Max, 32 GB, macOS 26.5, otherwise idle.

**Vehicle** (identical envelope to every ledger row since 2026-07-26, so the series stays
comparable): CoW clone of `Fedora-Workstation-44.enhanced.raw`, booted through
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh` (EFI → GRUB → the guest's own kernel, SELinux
**enforcing**, coexist venus on KosmicKrisp), 4 vCPU / 4 GiB, window 1280x800, guest display
**pinned 1280x800 @ 1.0** (verified read-only before every run).

**Provenance.** Worker at `90fbe25`, linked against `third_party/virgl-prefix`
(`otool -L` verified — the software-2D trap). Guest: kernel `7.1.6-2-limina16k` (16 KiB pages),
mesa `26.1.5-7.limina.fc44`, `VN_PERF` unset. `glmark2` was `dnf`-installed during this run
(the image ships without it).

Raw rows in `perf/ledger.csv`; aquarium frames in `perf/evidence/aquarium-2026-08-08/`.

## TL;DR

- **The 2026-08-03 glmark2 regression is closed, and then some.** `glmark2-wayland-venus` is
  **2944** (median of 5, spread **±0.3%**) against 1613 on the fence-absent 7.1.4 arm and 1170 on
  the fence-present 7.1.6 arm. That is **+82%** over the best previous arm and **+45%** over
  2026-07-26. The historical run-to-run instability (±20% on 08-03, ±3% at best) is **gone**.
- **Both GL tiers now hold 60 fps on the WebGL aquarium through the entire historical range**
  (5k/10k/15k fish). 60 is the *pacing ceiling*, not throughput — so the sweep was extended to
  find the real limit: the shipped vrend path holds 60 all the way to **20 000 fish**.
- **The host wake budget collapsed.** Clean-fullscreen blobs at a *verified* 60 fps costs
  **~1 850 wakeups/s** on the shipped path against the ~8 100/s baseline of 2026-07-22
  (**−77%**), and ~3 400/s with GL forced back onto zink-on-venus (**−58%** like-for-like).
- **venus's Vulkan side moved too, on two independent instruments: `vkmark` 3151** (±0.15%)
  against 2140 for the same distro-packaged binary on 2026-07-28 (**+47%**), and
  **`vk-replay-venus-headless` 1974.7** against 1399 on 2026-07-26 (**+41%**). The latter row had
  gone missing and was recovered by baking `gfxrecon-replay` into the enhanced base.
- **One real regression found, and it is a memory leak, not a speed one.** The worker's
  `IOAccelerator (graphics)` allocations **ratchet ~9–12k regions / ~1.3 GB per workload
  open/close cycle and never return** — physical footprint 20.4 → 24.2 GB over three cycles.
  This is *not* the IOSurface scanout leak fixed on 08-07: that fix holds cleanly here. A scoping
  A/B puts it **on the vrend GL path only** — the shipped default — while zink-on-venus returns
  every byte.
- **`gl-replay-venus` is flat at ~47.6 fps.** The −18% versus the 57.6 reference of 2026-06-25
  remains open and accepted; nothing in this pass moved it.

## Ledger battery

All medians; repeats in parentheses.

| Workload | 2026-07-26 | 2026-08-03 (7.1.4, fence absent) | 2026-08-03 (7.1.6, fence present) | **2026-08-08** |
|---|---|---|---|---|
| `glmark2-wayland-venus` (512², score) | 2007–2056 | 1613 (n=5, ±3%) | 1170 (n=5, ±20%) | **2944** (n=5, ±0.3%) |
| `gl-replay-venus` (fps) | 47.29 | 44.39 | 45.84 | **47.60** (47.60/47.70/47.45) |
| `gl-replay-llvmpipe` (fps, CPU control) | 753 | 818 | 811 | **746** (745.9/746.7) |
| `vk-replay-venus-headless` (fps) | 1399 | — | — | **skipped** (see below) |
| `vkmark` (score, `-s 1280x720`) | — | — | — | **3151** (3146/3155/3151) |

`vkmark` was run to cover the gap left by the skipped `vk-replay` row — venus's claim on the
enhanced tier is now the *Vulkan* side, so a pass with no Vulkan number would not have been able
to say whether venus itself moved. Comparability care: the guest runs the **distro-packaged**
`vkmark-2025.01-3.fc44`, which matches the `vkmark-default-venus` row of 2026-07-28
(**2140**/2132), *not* the `vkmark-3scene-venus` rows (2674) that came from a differently-baked
binary. Against the like-for-like row that is **+47%**. Forcing the historical
`VN_PERF=no_fence_feedback` env that the old `ab-vkmark.sh` rows carried gives **3504** — worth
recording, but it is not a config we ship.

### The kernel-fence question is closed

`docs/perf/gsrs-local-rig.md` isolated the 7.1.6 blob-scanout flush fence as the sole cause of
the 08-03 drop (86.3% missed vblanks; arm C, the same tree with just that commit reverted,
landed back at the 7.1.4 frame clock). The fence was dropped in the **7.1.6-2** respin, which is
the kernel this guest runs. At 2944 the number is not merely recovered — it is far above the
fence-absent control. **Closed.**

### The +82% is real but deliberately unattributed

Four things argue it is not measurement error: the spread is ±0.3% across five runs, the CPU
control (`gl-replay-llvmpipe`) moved *down* 8% rather than up, `vkmark` moved +47% independently
on the Vulkan path, and a full-output run confirms the scene genuinely renders — correct
`512x512 windowed` surface config (no `buffer_scale` protocol error, the trap that produced
garbage scores on 2026-07-26), `zink Vulkan 1.4(Virtio-GPU Venus …)`, 3011 FPS at 0.332 ms
frametime across the full 3 s duration.
But several host-side changes landed together between 08-03 and 08-08 — the **KosmicKrisp MTL4
rebase** (Vulkan 1.4, 08-05; the guest now reports `zink Vulkan 1.4(Virtio-GPU Venus …)`), guest
**mesa 26.1.5-7**, and the **classic-gbm venus import** fixes (08-05/08-06). This pass does not
separate them; the MTL4 rebase is the leading candidate on size and timing alone. Naming one
without an A/B would repeat a mistake this project has already made twice.

### `vk-replay-venus-headless` — missing, then recovered (**1974.7 fps, +41%**)

`gfxrecon-replay` is not packaged for Fedora and was not baked into the enhanced base, so the
ledger script skipped the row — **silently**, which is how it went unnoticed until write-up time.
It was then built into `Fedora-Workstation-44.enhanced.raw` (upstream `765c3d6`) and the row
recovered:

| | 2026-06-25 | 2026-07-26 (F44) | **2026-08-08** |
|---|---|---|---|
| `vk-replay-venus-headless` (fps) | 1601 | 1399 | **1974.7** |

**+41% over the F44 baseline**, and it is worth more than its own number: it is a pure venus
Vulkan path with no GL and no compositor in it, so it corroborates the `vkmark` +47%
*independently of any GL measurement*. Two unrelated Vulkan instruments agreeing at +41/+47%
is what turns "glmark2 got faster" into "the venus/KK path got faster".

Two harness fixes came out of this, both committed: the ledger now **aborts** rather than
dropping the row (`LIMINA_PERF_SKIP_VK=1` to override deliberately), and the whole battery
— `glmark2`, `apitrace`, `vkmark`, `fio`, `gfxrecon-replay` — is baked into the enhanced base so
no pass has to install its own instruments (`docs/images.md` §Baked-in perf tooling).

### Base-image cross-check

The battery was re-run on the enhanced **base** image after the tooling bake, against the clone
the rest of this pass used:

| workload | clone | base | agreement |
|---|---|---|---|
| `gl-replay-venus` | 47.60 | 47.36 | 0.5% |
| `gl-replay-llvmpipe` | 746 | 745.3 | 0.1% |
| `glmark2-wayland-venus` | 2944 | 2893 | 1.7% |

So the headline results are a property of the stack, not of one clone or one boot.

## WebGL aquarium — on-display throughput

1024×1024 canvas, seated session, fps read from the supervisor's own frame capture (crops in
`perf/evidence/aquarium-2026-08-08/`). **`vrend-shipped` is the enhanced tier's default GL path
since the `drop-guest-zink` flip**; `zinkvenus` forces GL back through zink→venus for continuity
with the historical "venus" rows.

| numFish | 2026-07-26 virgl | 2026-07-26 venus | **08-08 vrend (shipped)** | **08-08 zink-on-venus** |
|---|---|---|---|---|
| 5 000 | 45 | 71 | **60** (capped) | **60** (capped) |
| 10 000 | 33 | 61 | **60** (capped) | **60** (capped) |
| 15 000 | 29 | 46 | **60** (capped) | **60** (capped) |
| 20 000 | — | — | **60** (capped) | 48 |
| 25 000 | — | — | 42 | 42 |
| 30 000 | — | — | 39 | 38 |

Two things must be said carefully here:

- **60 is the frame-pacing ceiling, not a throughput measurement.** Every cell reading 60 has
  unknown headroom above it. The 2026-07-26 venus column reads **71** at 5 000 fish precisely
  because present was *unpaced* in that era; now that presents are fence-accurate and vsync-paced,
  the counter cannot exceed the mode. So "71 → 60" is **not** a regression, and the two columns
  are not directly commensurable. This is why the sweep was pushed to 30 000.
- **Above the ceiling the two GL paths converge** (42/42 and 39/38 at 25k/30k) — that regime is
  GPU-bound, where the guest-side command-stream cost stops mattering. The one point that
  separates them is **20 000 fish: vrend 60 vs zink-on-venus 48**, consistent with the standing
  finding that venus's per-command toll scales with the command stream rather than frame size.

Against the like-for-like 2026-07-26 virgl column the improvement is large — 29 → ≥60 at 15 000
fish, i.e. **at least +107%**, and the true figure is higher because the ceiling hides it.

Worth noting for `docs/perf/aquarium-fps-instability.md`: that open issue recorded the dev Mac
ping-ponging **40–60 fps at 500 fish**. This pass held a rock-steady 60 up to 20 000 fish on the
same machine. The symptom did not reproduce.

## Runtime overhead — wakeups

Host worker wakeup rates via `spikes/wakeup-probe/procwake` (`ri_interrupt_wkups`). **Both loaded
arms were pixel-verified at 60 fps** by reading the blobs demo's own on-screen counter out of the
supervisor capture — a wakeup drop bought with a slower workload would be no win at all.

| state | 2026-07-22 baseline (6 vCPU) | **2026-08-08 (4 vCPU)** |
|---|---|---|
| idle | ~130/s | **~101/s** |
| blobs @60fps — shipped vrend GL | — | **~1 850/s** |
| blobs @60fps — zink-on-venus GL | ~8 100/s | **~3 400/s** |
| worker CPU under load | ~75–85% | 82–84% |
| supervisor CPU under load | ~7% | ~9.7% |

Guest-visible decomposition under load (10 s `/proc/interrupts` delta, shipped vrend arm; the
2026-07-21 6-vCPU baseline in parentheses): `arch_timer` **2 483/s** (~3 590), `IPI1` **3 859/s**
(~4 970), `virtio4` = virtio_gpu **259/s** (912 → 539 after libkrun 0091).

One caveat in the conservative direction: a GNOME "Critical Updates" toast was on screen during
both loaded arms. If it kept mutter off direct scanout, it inflated **both** arms equally, so the
−77% / −58% figures are if anything understated.

Reading this honestly:

- The envelope changed (4 vCPU here vs 6 in the baseline), but the 2026-07-22 vCPU A/B bounds
  that effect at **~800/s** (6→2 vCPU moved 8.1k→7.3k). The observed drop is ~6 200/s, so vCPU
  count explains at most a small fraction.
- The **like-for-like** comparison is the zink-on-venus arm, since the baseline ran GL on venus:
  8 100 → **3 400/s**, −58%. That is a genuine reduction in the venus ring's wake budget.
- The **shipped** path is better still (~1 850/s) because vrend does not use the venus ring for
  GL at all, so the ~5.9k/s `vkr_ring` poll-sleep budget that the M13 plateau lever exists to
  attack is simply **absent from the common desktop path**. That materially changes the priority
  of the M13 `(visible, power)` knob: it now only buys anything for Vulkan clients.
- Worker CPU (82–84%) sits at the top of the historical band despite far fewer wakeups. Wakeups
  and CPU are not the same axis and this pass did not attribute the CPU side.

## Memory — one clean result and one regression

Method: `vmmap --summary` on both processes across staged workload open/close cycles (aquarium
5 000 fish), settling 30 s after each close.

### The 08-07 supervisor scanout-leak fix holds

Supervisor `IOSurface` count at successive **closed** states: 21 → 28 → 29 → 28. Worker
`IOSurface`: 26 → 29 → 28 → 29. Both return to a stable plateau after every cycle — surfaces are
released when the guest lets go of them, exactly as `limina-owned-unmapped-leak` intends. No
creep.

### NEW: `IOAccelerator (graphics)` ratchets without bound

Worker `IOAccelerator (graphics)`, measured at the **closed** state of each cycle (i.e. with no
workload running at all). The first row is a **fresh cold boot with no workload ever run**,
measured after the reboot at the end of this pass — the control that makes the arithmetic exact:

| cycle (closed) | regions | size | worker physical footprint |
|---|---|---|---|
| **fresh boot, idle** | **3 851** | **600 M** | **4.3 G** |
| start (warmed specimen) | 129 666 | 13.7 G | 20.4 G |
| after 1 | 138 876 | 15.0 G | 21.6 G |
| after 2 | 147 679 | 15.9 G | 22.6 G |
| after 3 | 160 028 | 17.3 G | 24.2 G |

Closed-to-closed deltas: **+9 210, +8 803, +12 349 regions** — linear, no plateau. Within each
cycle the close returns only ~20–76 of the ~9 000–12 000 regions the open allocated. **A cache
fills once and levels off; this does not.**

The fresh-boot control settles it: an idle worker holds **3 851** regions / 600 MB. This specimen
ran ~15 firefox open/close cycles before the first snapshot, and 3 851 + 15 × ~9.3k ≈ 143k
against an observed 129 666 — the same linear per-cycle ratchet, running all session from a small
honest baseline.

Consequences worth stating plainly: the worker reached a **24.2 GB physical footprint against a
4 GiB guest** on a 32 GB host, with 9.1 GB swapped, having started the session at 4.3 GB — a
**34× growth in region count** driven purely by opening and closing a GL workload. `vmmap` itself
took minutes per snapshot at these region counts. On a long dogfood session this grows unbounded.

This is a **different fault** from the 08-07 IOSurface leak (which is clean above) and is not
bounded by the `LIMINA_GPU_MEM_BUDGET_MIB` cap (default 8192 MiB for a 4 GiB guest — the ledger
counts venus blob allocations, and this is already at 17.3 G). Per
`limina-owned-unmapped-leak`'s own sequel note, *the cap was a bound, not a fix*.

#### Scoping A/B: it is the vrend path, not a common layer

One further open/close cycle with GL forced onto **zink-on-venus**, same workload, same host,
same boot:

| | regions | size | footprint |
|---|---|---|---|
| closed (start) | 159 973 | 17.2 G | 24.0 G |
| open | 176 345 | 19.3 G | 26.4 G |
| **closed** | **159 911** | **17.1 G** | **24.0 G** |

**Everything comes back** — net −62 regions, footprint returns to 24.0 G exactly. So the ratchet
is **not** in KosmicKrisp/Metal or virglrenderer's core, both of which this arm exercises just as
hard. It is specific to the **vrend GL path** — which is precisely where the newest code lives
(the EGLImage-backed vrend scanout where vrend renders *into* the display IOSurface, and the
classic-gbm venus import work of 08-05/08-06).

That makes this the shipped-default path leaking while the non-default one is clean, which is the
worst way round. It is filed, not root-caused — root-causing is not this pass's job. **Repro
committed at `spikes/vrend-region-leak/`** (both arms plus the closed-to-closed reading rule).

## Breadth — boot, disk, memory floor

Dimensions with no recent numbers at all, measured once each so the pass is not graphics-only.

### Boot

Cold boot of the enhanced tier, wall-clock from process launch, guest display 1280x800:

| milestone | time |
|---|---|
| launch → ssh answering | **15.1 s** |
| launch → seated GNOME session (`wayland-0`) | **16.4 s** |
| guest-internal (`systemd-analyze`) | 9.46 s (207 ms kernel + 2.09 s initrd + 7.16 s userspace) |

So roughly **7 s of the 16.4 s is pre-kernel** — our GOP `KRUN_EFI` firmware plus GRUB, most of
it the GRUB countdown. The largest single guest-side unit is `plymouth-quit-wait.service` at
**3.07 s**, which is suspicious given `limina-plymouth-serial-console` records that our
`console=ttyAMA0` forces Plymouth into details mode; a splash that is not being shown should not
cost 3 s of a 16 s boot. Both are cheap, unexamined wins.

### Disk (virtio-blk)

`fio`, `--direct=1`, inside the guest on the btrfs root:

| pattern | result |
|---|---|
| sequential write, 1 MiB blocks, iodepth 8 | 10.0 GiB/s |
| sequential read, 1 MiB blocks, iodepth 8 | 7 699 MiB/s |
| random read, 4 KiB, iodepth 32 | 2 757 MiB/s, **~705k IOPS** |

⚠ **Read these as virtio-blk *path* numbers, not storage numbers.** `--direct=1` bypasses the
*guest* page cache but not the host's, and the 1 GiB file sits entirely in host cache on an APFS
NVMe — nothing here touched the device. What they legitimately show is that the virtqueue,
`imago` backend, and 16 KiB-page guest driver sustain ~10 GB/s and ~700k IOPS, i.e. the block
path is nowhere near being a bottleneck. A real storage measurement needs a working set larger
than host RAM.

### Memory floor

A freshly booted, idle 4 GiB guest costs the worker a **4.3 GB physical footprint** — roughly the
guest RAM allocation plus ~300 MB. That is the honest floor, and it is a good one. Note that
after the disk leg the footprint had risen to 5.9 GB purely from guest page-cache high-water
(guest `buff/cache` 1 287 MB), which never returns to the host — documented behaviour
(`limina-mem-overhead`), not a leak, and quite separate from the graphics ratchet above (whose
region count stayed flat at 3 958 across the whole disk leg, confirming that fault is specific to
GL rendering).

## Follow-ups

1. **Root-cause the `IOAccelerator (graphics)` ratchet.** The highest-value item here by a wide
   margin — unbounded growth in the shipped configuration, already scoped to the vrend GL path
   (see the A/B above), which narrows the search to the recent EGLImage-scanout / gbm-import
   work. A RED-first L2 guard in the shape of the existing `venus_fd_census` /
   `testcomp` region-count tests is the natural vehicle.
2. ~~Bake `gfxrecon-replay` into the enhanced image and make the ledger fail loudly.~~ **DONE
   2026-08-08** — the full battery is baked into the base and the ledger aborts on a missing
   `vk-replay`; the row is recovered at 1974.7 fps. The aquarium default sweep also now runs to
   30 000 fish, since 5k/10k/15k no longer clears the vsync ceiling.
3. **Attribute the +82% glmark2 gain** if it is worth knowing — the cheap discriminator is a KK
   A/B across the MTL4 rebase, the same shape as the 2026-07-26 KK exoneration.
4. **Retire or re-scope `docs/perf/aquarium-fps-instability.md`** — the symptom did not reproduce
   on the machine that reported it.
5. **Re-judge the M13 `(visible, power)` relax-plateau lever.** Its target — the `vkr_ring`
   poll-sleep budget — is absent from the shipped GL path now. It remains relevant for Vulkan
   clients only.
6. **Cheap boot wins:** ~7 s of a 16.4 s boot is pre-kernel (GRUB countdown), and
   `plymouth-quit-wait` costs 3.07 s for a splash we force into details mode anyway.
7. **A real storage measurement** with a working set larger than host RAM — this pass only
   established that the virtio-blk path is not the limit.
