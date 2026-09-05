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

## The faulting access

Metal shader validation names it. With `MTL_SHADER_VALIDATION=1` the loss does not occur, and the
dominant report — 111,873 hits in an 11-minute run, against 305 for the next — is:

```
Invalid texture type MTLTextureType2DArray bound to shader, expected MTLTextureType2D,
executing fragment function: "main_entrypoint"
pipeline UID: B7B504DD3C3CD60243A9EAA4E55AE34B5D3A4BADD0FEDAFB060D7916720ABBD9
	* frame #0: main_entrypoint() - /program_source:69:19
```

That pipeline's MSL (via `KK_LIMINA_SHADER_DUMP`) is **u_blitter's blit fragment shader** — one
`texture2d<float>` sampled with a LOD bias and written to all eight color outputs — which is what
`zink_render_attachment_shadow` runs, since KosmicKrisp implements no
`VK_EXT_multisampled_render_to_single_sampled`. Line 69 is its `.sample()`. The NIR that produced
it declares `sampler2D` and a `2D` tex op, so **KK's code generation is right and the binding is
wrong**: a 2D-array texture reached a slot the shader dereferences as 2D.

Two independent confirmations that this read is the fault:

- **Validation masks it at full speed.** 11 minutes, 54 fps, no loss — and the cubes render
  **black**, because `FAIL_MODE=allow` makes the invalid read return zero instead of faulting.
  The identical build with no `MTL_*` env dies in 65 seconds with the cubes rendering correctly.
  So masking is not the ~10x slowdown full validation costs; the mask is the substituted read.
- **The address is live.** Two leak arms (below) removed every free from the picture and the loss
  survived both, so this is a mistyped binding, not a dangling one.

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
| the sampled-image descriptor write | `LIMINA_KK_DESCLOG=1`, every distinct type tuple | all honest |

**No Vulkan resolve is involved, in either implementation.** Upstream mesa replaced KosmicKrisp's
meta/shader resolve with Metal's native render-pass resolve (`db5ab8de776`, `a86f79d5f66`, MR
43216), which is the obvious candidate and post-dates our base. Ported onto `limina-kk` it does not
help — and instrumenting the path shows why: `kk_attachment_do_renderpass_resolve` never once sees
an attachment with `resolve_mode != VK_RESOLVE_MODE_NONE` on this workload. zink emulates
`EXT_multisampled_render_to_texture` with a `util_blitter` draw, so the multisample traffic here is
ordinary rendering to and sampling from a 4-sample texture, and never a resolve. (The port lives on
the local `msaa-resolve-test` branch of the KK checkout; it is upstream work we get on the next
rebase, not something to carry.)

**The sampled-image descriptor path is not where the mistyped binding is written.** A whole run
produces exactly three distinct (descriptor type, `VkImageViewType`, `MTLTextureType`) tuples, and
each is correct:

```
desc_type=1 vk_view_type=1 input=0 -> mtl_texture_type=2 samples=1 512x512      # 2D -> 2D
desc_type=2 vk_view_type=5 input=0 -> mtl_texture_type=3 samples=1 4096x4096    # 2D_ARRAY -> 2DArray
desc_type=1 vk_view_type=1 input=0 -> mtl_texture_type=4 samples=4 960x600      # 2D+4s -> 2DMultisample
```

KosmicKrisp does not implement `VK_EXT_descriptor_buffer` (checked, not assumed), so
`get_sampled_image_view_desc` really is the path a `vkUpdateDescriptorSets` binding takes.

## Where to look next

The remaining producer of a forced array type is `kk_image_view_init`'s attachment block: for any
color/depth-stencil view that is not already 3D or 2D_ARRAY it rewrites `view_layout.view_type` to
`VK_IMAGE_VIEW_TYPE_2D_ARRAY` and builds `mtl_handle_input` from that. Whether that handle, or an
argument-table binding outside the descriptor-set path, is what the blit shader dereferences is the
open question — and it is answerable by extending the same tuple log to every site that hands a
`MTLResourceID` to a shader, rather than by reading the source.

The third descriptor tuple above is worth keeping in view for a different reason: a
`COMBINED_IMAGE_SAMPLER` legitimately carries a 4-sample texture, and only one dumped fragment
shader declares `texture2d_ms`. A second shader reaching that texture would be the same class of
fault by a different route.

## Method rules this bug earned

- **A validator class that fires on healthy frames explains nothing.** Diff *instances* (pipeline
  UID + source line) between a failing and a passing half of the same run; classes are identical,
  instances are not. Note the converse trap too: a pipeline UID hashes `rasterSampleCount`, so
  every pipeline drawing into an MSAA target has an "AA-only" twin. Only a shader with no non-AA
  twin — a blit that exists because of MSAA — is evidence on its own.
- **Prove the mask is not the slowdown.** Validation costs ~10x; that a workload survives under it
  means nothing until an arm shows the mask working at full speed, or the fault returning with the
  slowdown kept.
- **Give it the full window, and do not read process liveness as health.** This bug kills between
  60 s and ~2 minutes, so a check at 150 s can still read healthy.
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

Then watch the worker log for `DEVICE_LOST`. The KK-side knobs used above —
`LIMINA_KK_ALLOC_DESTROY`, `LIMINA_KK_BO_LEAK`, `LIMINA_KK_VIEW_LEAK`, `LIMINA_KK_DESCLOG`,
`KK_LIMINA_SHADER_DUMP`, `LIMINA_KK_LABELS` — all live on the `limina-kk` branch of
`/Volumes/mesa-cs/mesa`.

**A fix must show all four:** the page for ≥5 minutes, no `DEVICE_LOST`, no nil views, **and the
cubes visibly rendering in the capture**. Black cubes with no fault is the validation mask, not a
fix.
