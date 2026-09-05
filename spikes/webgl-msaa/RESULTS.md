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

## The loss is display-size dependent

At `--display-size 2560x1440` the VM dies in 60–120 s. At `1280x800`, with `antialias` granted
(`SAMPLES=4`, `SAMPLE_BUFFERS=1`), cubes rendering and 14,944 frames drawn, it survives past four
minutes. Measured 2026-09-05, same build, no Metal validation in either.

This is the strongest structural hint available: whatever is exhausted or mis-addressed scales with
render-target size, and it retires every measurement taken at the smaller size.

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
| descriptor slot decoding | both virglrenderer implementations traced in guest ids | identical |

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

1. **Reproduce at 2560×1440 or not at all.** Every census below that size is invalid.
2. **Isolate by masking one pipeline.** With stable hashes: one validation run to collect UID↔hash
   pairs, then `MTL_SHADER_VALIDATION_DEFAULT_STATE=none` plus `ENABLE_PIPELINES=<one UID>` and
   `FAIL_MODE=allow`, one pipeline at a time. The pipeline whose masking *alone* both survives and
   blackens the cubes is the fatal read. Capture must be on or the blackening cannot be seen.
3. **Trace the 4-sample sampler view to its zink site**, since it is the one mismatch in the class
   that can misaddress rather than mis-sample.

## Method rules this bug earned

- **A validator class that fires on healthy frames explains nothing.** Diff *instances* between a
  failing and a passing half of the same run — and check the halves are what they claim before
  reading the diff.
- **Relaunching a browser does not reset it.** An A/B that stops Firefox and starts it again on the
  same profile gets session restore, so the second half runs both pages. Give each half its own
  `--profile <dir> --new-instance`, and create the directory: Firefox refuses a missing one with a
  modal, and the arm then measures a VM with no workload on it.
- **A capture is not optional.** Two arms in one session measured nothing — one where the browser
  never started, one where the halves were swapped — and in both the capture was the only thing
  that could have said so. Never run an arm without one.
- **Never change two variables to make an arm cheaper.** Dropping the display size to make
  validation affordable silently moved the run to a configuration that does not crash.
- **A label can perturb what it names.** A pipeline label is hashed into the pipeline UID, so
  adding one invalidates every UID recorded before it.
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
ssh … 'XDG_RUNTIME_DIR=/run/user/1000 systemd-run --user --unit=webglmsaa --collect \
        firefox --kiosk file:///home/claude/webgl-msaa.html'
```

Then watch the worker log for `DEVICE_LOST`. The KK-side knobs used above —
`LIMINA_KK_ALLOC_DESTROY`, `LIMINA_KK_BO_LEAK`, `LIMINA_KK_VIEW_LEAK`, `LIMINA_KK_DESCLOG`,
`KK_LIMINA_SHADER_DUMP`, `LIMINA_KK_LABELS` — all live on the `limina-kk` branch of
`/Volumes/mesa-cs/mesa`. Keep `--display-size 2560x1440`: the loss does not reproduce below it.

Full Metal shader validation is not survivable on this workload for more than a couple of minutes:
the slowed GPU makes KosmicKrisp's allocator pool run away (class 1 grew to 7,283 allocators,
"in-flight depth is outrunning completion") and the worker aborts. Use selective validation.

**A fix must show all four:** the page for ≥5 minutes, no `DEVICE_LOST`, no nil views, **and the
cubes visibly rendering in the capture**. Black cubes with no fault is the validation mask, not a
fix.
