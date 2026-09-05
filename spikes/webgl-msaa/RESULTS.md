# A WebGL page that asks for antialiasing loses the Vulkan device and kills the VMM

**Status:** OPEN — reproducible on the shipped stack. The fault is named (a GPU address
fault, not a hang); the cause is not.
**Vehicle:** `webgl-msaa.html`, self-contained (no network, generated texture). Three textured
cubes on a `webgl` context; `?aa=0` requests `{antialias:false}` for the control arm.
**Backlog entry:** `docs/hardening-backlog.md` §"A guest WebGL page that requests MSAA loses the
Vulkan device and aborts the VMM".

## What happens

An `{antialias:true}` context — which is also what `getContext('webgl')` with no options gives,
since `antialias` defaults to true in the spec — kills the VM in about two minutes:

```
MESA: error: ZINK: vkQueueSubmit failed (VK_ERROR_DEVICE_LOST)
[LIMINA-ALLOC-POOL] class 0 grew to 65 allocators — in-flight depth is outrunning completion
   ... thousands of growth lines ...
VM stopped — worker terminated by signal 6
```

The pool runaway is **downstream**, not a second fault: once the device is lost nothing completes,
so no allocator ever drains, and `kk_alloc_pool_get` mints on every request by design (it never
blocks on GPU progress). The abort is downstream again — see below.

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

## Where it aborts

From the crash report's faulting thread (`gpu worker`, SIGABRT, `abort() called`):

```
IOGPU          IOGPUMetalCommandBufferStorageAllocResourceAtIndex → abort()
AGXMetalG13X   ContextCommon::newCommand → beginComputePass
               -[AGXG13XFamilyCommandBuffer_mtlnext computeCommandEncoder]
KosmicKrisp    mtl_new_compute_command_encoder ← cs_get_compute
               ← kk_dispatch_precomp ← kk_cmd_write ← kk_heap ← kk_draw
zink           draw<zink_multidraw> → tc_call_draw_single → _tc_sync → tc_flush
               → st_glFlush → _mesa_make_current → eglMakeCurrent
virglrenderer  vrend context make_current ← submit ← virgl_renderer_submit_cmd
```

**This is not zink's device-lost abort.** Both of those (`zink_screen.h:96`, `zink_batch.c:624`)
are gated on `screen->abort_on_hang && !screen->robust_ctx_count`, and `abort_on_hang` comes from
`ZINK_HANG_ABORT`, default false. The abort is Apple's, inside IOGPU's command-buffer storage
allocator, and the plausible reading is AGX refusing to allocate with thousands of live
`MTLCommandAllocator`s outstanding — i.e. a consequence of the pool runaway, hence of the device
loss. One root, two derived symptoms.

The route is `cs_get_compute` — pre-gfx compute submitted inside an open render pass, on a
different command buffer and therefore a different allocator than the draws already recorded in
that pass. `kk_alloc_pool` calls this "the dangerous route" in its own comments.

## What has been ruled out

| variable | arms | result |
|---|---|---|
| KK revision | pinned `552edc3f62f` vs a bundle two commits older | both die |
| scanout path | windowed (`--window`) vs headless (`--display-capture`) | both die |
| guest tier | stock F44 vs enhanced F44 | both die |
| context attributes | `{antialias:true}`, defaults, vs `{antialias:false}` | only AA dies |
| fan unrolling | `LIMINA_ZINK_NO_FANS=1` vs default | both die |
| KK allocator pool retirement | `LIMINA_KK_ALLOC_DESTROY=0` (verified: 34 retirements → 0) | both die |
| BO lifetime | `LIMINA_KK_BO_LEAK=1` — nothing ever released or de-resident | both die |
| texture residency | plane textures + sampled/storage views registered | both die |
| MSAA resolve implementation | meta/shader resolve vs upstream's Metal render-pass resolve | **neither runs** |

The last four are the load-bearing ones, because each closes a whole mechanism rather than a
setting. Nothing KK allocated was ever freed or made non-resident in the leak arm, so the faulting
address is not a dangling one; the fault is a *live* address the GPU could not translate.

**No Vulkan resolve is involved, in either implementation.** Upstream mesa replaced KosmicKrisp's
meta/shader resolve with Metal's native render-pass resolve (`db5ab8de776`, `a86f79d5f66`, MR
43216), which is the obvious candidate for an MSAA fault and post-dates our base. Ported onto
`limina-kk` it does not help — and instrumenting the path shows why: `kk_attachment_do_renderpass_resolve`
never once sees an attachment with `resolve_mode != VK_RESOLVE_MODE_NONE` on this workload. zink
emulates `EXT_multisampled_render_to_texture` with a `util_blitter` draw, so the multisample
traffic here is ordinary rendering to and sampling from a 4-sample texture, and never a resolve.
Both resolve implementations are irrelevant to this bug. (The port lives on the local
`msaa-resolve-test` branch of the KK checkout; it is upstream work we get on the next rebase, not
something to carry.)

## What the fault actually is

Metal names it, once KosmicKrisp stops discarding the reason (`kk: say why the device was lost`
on `limina-kk`):

```
[LIMINA-DEVICE-LOST] MTL_COMMAND_QUEUE_ERROR_TIMEOUT (code 1) gpu=...(6.3 ms)
  message: The operation couldn't be completed. (MTL4CommandQueueErrorDomain error 1.)
  details: ... Code=2 "Caused GPU Address Fault Error
           (0000000b:kIOGPUCommandBufferCallbackErrorPageFault)"
```

**Read the underlying error, not the outer one.** The outer code is `TIMEOUT` and the underlying
one is a page fault; 6 ms of GPU time says the command buffer touched something invalid, not that
it ran long. Taking the outer code at face value sends you looking for a hang that is not there.

## The residency reports are chronic, and do not explain this

Metal's debug layer reports, on every frame, that attachment textures are "not added to any
residency set". Labelling the textures makes the reports name a 4-sample 2D-multisample texture at
the canvas size as the dominant offender — 383 hits against 36 for the next. That is a real
omission and is now fixed, and the reports go to zero. **The fault survives it.** The tell was
there in advance: the same reports fire for single-sample textures that never fault, and a class
that fires constantly on healthy frames cannot explain a failure specific to one of them.

## What still points somewhere

Metal shader validation with `MTL_SHADER_VALIDATION_FAIL_MODE=allow` prevents the loss for as long
as it has been run. Its reports carry a pipeline UID and a `program_source` line, so instances —
not classes — can be diffed between an AA and a non-AA half of one run. Three pipelines appear only
in the AA half with real volume (66795, 22264, 701 hits), all of class *"MTLResourceUsage flags
mismatch or missing for texture executing fragment function"*. That is the sharpest surviving lead.

Two cautions on it. Validation also slows the workload by roughly an order of magnitude, so its
survival is not by itself proof of masking; the control that separates those is to keep validation
on and turn off only `MTL_SHADER_VALIDATION_TEXTURE_USAGE` / `_RESOURCE_USAGE`. And the class is
chronic like the residency one — it is the *instance* diff, not the class, that carries the signal.

The mechanism this would fit: KosmicKrisp has no `VK_EXT_multisampled_render_to_single_sampled`,
so MSAA resolve goes through `vk_meta_resolve_rendering` — a fragment shader that samples the
multisample texture. A shader handed a resource ID it cannot correctly dereference faults exactly
this way. KK's texture-type plumbing is independently known to be confused somewhere ("Invalid
texture type MTLTextureType2DArray bound to shader, expected MTLTextureType2D", 127k hits) —
though that class is equally present without MSAA, so it is a neighbour, not the culprit.

## Method notes worth keeping

- **A validator class that fires on healthy frames explains nothing.** Diff *instances* (pipeline
  UID + source line) between a failing and a passing half of the same run; classes are identical,
  instances are not.
- **Give it the full window, and do not read process liveness as health.** This bug kills at
  ~2 minutes, so a check at 150 s can still read healthy.
- **One capture path per arm.** Arms sharing a `LIMINA_WINDOW_CAPTURE` / `--display-capture` file
  overwrite each other's only evidence that the workload was running at all — a VM that "survived"
  because the browser had exited reads exactly like a VM that survived.

## Reproducing

Boot any enhanced F44 image and run the page in the seated session:

```
cargo xtask run --disk <clone>.raw          # or LIMINA_DISPLAY_CAPTURE=<png> for headless
scp spikes/webgl-msaa/webgl-msaa.html claude@127.0.0.1:/home/claude/   # port from the worker log
ssh … 'XDG_RUNTIME_DIR=/run/user/1000 systemd-run --user --unit=webglmsaa --collect \
        firefox --kiosk file:///home/claude/webgl-msaa.html'
```

Then watch the worker log for `DEVICE_LOST`. Allow ~2 minutes: a check at 150 s read as healthy on
a VM that died 40 s later.
