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
| **1** | KMS + Vulkan scanout allocation + churn. No Wayland, no input. | **Reproduces `kmschurn.py --vk`'s numbers on the same host.** Until the two vehicles agree, a difference could as easily be a transcription bug as a finding. |
| 2 | Wayland frontend (smithay), `wl_shm` clients | a real client maps and composites |
| 3 | `linux-dmabuf` import, client death | reaches the path × holder cases in `buffer-lifetime-matrix.md` |

M1 depends on neither smithay nor libinput, so `Cargo.toml` does not carry them yet. When
M2 adds smithay, the version to use is the one a working compositor on this exact stack
resolves — synoik pins a **git fork** (`github.com/kov/smithay`, branch `synoik`) for
para-virt cursor-plane hotspot support, not a crates.io release.

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
identical guest workload). This one has not yet — until it does, "the compositor shows no
leak" is not evidence of anything.
