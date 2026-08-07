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
| 3 | `linux-dmabuf` import, client death | reaches the path × holder cases in `buffer-lifetime-matrix.md` |

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
identical guest workload). testcomp earned it for the **scanout-churn class** on
2026-08-07 (+4.17 GiB against the bug, +361 MiB with the fix — see above).

It has **not** earned it for the classes it was actually built for: client dmabuf import and
buffer lifetime across a client's death. Those arrive with M3, and until one of them
reproduces something, "testcomp shows no leak on the client path" is not evidence of
anything. The rule is per failure class, not per vehicle.
