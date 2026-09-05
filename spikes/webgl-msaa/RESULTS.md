# A WebGL page that asks for antialiasing loses the Vulkan device and kills the VMM

**Status:** OPEN — reproducible on the shipped stack, and localized to one shader's texture
read. The faulting access is named; the reason the binding is wrong is not.
**Vehicle:** `webgl-msaa.html`, self-contained (no network, generated texture). Three textured
cubes on a `webgl` context; `?aa=0` requests `{antialias:false}` for the control arm.
**Backlog entry:** `docs/hardening-backlog.md` §"A guest WebGL page that requests MSAA loses the
Vulkan device and aborts the VMM".

## What happens

An `{antialias:true}` context — which is also what `getContext('webgl')` with no options gives,
since `antialias` defaults to true in the spec — kills the VM in one to two minutes:

```
MESA: error: ZINK: vkQueueSubmit failed (VK_ERROR_DEVICE_LOST)
[LIMINA-ALLOC-POOL] class 0 grew to 65 allocators — in-flight depth is outrunning completion
   ... thousands of growth lines ...
VM stopped — worker terminated by signal 6
```

The pool runaway is **downstream**, not a second fault: once the device is lost nothing completes,
so no allocator ever drains, and `kk_alloc_pool_get` mints on every request by design (it never
blocks on GPU progress). The abort is downstream again — it lands in Apple's
`IOGPUMetalCommandBufferStorageAllocResourceAtIndex`, reached via `cs_get_compute` ←
`kk_dispatch_precomp` ← `kk_draw`, which is AGX refusing to allocate with thousands of live
`MTLCommandAllocator`s outstanding. It is **not** zink's device-lost abort: both of those are
gated on `abort_on_hang`, which is `ZINK_HANG_ABORT`, default false.

`{antialias:false}` runs indefinitely with the frame still animating.

## MSAA is genuinely taken, which is new

The page prints what the driver **granted**, not what it asked for: `granted aa=true SAMPLES=4
SAMPLE_BUFFERS=1`. So the long-documented Firefox behaviour — MSAA backbuffer reports
`FRAMEBUFFER_INCOMPLETE_ATTACHMENT`, Firefox silently falls back to non-AA, cosmetic only — **does
not hold on the current KosmicKrisp stack.** That finding was measured on the retired MoltenVK
backend and was never re-measured on KK.

This is the whole reason the failure went unnoticed: pages that never take the MSAA path are fine,
and until MSAA started actually working there was no MSAA path to take. Any A/B here **must** read
`getContextAttributes().antialias`; "requested AA" is not evidence AA happened.

## What Metal reports, and what it does not

KosmicKrisp now prints the reason it used to discard (`kk: say why the device was lost` on
`limina-kk`). The outer code is always `MTL_COMMAND_QUEUE_ERROR_TIMEOUT`; the underlying error is
one of two, and **which one varies between runs of the identical build**:

| underlying error | GPU time |
|---|---|
| `kIOGPUCommandBufferCallbackErrorPageFault` | ~6 ms |
| `kIOGPUCommandBufferCallbackErrorHang` | ~46 ms |

Measured across five arms, roughly evenly split. **Read the underlying error, not the outer one** —
the outer `TIMEOUT` sends you after a hang that is only half the time even there. Neither figure
is a long-running command buffer; both are what a bad resource dereference looks like from
outside, depending on whether the address translated.

## What the validator's reports do and do not say

The dominant report class is a texture-type mismatch:

```
Invalid texture type MTLTextureType2DArray bound to shader, expected MTLTextureType2D,
executing fragment function: "main_entrypoint"
pipeline: "msl=<hash>", UID: "<hash>" encoder: "87", draw: 6
```

Three properties of the class bound how much it can carry. It is **chronic** — it fires on many
pipelines in every run, a healthy desktop included. It is **bidirectional**: `2D` bound where
`2DArray` was expected occurs alongside the reverse. And a report carries no texture identity, no
address and no binding index, so a report alone can never name the resource that produced it.

An earlier reading of this section identified one pipeline as the faulting access. That was wrong
on two counts: the pipeline named was reporting a different validation class (`MTLResourceUsage
flags mismatch`), and no single pipeline survives a controlled AA-versus-non-AA repeat. Nothing
here currently names a faulting access.

**Pipelines are now self-identifying.** KosmicKrisp labels each `MTLRenderPipelineState` with a
hash of its generated MSL, and `KK_LIMINA_SHADER_DUMP` names its files by the same hash, so a
report, a shader dump and a second run's report all name one thing. The label must be content-
derived: it is hashed into the UID Metal reports against, so labelling with a per-run pointer makes
every UID per-run too and two runs cannot be joined at all.

## The failing path, read off the wire

`LIMINA_VREND_TRACE=256` on the killing configuration, dumped 25 s in (the tracer dumps
on `echo x > $LIMINA_VREND_TRACE_FIFO`; the worker SIGABRTs, so its `atexit` dump never
runs). In a 31-second window: 3,220 draws, 248 framebuffer changes, and **134 BLITs,
every one of them the same shape**:

```
src  res=1209  1280x720  nr_samples=4  format=67  bind=0xa
dst  res=1217/1218/1229/1230/1231/1233  1280x720  nr_samples=0  format=1  bind=0x10000a
```

That is an **explicit multisample resolve** — one 4-sample colour texture resolved into
each of six rotating single-sample destinations carrying the shared/scanout bind bit,
i.e. a swap-chain pool. The whole guest run contains exactly two multisampled resources
(the colour target above and its 4-sample depth companion, res=1210).

**Source and destination formats differ (67 vs 1), and that decides the route.** On a
GLES host — which limina is — `vrend_renderer_prepare_blit` returns false for an
MS-source RGBA blit whose `src.format != dst.format`, so the blit does **not** take the
FBO path; it falls through to `vrend_renderer_blit_gl`, the shader blitter, running in
its own GL context (`third_party/virglrenderer/src/vrend/vrend_renderer.c:12751-12764`,
dispatch at `:12965-12973`).

**This is not the path any probe took.** `host-msaa-loop.c` and `guest-msaa-present.c`
both use `EXT_multisampled_render_to_texture`, which is implicit MSAA: vrend's
`feat_implicit_msaa`, zink's shadow-attachment emulation, no `VIRGL_CCMD_BLIT` at all.
Their negatives — 226,909 frames in the guest, 144,037 presenting — constrain the
shadow-attachment path and say nothing about this bug. Every "shadow blit" line in the
older parts of this record describes the probe, not the browser.

The route is now read directly from the renderer rather than inferred from the source.
`LIMINA_VREND_BLIT_LOG=1` (on the `limina` branch of our virglrenderer fork; the shipped
build compiles `VREND_DEBUG` out) prints each distinct blit shape once with the route and
the predicate that forced it. On the killing configuration it prints exactly two shapes:

```
FBO   src=fmt67/s0 2560x1440 -> dst=fmt67/s0 2560x1440  (swizzle=0 redblue_or_fmt=0 …)
GLFB  src=fmt67/s4 1280x720  -> dst=fmt1/s0  1280x720   (swizzle=0 redblue_or_fmt=1 …)
```

The desktop's own compositing blit stays on the FBO path; **every MSAA resolve takes the
shader blitter**, and `redblue_or_fmt=1` names the format difference as the reason. The
companion knob `LIMINA_VREND_FORCE_FBO_BLIT=1` keeps such a blit on the FBO path (colours
come out wrong, which is acceptable for an arm asking only whether the route is what loses
the device); that arm has not been read yet.

## The size and fullscreen framings are both dead

They were built on single-shot arms with no repeats. The archived logs falsify them
outright: at **1280x800**, `log-armL-noval.log` dies at +66 s while `cap-armS.png`'s run
survived 240 s with antialiasing granted and 14,944 frames drawn. The same display size
both kills and survives, so display size was never the variable, and neither was the
canvas size or fullscreen that replaced it.

The "+66 to +85 s schedule" the archived logs seemed to show is dead too, and the way it
died named a much larger error: **an arm launched over plain `ssh` is not this workload.**
Firefox started from an ssh command line — even with `WAYLAND_DISPLAY` and a live
compositor — does not inherit the seated session's environment, renders through a
different driver, and survives indefinitely. Two such arms read as clean 4-minute
survivals of a configuration that kills. Launched through the session's own manager
(`systemd-run --user`, with `DBUS_SESSION_BUS_ADDRESS` exported so the call does not fail
silently), the same build on the same clone lost the device inside 4 minutes. The launch
method is a load-bearing variable, not a convenience.

Two further corrections fall out:

- **The validation-mask arm ran at 1280x800** (`armK.sh` sets no `--display-size`, and
  `cap-armK.png` is 1280x800) — a configuration that both kills and survives. "Validation
  masks the loss" is therefore not established, and neither is the texture-type lead that
  rested on it. Worse, "the cubes render black" is indistinguishable from a zeroed canvas
  against this page's near-black clear colour; the page needs a loud clear before that
  observation can mean anything.
- **Metal names nothing.** The device-lost report carries `NSMultipleUnderlyingErrorsKey`
  and an `IOGPUCommandQueueErrorDomain` code and no encoder, label or resource, so the
  faulting work has to be named by our own instrumentation.

## What has been ruled out

| variable | arms | result |
|---|---|---|
| KK revision | pinned `552edc3f62f` vs a bundle two commits older | both die |
| scanout path | windowed (`--window`) vs `--display-capture` | both die |
| guest tier | stock F44 vs enhanced F44 | both die |
| context attributes | `{antialias:true}`, defaults, vs `{antialias:false}` | only AA dies |
| fan unrolling | `LIMINA_ZINK_NO_FANS=1` vs default | both die |
| KK allocator pool retirement | `LIMINA_KK_ALLOC_DESTROY=0` (verified: 34 retirements → 0) | both die |
| BO lifetime | `LIMINA_KK_BO_LEAK=1` — nothing ever released or de-resident | both die |
| image-view lifetime | `LIMINA_KK_VIEW_LEAK=1` — no view ever released | both die |
| nil texture views | `mtl_new_texture_view_with` reports nil unconditionally | zero, three arms |
| texture residency | plane textures + sampled/storage views registered | both die |
| MSAA resolve implementation | meta/shader resolve vs upstream's Metal render-pass resolve | **neither runs** |
| the zink shadow blit alone | `spikes/zink-shadow-recursion` on host zink-on-KK, no VM | **no mismatch** |
| MSAA render-to-texture at full size, host | `host-msaa-loop`, 2560x1440, 4 samples, no VM | 20,520 frames, survives |
| the same, plus reallocation churn | `--churn 30`, canvas+depth+FBO rebuilt every 30 frames | 30,712 frames, survives |
| the same, inside the guest over virgl | `msaa-loop` on the guest, MSAA granted (`SAMPLES=4`) | **226,909 frames, survives** |
| presenting fullscreen, multisampled surface | `guest-msaa-present --fullscreen --mode surface` | 144,037 frames, survives |
| presenting fullscreen, MSRTT canvas blitted 1:1 | `guest-msaa-present --fullscreen --mode msrtt` | 139,000+ frames, survives |
| Firefox's native compositor | `gfx.webrender.compositor=false` on the killing config | **dies in 65 s** |
| descriptor slot decoding | both virglrenderer implementations traced in guest ids | identical — but a **venus** result, so it constrains nothing on this GL path |

**No Vulkan resolve is involved, in either implementation.** Upstream mesa replaced KosmicKrisp's
meta/shader resolve with Metal's native render-pass resolve (`db5ab8de776`, `a86f79d5f66`, MR
43216), which is the obvious candidate and post-dates our base. Ported onto `limina-kk` it does not
help — and instrumenting the path shows why: `kk_attachment_do_renderpass_resolve` never once sees
an attachment with `resolve_mode != VK_RESOLVE_MODE_NONE` on this workload. zink emulates
`EXT_multisampled_render_to_texture` with a `util_blitter` draw, so the multisample traffic here is
ordinary rendering to and sampling from a 4-sample texture, and never a resolve. (The port lives on
the local `msaa-resolve-test` branch of the KK checkout; it is upstream work we get on the next
rebase, not something to carry.)

**The shadow blit alone does not produce the mismatch.** `spikes/zink-shadow-recursion` drives
`zink_render_attachment_shadow` against host zink-on-KK with no VM in the loop; under full Metal
shader validation it reports nothing. Its blit source is a `2D_ARRAY` view of the multisample
transient, `MTLTextureType2DMultisampleArray`, and the validator is content with it — so zink and
the blit shader agree on that path.

**Descriptor writes are honest per slot.** With the log keyed on (set, binding, array element,
texture identity) rather than on a type tuple, every sampled-image write in a run is internally
consistent. What the log does show is the shape of what zink asks for: mipmap chains, 2560×1440
down to 2×1, each level a separate `VK_IMAGE_VIEW_TYPE_2D_ARRAY` view with `layerCount=1`, all
written into one slot. Array views reaching shaders that declare plain `texture2d` is therefore
routine here, which is consistent with the mismatch class being chronic and mostly survivable.

One write in that log is a genuine Vulkan violation rather than a curiosity: a
`COMBINED_IMAGE_SAMPLER` naming a `VK_IMAGE_VIEW_TYPE_2D` view of a **4-sample** image, so a
shader declaring `texture2d<float>` samples an `MTLTextureType2DMultisample`. That is the
memory-unsafe member of the class — Metal's 2D and 2DMultisample layouts differ, so the read is a
genuine misaddress rather than a silent layer-0 substitution. It is **not** yet tied to the loss.

## Where to look next

The path is named, so the next arms are on it rather than near it.

1. **`vrend_renderer_blit_gl` is where every frame's resolve goes.** It runs in the
   blitter's own GL context against a texture shared with the client's context, and
   there is already an open finding that the shader blitter leaves its texture
   parameters on the shared source texture. Instrument that function first: log the
   source/destination resources, the sampler state it sets and restores, and whether the
   source is multisampled.
2. **Make the blit take the FBO path instead** and see if the loss goes. The route is
   chosen only because `src.format != dst.format`; a targeted change (or forcing the
   comparison true for this case) is a one-line experiment with a clear reading — it
   survives, the GL fallback is implicated; it dies, the resolve is innocent and the
   destination or the sharing is the ingredient.
3. **Rebuild the probe against the explicit path**, not the implicit one: a real
   `glRenderbufferStorageMultisample` target resolved with `glBlitFramebuffer` into a
   destination whose format differs from the source, and — for the closest rung — into
   an imported/dmabuf-backed texture sampled by a second context.
4. **Forward zink's debug labels to the Metal encoder.** KosmicKrisp advertises
   `EXT_debug_utils` but only labels encoders for capture; `MESA_TRACE=markers` makes
   zink emit `blit_resolve(...)`/`copy_image(...)` labels, and passing them through would
   make the device-lost report name the operation instead of nothing.

Arms that are no longer worth running: anything varying display size, canvas size or
fullscreen, since 1280x800 both kills and survives.

## Method rules this bug earned

- **A validator class that fires on healthy frames explains nothing.** Diff *instances* between a
  failing and a passing half of the same run — and check the halves are what they claim before
  reading the diff.
- **Relaunching a browser does not reset it.** An A/B that stops Firefox and starts it again on the
  same profile gets session restore, so the second half runs both pages. Give each half its own
  `--profile <dir> --new-instance`, and create the directory: Firefox refuses a missing one with a
  modal, and the arm then measures a VM with no workload on it.
- **The workload must be shown running before a survival is read.** A survival is the one
  reading a broken arm produces for free: no browser, no session environment, wrong driver,
  all look like health. Check the capture *and* that the guest process exists before
  starting the clock, and prefer launching through the session manager over `ssh`.
- **A capture is not optional.** Two arms in one session measured nothing — one where the browser
  never started, one where the halves were swapped — and in both the capture was the only thing
  that could have said so. Never run an arm without one.
- **Never change two variables to make an arm cheaper.** Dropping the display size to make
  validation affordable silently moved the run to a configuration that does not crash.
- **A label can perturb what it names.** A pipeline label is hashed into the pipeline UID, so
  adding one invalidates every UID recorded before it.
- **A liveness check must not match itself.** Two arms reported a full-duration
  "survived" from a loop whose `grep 'debug/limina-vmm --cpus'` matched its own grep
  process, so the loop could never observe the VM exiting. The worker log said the VM
  had died mid-arm. Always bracket the pattern (`'[l]imina-vmm'`), and confirm a
  survival against the worker log rather than against the watcher that was supposed to
  notice.
- **Read the size the workload actually got, not the one you configured.** A fullscreen
  kiosk window on a 2560x1440 display at scale 2 is 1280x720 logical, so "the canvas
  follows the window" meant 1280x720 while a deliberately-forced canvas was 2560x1440 —
  the killing arm had the *smaller* buffer. The page prints its drawing-buffer size for
  exactly this reason; a table built without it inverted the conclusion.
- **Prove the mask is not the slowdown.** Validation costs ~10x; that a workload survives under it
  means nothing until an arm shows the mask working at full speed, or the fault returning with the
  slowdown kept.
- **Give it the full window, and do not read process liveness as health.** This bug kills between
  60 s and ~2 minutes, so a check at 150 s can still read healthy.

## Reproducing

Boot any enhanced F44 image and run the page in the seated session:

```
cargo xtask run --disk <clone>.raw          # or LIMINA_DISPLAY_CAPTURE=<png> for headless
scp spikes/webgl-msaa/webgl-msaa.html claude@127.0.0.1:/home/claude/   # port from the worker log
ssh … 'export XDG_RUNTIME_DIR=/run/user/1000 \
             DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; \
        rm -rf /tmp/ff && mkdir -p /tmp/ff; \
        systemd-run --user --unit=webglmsaa --collect /usr/bin/firefox \
            --profile /tmp/ff --new-instance --kiosk \
            file:///home/claude/webgl-msaa.html?aa=1'
```

Then watch the worker log for `DEVICE_LOST`. The KK-side knobs used above —
`LIMINA_KK_ALLOC_DESTROY`, `LIMINA_KK_BO_LEAK`, `LIMINA_KK_VIEW_LEAK`, `LIMINA_KK_DESCLOG`,
`KK_LIMINA_SHADER_DUMP`, `LIMINA_KK_LABELS` — all live on the `limina-kk` branch of
`/Volumes/mesa-cs/mesa`. Arms here all run at `--display-size 2560x1440`. Size is not the variable (see above), but
holding it fixed removes one source of noise.

Full Metal shader validation is not survivable on this workload for more than a couple of minutes:
the slowed GPU makes KosmicKrisp's allocator pool run away (class 1 grew to 7,283 allocators,
"in-flight depth is outrunning completion") and the worker aborts. Use selective validation.

**A fix must show all four:** the page for ≥5 minutes, no `DEVICE_LOST`, no nil views, **and the
cubes visibly rendering in the capture**. Black cubes with no fault is the validation mask, not a
fix.
