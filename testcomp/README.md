<!-- SPDX-License-Identifier: GPL-3.0-only -->

# limina-testcomp — a small Wayland compositor we fully control

A deliberately small but **realistic** Wayland compositor for limina's guest, built to
reproduce host-side behaviours on demand: scanout churn, buffer lifetime, dmabuf import,
mode changes. It exists because the alternative — reproducing through a full desktop —
costs a GDM login, a seat fight, and a compositor whose behaviour we can only observe.

It is not a desktop. There is no shell, no window management, no animation, no D-Bus, no
settings. Clients get mapped and composited; that is the whole product. Everything it
*does* do, it does the way a real compositor does — real Vulkan allocation, real dmabuf
import, real KMS page flips — because a vehicle that fakes the path under test is worth
less than no vehicle at all.

## Licensing — read this before moving code in or out

**This directory is `GPL-3.0-only`**, and the rest of the limina repository is
`GPL-2.0-only WITH LicenseRef-limina-exception`. Those two are incompatible *in
combination*: GPL-3-only code cannot become part of a GPL-2-only work.

That is fine here, and the reason it is fine is structural, so do not erode it:

- This builds a **standalone aarch64 Linux executable that runs inside the guest**. limina
  is a macOS host application. They are separate programs that never link.
- The test harness *invokes* it over SSH, exactly as it invokes the Python probes. Mere
  aggregation, not combination.
- Its dependencies (smithay, wayland-rs, ash, drm-rs) are MIT/Apache, so nothing pulls it
  toward GPL-2 either.

**The rule this implies:** never share a *library crate* with limina's own guest code
(`guest/limina-agent` and friends, GPL-2-only). Cargo workspace membership is not linking
and would be fine; a shared crate is exactly the thing that breaks it. If this ever needs a
protocol type from limina, copy or re-specify it rather than depending on it.

Code lifted from **synoik** (`GPL-3.0-only`, the same author) is welcome here for the same
reason, and is noted per file where it happens.

## Why it is shaped the way it is

**smithay handles the Wayland protocol; we handle the GPU and the display.** Smithay's
`DrmCompositor` would want a `Renderer`/`Frame` implementation for our Vulkan renderer —
in synoik that pair is ~6k lines, and it buys us nothing we need, because we already have
a working KMS present path. So the split is:

| layer | who | why |
|---|---|---|
| `wl_compositor`, `xdg_shell`, `linux-dmabuf` | smithay | protocol correctness is not what we are testing, and getting it wrong would only add noise |
| Vulkan device, dmabuf import, compositing | ours | this is the path under test |
| Scanout allocation + KMS page flip | ours | ditto, and the reason this exists |

**Scanout buffers are allocated in Vulkan directly, never through gbm.** gbm only yields
venus blobs when the loader resolves to a venus-backed driver, which limina stopped
configuring by default on 2026-08-04 (`MESA_LOADER_DRIVER_OVERRIDE` flipped to
`virtio_gpu`, after which gbm hands out classic virgl resources and vkr refuses them:
`invalid res_id`). Allocating in venus removes that dependency and takes limina's
`SET_SCANOUT_BLOB` zero-copy present path. The sequence is in synoik's
`scanout-buffers-via-vulkan.md`; the constraints that bite are `DRM_FORMAT_MOD_LINEAR`
only, and taking `rowPitch` from `vkGetImageSubresourceLayout` rather than computing it.

## Milestones

It is built bottom-up, and each step is gated on evidence rather than on compiling:

| M | scope | gate |
|---|---|---|
| **1** ✅ | KMS + Vulkan scanout allocation + churn. No Wayland, no input. | **Reproduces `kmschurn.py`'s `churn-vk` numbers on the same host.** Until the two vehicles agree, a difference could as easily be a transcription bug as a finding. Passed 2026-08-07 — see below. |
| **2** ✅ | Wayland frontend (smithay), `wl_shm` clients | a real client's pixels reach the scanout. Passed 2026-08-07 — see below. |
| **3a** ✅ | `linux-dmabuf` import (venus-allocated client) | a client's dmabuf pixels reach the scanout **and** the host log shows the IOSurface import branch. Passed 2026-08-07 — see below. |
| **3b** ✅ | teardown paths × holders | discriminates between the paths in `buffer-lifetime-matrix.md` §4, against an oracle shown to move (+41) under a deliberate leak. Passed 2026-08-07. |
| 3c | vrend-allocated (gbm) client buffers | reaches the **asymmetric** holder of matrix §3, the observed compositor-quit shape. Needs the `vkr_budget_set_context` fix in matrix §6 first. |

### M3a's gate has two halves, and the pixels are the weaker one

The host resolves a cross-context import down a **silent fallback ladder** — IOSurface, then
`map_ptr`, then cross-context SHM bytes (`vkr_device_memory.c:320-345`). If the IOSurface lookup
fails, the import still succeeds and the pixels are still correct; what does *not* happen is the
borrowed `+1` on `mem->imported_iosurface` (`:794`). Since that reference is the entire subject of
M3b, a green pixel capture on its own would certify a vehicle that arms nothing.

So M3a must show **both**:

1. the client's quadrants on the scanout (`iosdump`), and
2. a host worker line proving the IOSurface branch fired — `limina: import res N <- host-pointer
   (IOSurface id=M ...)` with a **nonzero** id, or `[LIMINA-VKR-MTLTEX] import res N IOSurface
   id=M`.

**Oracle trap, cost one boot cycle:** those lines go through `vkr_log`, which is
`VIRGL_LOG_LEVEL_INFO` (`vkr_common.c:261`) and therefore **invisible at the default
`RUST_LOG=warn`**. Their absence at `warn` says nothing at all. Boot with
`RUST_LOG="warn,krun_rutabaga_gfx::virgl_renderer=info"`, and note that the budget lines *are*
visible at `warn` (they are `vkr_log_error`), which makes the log look instrumented when the
import path is not.

### M3a passed both halves (2026-08-07)

```
sudo -n env VK_DRIVER_FILES=…/virtio_icd.aarch64.json ./limina-testcomp run &
sudo -n env … WAYLAND_DISPLAY=wayland-1 ./limina-testcomp client-dmabuf 40
```

**Mechanism**, from the host worker log — the half that certifies the holder is armed:

```
virgl: vkr: limina: import res 227 <- host-pointer (IOSurface id=78 base=0x12ea98000 size=491520)
```

The **nonzero** `IOSurface id` is the whole point: it means `vkr_mtl_iosurface_lookup` succeeded
and the borrowed `+1` is parked in `mem->imported_iosurface`, rather than the import having
quietly landed one rung down the fallback ladder. `size=491520` is 400×300×4 rounded up to the
16 KiB guest page, which is the arithmetic agreeing too.

**Pixels** (`spikes/venus-churn-retention/m3a-dmabuf-scanout.png`, cropped from the 2560x1440
scanout): the four quadrants at (0,0), sampled and exact —

| sample | got | expected |
|---|---|---|
| (100,75) TL | (255,0,0) | red |
| (300,75) TR | (0,255,0) | green |
| (100,225) BL | (0,0,255) | blue |
| (300,225) BR | (255,255,0) | yellow |
| (1200,700) | (26,26,64) | backdrop |

Row-major and unswapped, so the import's stride and format survived the round trip. The client
ran **1792 frames in 40 s**, which is the dmabuf release path and the frame callbacks both
working — a compositor that never released would stall it after one frame. `imported=1` then
`evicted` on client exit, `import_failures=0`.

`iosprobe` reported `nonzeroRGB=0` for this very surface while `iosdump` read `nonzero=3686400`
— the M2 trap, unchanged: enumerate ids with `iosprobe`, judge content only with `iosdump`.

### M3b: the teardown paths, and what they actually showed (2026-08-07)

`./teardown-matrix.sh` (client at 1920x1080, so a retained buffer is 8.3 MiB rather than 0.5 MiB
and clears the `owned unmapped` noise floor). Three paths, all with the compositor holding a live
import:

| path | `destroying context` line | objects left | `owned unmapped` | census |
|---|---|---|---|---|
| 1 — client clean exit | `instance was gone` | 0 | 0 | +0 alive |
| 2 — client SIGKILL, buffer committed+unreleased | **`with a valid instance`**, then `instance was gone` | 0 | 0 | +0 alive |
| 2b — **compositor** SIGKILL holding a live import | **`with a valid instance`**, then `instance was gone` | 0 | 0 | +0 alive |

**Path 2 settles the matrix correction empirically.** `destroying context 4 (limina-testcomp)
with a valid instance` is `vkr_context.c:1006` firing — a `SIGKILL`ed client leaves
`ctx->instance` set, so the full sweep runs down to `vkr_device_memory_release` and drops the
borrowed `+1`. The matrix predicted the opposite (bare-`free()`, leak). It is the *clean* exit
that shows `instance was gone` with nothing to sweep. Source said so; the log agrees.

Path 2b is the one that matters most, and it is the one the original plan would have missed: the
`+1` is held by the **importer**, so killing clients only ever tests exporter-side death. Killing
the compositor with a populated import cache is the shape of the observed compositor-quit
residual — and here it came back clean, with the census returning to `iosurface N/N (+0)`.

### M3b detects the failure it is meant to detect (2026-08-07)

The table above would have been worthless on its own — a green result and a blind oracle are the
same observation. So the import cache was made to leak on purpose (`run --leak-imports`: never
evict, never sweep at exit) and driven with `client-churn 40 1920x1080`, forty **distinct**
dmabufs so retention accumulates instead of staying at one:

| arm | alive IOSurfaces, before → after | evictions |
|---|---|---|
| GREEN — shipped behaviour | 2 → **2** | 42 |
| RED — `--leak-imports` | 2 → **43** | 0 |

+41 retained against a flat baseline. The vehicle detects this failure class.

**Getting there cost three wrong oracles, and the wrong ones all read "clean" in both arms** —
the invariance smell, exactly as `CLAUDE.md` describes it:

| oracle | GREEN | RED | verdict |
|---|---|---|---|
| `owned unmapped` bytes | row **absent** | row **absent** | M1's oracle. Not this one — and absent parses as an empty string that looks like a failed read, not as zero. |
| `vmmap` `IOSurface` row | 364.2M → 351.9M | 364.2M → 351.9M | virtual size, dominated by other mappings |
| census `iosurface A/F (+N)` | +1 | +1 | counts registry refs, not deallocation |
| census `DEALLOC iosurface N (alive M)` | 2 → 2 | 2 → **43** | **the oracle** |

The fourth is object-exact — it counts real IOSurface `-dealloc` — so one retained surface is
visible regardless of byte noise, which also removes the need for large buffers.

**And the census is a sampler, not a gauge.** It ticks on the *allocation* path
(`vkr_budget.c:425`) every `LIMINA_GPU_MEM_BUDGET_CENSUS` seconds, so a quiesced worker never
emits a fresh line and `tail -1` silently hands back a **stale** one — which reads as "nothing
changed" and is how the first two RED runs came back green. `teardown-matrix.sh` forces a tick
(wait out the interval, then run a throwaway client so the compositor allocates again); after a
path that kills the *compositor*, it has to start a fresh one first, or nothing allocates and the
final read is empty rather than zero.

With that oracle, the three teardown paths above re-ran and all returned to baseline (2 → 2).
That result now means something.

### M2's gate: a real client's pixels on the scanout (2026-08-07)

`limina-testcomp run` on the guest console, `limina-testcomp client` against it, and the
oracle is the host reading the scanout IOSurface — not the compositor's own logs:

```
sudo -n env VK_DRIVER_FILES=…/virtio_icd.aarch64.json ./limina-testcomp run &
sudo -n env XDG_RUNTIME_DIR=/tmp/testcomp-run WAYLAND_DISPLAY=wayland-1 \
    ./limina-testcomp client 15
# host, with LIMINA_GLOBAL_SCANOUT=1 in the boot env:
spikes/venus-draw-probe/iosdump <id>
```

The capture shows the client's four quadrants — red, green, blue, yellow in row-major
order, so no channel swap — composited at (0,0) over the backdrop, on a 2560x1440 venus
scanout. The client ran **853 frames in 15 s** (~57 fps, vsync-paced), which is the frame
callback loop and the immediate buffer release both working: a client that never gets its
buffer back stalls after two frames, and one whose callbacks go unanswered stops at one.
`toplevel_destroyed` fired on exit, the compositor survived, and it accepted a second client
afterwards.

**Two oracle traps here, both nearly producing a false "nothing rendered":**
- **`iosprobe` reports `nonzeroRGB=0` for surfaces `iosdump` reads as fully painted.** It
  does not take the GPU-coherent lock that `iosdump` does, so it sees stale CPU-side
  content. Use `iosprobe` to *enumerate* ids, never to judge content.
- **The worker log's `iosurface scanout:` lines are the vrend/EGL path.** The compositor
  presents venus blobs through `SET_SCANOUT_BLOB`, which logs nothing at that level — an
  absence of new lines after the compositor starts is not an absence of scanouts.

### M1 detects the failure it is meant to detect (2026-08-07)

Matching `kmschurn` on a healthy host shows the transcription is faithful; it does **not**
satisfy the rule at the bottom of this file, which is a different and higher bar. So M1 was
also run against the known bug, by lifting `SURFACE_STORE_CAP` (`crates/limina/src/window/present.rs`)
to 100000 — the shape the supervisor had before limina `8e00d94`:

| `SURFACE_STORE_CAP` | before | after | growth |
|---|---|---|---|
| 100000 (the bug) | 30.2 M | 4.2 G | **+4.17 GiB** |
| 32 (shipped) | 47.4 M | 408.7 M | +361 MiB |

4.17 GiB is 300 × 14.7 MiB: one whole retained framebuffer per frame, exactly. An 11.5×
separation, so the vehicle detects this failure class rather than merely agreeing on green.

**Trap when re-running that A/B:** restoring the source file with `mv present.rs.orig
present.rs` restores its *mtime* too, and cargo then skips the rebuild and leaves the
cap-lifted binary in place — the GREEN half would silently re-measure the RED build. `touch`
the file and confirm the "Compiling limina" line before booting.

### M1's gate, and the trap in measuring it (2026-08-07)

Both vehicles, 300 buffers each at 2560x1440, against the same host build; the oracle is
`owned unmapped` bytes on the worker (`scanout_churn_retention.rs` explains why bytes and
not regions):

| boot | order | vehicle | before | after | growth |
|---|---|---|---|---|---|
| A | 1st | `kmschurn.py churn-vk` | 72.4 M | 408.7 M | **+336 MiB** |
| A | 2nd | `limina-testcomp churn` | 408.7 M | 408.7 M | +0 |
| B | 1st | `limina-testcomp churn` | 45.1 M | 409.6 M | **+364 MiB** |
| B | 2nd | `kmschurn.py churn-vk` | 409.6 M | 409.6 M | +0 |

The resting values agree to within 1 MiB, which is the comparison that means something: the
resting number *is* `SurfaceStore` holding its 32 surfaces, so both vehicles drive the host
allocator the same way. Gate passed.

**The trap, which cost a re-run:** whichever vehicle goes *second* measures **+0 growth**, and
it is not a result. The store rests at its cap, so a second churn evicts and replaces rather
than accumulating — the host-side `[SCANOUT-LEDGER]` line shows the fresh binds happening
(194 → 449 fresh) while the byte count sits still. **Each arm needs its own boot**, or the
run reads as "my vehicle allocates nothing" when the truth is "the cache was already full".
Enable the ledger to tell those apart:
`RUST_LOG=krun_devices::virtio::gpu::virtio_gpu=debug,warn`.

M1 depends on neither smithay nor libinput, so `Cargo.toml` does not carry them yet.

**When M2 adds smithay, pin upstream as a git dep at rev
`ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef`** — the base synoik's `synoik` branch forks from.
That is the exact API surface every piece of synoik code lifted here was written against,
which is the whole reason to pin it. Not a crates.io version (a version number is a guess
until something resolves it), and not synoik's fork either: its deltas are para-virt
cursor-plane hotspot and text-input work that this vehicle has no use for, and depending on a
personal fork would make testcomp harder to build than it needs to be.

## Building

Not on the host — this is a Linux binary. It builds in the `limina-build:fc43` container
alongside every other Linux build (see `docs/dev-onboarding.md`):

```
scripts/build-testcomp.sh
```

## Relationship to `kmschurn.py`

`crates/limina-test/guest/kmschurn.py` is the *minimal* vehicle: a KMS presenter with no
Wayland at all, which reproduced the 2026-08-07 retention bug and guards it today
(`crates/limina-test/tests/scanout_churn_retention.rs`). It stays — it is fast, has no
build step, and its narrowness is a feature when the question is "is the scanout path
itself leaking?".

This is the *realistic* vehicle, for the questions kmschurn cannot reach: client buffer
import, compositing with real surfaces, buffer lifetime across a client's death. See
`spikes/venus-churn-retention/buffer-lifetime-matrix.md`.

## The rule both vehicles live under

**It must reproduce a real failure at least once before any negative result from it
counts.** kmschurn earned that (+606 regions against the bug, +23 with the fix, over an
identical guest workload). testcomp has earned it twice, and the rule is **per failure class**:

| class | RED | GREEN | separation |
|---|---|---|---|
| scanout churn (M1) | `SURFACE_STORE_CAP` lifted: +4.17 GiB | shipped cap: +361 MiB | 11.5x |
| **client dmabuf retention (M3)** | `--leak-imports`: +41 alive IOSurfaces | shipped: +0 | flat vs 41 |

So "testcomp shows no leak on the client dmabuf path" is now a statement with content, for the
paths in the M3b table and **only** those. It still says nothing about:

- **vrend-allocated (gbm) client buffers** — the asymmetric holder of matrix §3, where vrend owns
  the IOSurface and venus only borrows. That is M3c, and it is the shape of the originally
  observed compositor-quit residual. Blocked on matrix §6's `vkr_budget_set_context` fix.
- **paths 3 and 4** — the `-1000158000` error-path partial and the ring-FATAL teardown. Path 4
  has a cheap deterministic vehicle available: an over-cap allocation trips
  `vkr_budget_kills_context` (`vkr_device_memory.c:285`), giving FATAL teardown on demand.

Each of those needs its own RED before its own green means anything.
