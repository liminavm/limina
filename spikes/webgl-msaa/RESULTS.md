# A WebGL page that asks for antialiasing loses the Vulkan device and kills the VMM

**Status:** OPEN — reproducible on the shipped stack, cause not found.
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
| fan unrolling | `LIMINA_ZINK_NO_FANS=1` vs default | **both die** (device loss, then signal 6) |

The last row matters most. The workload's most striking signature is geometry unrolling —
`unroll triggers: fan=69366`, `midpass pre_gfx caller: kk_draw 83666` — which is also the
signature named as the live lead for the separate AGX allocator crash class. Disabling fan
unrolling does **not** prevent the device loss, so the fan route is not the trigger. It remains
the route the eventual abort travels, which is a different claim.

MSAA render-to-texture on this stack is structurally special for one reason worth keeping in
view: KosmicKrisp does not implement `VK_EXT_multisampled_render_to_single_sampled`, so every such
render goes through `zink_render_attachment_shadow` and a `util_blitter` blit with
`ctx->blitting` set. That path already carries one fixed defect (infinite recursion) and one open
one (a stale `pStencilAttachment` against `VK_FORMAT_UNDEFINED`). Neither is confirmed to be this.

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
