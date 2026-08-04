# vrend/virgl desktop corruption — ONE observation, not yet reproduced

Date: 2026-08-04. Vehicle: `vrend-arm.raw` (clone of `Fedora-Workstation-44.enhanced.test.raw`),
booted with `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`. Found while trying to run a GL A/B
(task #26) for the "drop guest zink, use vrend for GL" proposal.

> **Read this first.** This file was rewritten twice because two earlier conclusions were wrong.
> A third pass then found the actual mechanism — see **"DIAGNOSED"** below. The void conclusions
> are kept because their failure mode is the reusable lesson.

## DIAGNOSED (2026-08-04): vrend poisons the compositor's context on a rejected command

The user pushed back on being told the symptom was benign ("I've seen glmark run hundreds of times
… there is nothing on the desktop other than the grinding dash"). That was correct and it led
straight to the mechanism.

Guest kernel, **1145 errors, starting 7 s after boot and continuing for the whole session**:

```
[drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1200 (command 0x207)
```

`0x207` = `VIRTIO_GPU_CMD_SUBMIT_3D`, `0x1200` = `VIRTIO_GPU_RESP_ERR_UNSPEC`. Host side, the same
thing at **~120 failures per second, sustained**, with the originating cause logged once:

```
virgl: vrend_decode_ctx_submit_cmd: context error reported 2 "gnome-shell" Illegal command buffer
virgl: context 2 failed to dispatch PIPE_RESOURCE_SET_TYPE: 22
```

**Mechanism.** gnome-shell's virgl context issues `VIRGL_CCMD_PIPE_RESOURCE_SET_TYPE`; vrend rejects
it with `EINVAL`; the context is then marked in error **permanently**, so every subsequent
`SUBMIT_3D` fails. Rendering stops, already-present content goes stale, and the desktop rots
progressively — which is exactly the reported "started with wrong colors, but degraded", persisting
after the load stopped. Same class as [[limina-ring-wait-fatal]]: a context marked fatal, with a
guest-visible symptom that looks unrelated to the cause.

**Rejection site.** `vrend_decode_pipe_resource_set_type` (`src/vrend/vrend_decode.c:1601`) returns
`EINVAL` when the length disagrees with the plane count, when `plane_count == 0`, or when
`plane_count > VIRGL_GBM_MAX_PLANES`. Note the GBM framing: our macOS build is deliberately
**GBM-less** (`085e1991 vrend(macOS): wire the no-GBM surfaceless EGL winsys`), and this command is
the dmabuf/modifier resource-typing path — a plausible place for a GBM-less host to disagree with a
guest that assumes GBM-shaped planes. **Which of the three conditions fires is not yet confirmed** —
that needs a one-line probe logging `length` and `plane_count` at the rejection, and is the next
step.

This also explains the non-determinism that defeated six reproduction attempts: whether the guest
emits `PIPE_RESOURCE_SET_TYPE` at all depends on what the compositor happens to do, not on
resolution or load.

**Corollary — scores from a poisoned session are fiction, and the evidence is quantitative.**
glmark2 counts its own frames, so failed submissions inflate FPS rather than erroring. The virgl
`build` scores track the damage:

| session | state | virgl `build` score |
|---|---|---|
| clean boot | healthy | 2365 |
| first A/B (the one that corrupted) | poisoned | 2554 / 2801 / 2867 |
| faithful replication | poisoned, connection dropped mid-suite | **3316 / 3337 / 3413** |

The worse the poisoning, the "faster" virgl looks. **That is very likely most or all of the
originally reported ~2x virgl advantage**, and it independently corroborates the 2026-07-29 honest
crossmark (`limina-virgl-vrend-perf`), which found venus winning or tying every guest cell once
vrend's fence dishonesty was fixed.

**A `virtio_gpu_dequeue_ctrl_func` count belongs in the harness as a comparability gate** — abort
rather than record a GL number from a session with a poisoned context.

**A misapplied exoneration, recorded so it is not repeated.** Earlier in this same investigation the
identical `SUBMIT_3D`/`ComponentError(22)` signature was dismissed as the documented-benign
`gst-plugin-scan` startup probe (`limina-virgl-vrend-perf`). That memory is right about the *boot*
occurrence and wrong as a blanket rule. The distinguishing evidence was always available and was not
checked: the benign ones are a handful before the login prompt; these were continuous at 120/s.
**Check the timestamps and the rate before reusing a known-red-herring verdict.**

## What is actually known

**One confirmed sighting.** During a long benchmark session the entire desktop became corrupted
(`glmark2-virgl-corruption.png`, human screenshot). Conditions at the time:

- compositor GL path: **virgl/vrend**
- display: `--display-resolution 1280x800`
- load: the **full glmark2 suite run twice back-to-back** (~10 min), virgl client then zink client
- described as progressive: "started with wrong colors, but degraded"
- the desktop **stayed** corrupted after the load stopped
- **no host-side error** accompanied it (the `ComponentError(22)` / "Illegal command buffer" lines
  in the worker log are boot-time `gst-plugin-scan` `CREATE_VIDEO_BUFFER` failures, timestamped
  before the login prompt — a known red herring, see `limina-virgl-vrend-perf`)

**Everything since has been clean.** Not reproduced in any of: match-host virgl (fresh), fixed
2560x1440 virgl (fresh), 1280x800 virgl (~40 s load), 1920x1200 virgl (~40 s load), fixed 2560x1440
virgl (one full suite), fixed 2560x1440 zink-on-venus (one full suite).

## Two conclusions that were published here and are VOID

**VOID 1 — "the trigger is `--display-resolution` / letterboxing."** Every clean observation was a
fresh boot or a short run and the one corrupt observation was the long benchmark, so resolution and
load-duration moved together. Resolution was the interesting variable, so it got blamed.

**VOID 2 — "the trigger is sustained GL load."** Replaced VOID 1 after a capture at fixed 2560x1440
under a full glmark2 suite *appeared* corrupted. It was not: that image is **normal glmark2 output**
— a fragment-shader scene (`conditionals`/`function`/`loop` draw a rotating quad with high-frequency
procedural output) seen small inside the Activities overview. Human-confirmed. The tell that should
have caught it immediately: the zink-on-venus control produced a **near-identical** image, and
corruption is stochastic — two independent GL stacks do not corrupt alike. Both
`reproduced-at-2560x1440-under-load.png` (misnamed; kept for the record) and `cap-zinkload` are
normal frames.

Consequence: the zink-under-load control is **also uninformative** — neither arm corrupted, so it
says nothing about whether the fault is vrend-specific. That remains open.

## The lesson, earned three times in one afternoon

Every wrong turn had the same shape: **two variables moved together and the more interesting one got
the blame.**

1. Host-mesa rebuild vs. display resolution → blamed the rebuild (8 commits, texture-layout-adjacent,
   straddling the before/after exactly). Reverting only the resolution, libraries untouched,
   exonerated all eight.
2. Display resolution vs. load duration → blamed resolution.
3. Then over-read a capture as corruption because it was expected, without checking it against a
   known-good reference of the same scene.

Corollaries worth keeping:

- **Proxies lie, and so does eyeballing a capture you expect to be broken.** Compare against a
  reference frame of the same workload before calling something corrupt.
- **Identical output across two independent stacks means "not corruption"** — or that the
  differential is not reaching the system under test.
- **Collapse to one VM window before asking a human about pixels.** Three windowed VMs were up
  during the early observations, making every visual verdict ambiguous about which VM it described.

## Ruled out (still valid)

- **The guest-zink patches** (0001/0003/0004/0006). Wrong failure mode — crash guards and feature
  emulation, which abort or lose a feature rather than corrupting pixels; preconditions provably
  absent on KK; none is load- or resolution-dependent.
- **The eight host-mesa commits** (exonerated by the resolution revert).
- **Producer-side stride handling.** `vrend_renderer_resource_sync_iosurface`
  (`vrend_renderer.c:8776`) allocates the IOSurface at the resource's own `width0/height0` and sets
  `GL_PACK_ROW_LENGTH` from the real `IOSurfaceGetBytesPerRow` — correct.

## Background: what the vrend scanout actually is

`d093cf90` implemented **plan A1** of `docs/design/vrend-iosurface-scanout.md`: a
`GL_AMD_pinned_memory` PBO aliased over `IOSurfaceGetBaseAddress`, so `glReadPixels` blits **directly
into the scanout IOSurface's bytes**. "Zero-copy" there means *the CPU never touches pixels* — it
replaced readback → per-pixel RGBA→BGRA convert → canvas upload. A GPU-side copy remains.

Two escalations were specified and never done: **A2** (render directly into the IOSurface, removing
even that blit) and **C** (KK IOSurface external-memory handle type via
`newTextureWithDescriptor:iosurface:plane:`, which would also make zink `resource_get_handle`
wireable). **Plan C now exists for venus** — it is the MTLTEXTURE handle type implemented
2026-08-04 — so extending it to vrend is a much shorter step than when the plan was written. That is
a genuine improvement on its own merits; it is **not** a known fix for this bug, which is not
localized to that path or to vrend at all.

## Next

1. **Reproduce first.** Replicate the exact original conditions: virgl compositor, 1280x800, full
   glmark2 suite ×2 back-to-back. Without a repro there is nothing to bisect and no way to know a
   fix worked.
2. If it reproduces, cheap localization before any implementation: gate the IOSurface fast path off
   (`vrend_renderer.c` bails to the readback fallback when `feat_amd_pinned_memory` is absent, so a
   one-line `getenv` gives a free A/B of the whole zero-copy scanout).
3. Only then chase a mechanism — and only then decide whether plan C for vrend is a fix or merely an
   improvement.
