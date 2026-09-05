# A WebGL page that asks for antialiasing loses the Vulkan device and kills the VMM

**Status:** OPEN. The failing work is named — the guest's own 4-sample render passes — and the
failure is stochastic, which governs how every arm here must be run.
**Vehicle:** `webgl-msaa.html`, self-contained (no network, generated texture). Three textured
cubes on a `webgl` context, drawn as indexed `TRIANGLES` with `UNSIGNED_SHORT` indices; `?aa=0`
requests `{antialias:false}` for the control arm.
**One arm, end to end:** `run-arm.sh <name>` — clones a pristine image, boots it, launches the
page in the seated session, watches, and declares the arm VOID unless the browser exists *and* a
multisampled blit reached vrend.
**Backlog entry:** `docs/hardening-backlog.md` §"A guest WebGL page that requests MSAA loses the
Vulkan device and aborts the VMM".

## What happens

An `{antialias:true}` context — which is also what `getContext('webgl')` with no options gives,
since `antialias` defaults to true in the spec — loses the device, usually within a couple of
minutes:

```
MESA: error: ZINK: vkQueueSubmit failed (VK_ERROR_DEVICE_LOST)
[LIMINA-ALLOC-POOL] class 0 grew to 65 allocators — in-flight depth is outrunning completion
   ... thousands of growth lines ...
VM stopped — worker terminated by signal 6
```

The pool runaway is **downstream**, not a second fault: once the device is lost nothing completes,
so no allocator ever drains, and `kk_alloc_pool_get` mints on every request by design. The abort is
downstream again — it lands in Apple's `IOGPUMetalCommandBufferStorageAllocResourceAtIndex`,
reached via `cs_get_compute` ← `kk_dispatch_precomp` ← `kk_draw`, which is AGX refusing to allocate
with thousands of live `MTLCommandAllocator`s outstanding. It is **not** zink's device-lost abort:
that is gated on `ZINK_HANG_ABORT`, default false.

`{antialias:false}` runs indefinitely with the frame still animating.

## MSAA is genuinely taken, which is new

The page prints what the driver **granted**, not what it asked for: `granted aa=true SAMPLES=4
SAMPLE_BUFFERS=1`. So the long-documented Firefox behaviour — MSAA backbuffer reports
`FRAMEBUFFER_INCOMPLETE_ATTACHMENT`, Firefox silently falls back to non-AA, cosmetic only — **does
not hold on the current KosmicKrisp stack.** That finding was measured on the retired MoltenVK
backend and was never re-measured on KK.

This is why the failure went unnoticed: pages that never take the MSAA path are fine, and until
MSAA started actually working there was no MSAA path to take. Any A/B here **must** read
`getContextAttributes().antialias`; "requested AA" is not evidence AA happened.

## The failure is stochastic, and that is the load-bearing fact

Four consecutive arms on the same build, same clone, same page, same resolve on the wire:

| arm | client | outcome |
|---|---|---|
| ssh-launched kiosk, fresh VM | sole | ~12 min, 16,512 AA frames, **survived** |
| session-launched kiosk, fresh VM | sole | 6 min, 23,456 AA frames, **survived** |
| session-launched kiosk, same VM after the above was stopped | second | lost, ~2 s after the transition |
| session-launched kiosk, fresh VM | sole | lost, ~25 s after its first resolve |

The last row refutes the row above it: a sole first client on a fresh VM both survives six minutes
and dies in twenty-five seconds, with nothing in the configuration separating them.

That kills every one-arm discriminator this investigation produced — display size, canvas size,
fullscreen, the Activities overview, the launch method, a preceding client's teardown, and the
"+66 to +85 s schedule" the archived logs seemed to show. It also disposes of the
launch-environment theory by measurement: dumped from `/proc/<pid>/environ`, the two launches are
**byte-identical**, twenty variables, empty `diff`.

**A single arm decides nothing here.** A hypothesis needs a survival rate over k repeats per side,
and the archived table below records outcomes that were read from single arms — treat every "both
die" in it as weak.

## What the GPU was actually doing

KosmicKrisp records every command buffer as it closes and marks the failing commit's own entries in
the device-loss report (`kk_limina_work_record`, dumped from `commit_callback`). Six arms, six
losses — five of them the unchanged configuration, one with the barrier scope widened — and the
marked work is the same every time:

| arm | Metal error | the failing commit held |
|---|---|---|
| probe1 | Hang, 86.0 ms | 3x `render 1280x720 s4 rts1 fmt37` |
| r1 | PageFault, 5.7 ms | 3x `render 1280x720 s4 rts1 fmt37` |
| r2 | Hang, 510 ms | 1x `render 1280x720 s4 rts1 fmt37` |
| r3 | PageFault, 6.4 ms | 3x `render 1280x636 s4 rts1 fmt37` |
| a1 | Hang, 46.1 ms | 3x `render 1280x720 s4 rts1 fmt37` |
| w1 | Hang, 46.9 ms | 3x `render 1280x720 s4 rts1 fmt37` |

`fmt37`/`fmt44` are `VK_FORMAT_R8G8B8A8_UNORM` and `B8G8R8A8_UNORM`. A frame is three 4-sample
RGBA8 passes, a compute buffer, then one single-sample BGRA8 pass at the same size — that last one
*is* vrend's shader blitter doing the resolve — and separately the compositor's own `2560x1440 s1`
passes:

```
      911  compute ops=2
 *    912  render 1280x720 s4 rts1 fmt37 c=0x862c61180 a=0x1547fe8000 d=130 ops=5
 *    913  render 1280x720 s4 rts1 fmt37 c=0x862c61180 a=0x1547fe8000 d=130 ops=5
 *    914  render 1280x720 s4 rts1 fmt37 c=0x862c61180 a=0x1547fe8000 d=130 ops=5
      915  compute ops=1
      916  render 1280x720 s1 rts1 fmt44 ops=5      <- the resolve
      919  render 2560x1440 s1 rts1 fmt44 ops=40    <- the compositor
```

**The faulting commit holds only multisampled passes — the guest's own antialiased rendering.
The resolve is never in it, and neither is the compositor.** The resolve is what *made* the content
multisampled and what led us to the shape, but the work the GPU dies on is the render into the
4-sample target, not the copy out of it. Both Metal error classes agree, which also retires the
idea that `Hang` and `PageFault` are two different bugs.

### The attachments are not dangling

`LIMINA_KK_ADDR_LOG=1` prints every heap-backed BO and every image plane at create and destroy with
its `[gpu, gpu+size)` range, stamped with the work sequence. In the failing arm both 4-sample
attachments — colour at `0x1547fe8000` (15,073,280 B, `1280x720 s4`) and its `D32_SFLOAT_S8_UINT`
companion at `0x1548e50000` — are allocated once, ~1,200 command buffers before the loss, and are
**never freed**; there is no `img-` or `bo-` event anywhere near the failing sequence range. The
render digest carries the attachment address for exactly this comparison, and it is the same
address in every frame of the run, healthy and failing alike.

So the leading "dangling attachment" reading is out. What the pass reaches *besides* its
attachments is where this has to go next.

### What the kernel says a fault was

`/Library/Logs/DiagnosticReports/gpuEvent-limina-vmm-*.ips` carries what Metal's error object does
not: `restart_reason_desc` and, for a page fault, the faulting VA. Across a day of arms, **every
fault is a read**, at page-table level 1 or 2, and the addresses are scattered across the whole
GPU VA space (`0x93bebcd440`, `0x252f204400`, `0xaaa79d000`, `0x3b9f87d000`, …). A dangling
attachment would fault at a clustered address; a scattered read fault is what an out-of-range
*fetch* looks like — an index or descriptor scaling into an arbitrary address. `restart_reason`
splits with the Metal error class: `BIF0 page fault` for PageFault, `MMU interrupt` for Hang.

These reports are written by an analytics daemon on its own schedule and did not appear at all
after a reboot in one session, so they are an oracle to check *later*, never to wait on.

## What the failing pass actually is

The digest names the pass's draws, so the failing work is read rather than inferred:

```
 *    959  render 1280x720 s4 rts1 fmt37 c=0xaae159e00 a=0x15494b8000 d=130 ops=5 draws=1 unroll=0
 *    960  render 1280x720 s4 rts1 fmt37 c=0xaae159e00 a=0x15494b8000 d=130 ops=5 draws=1 unroll=0
 *    961  render 1280x720 s4 rts1 fmt37 c=0xaae159e00 a=0x15494b8000 d=130 ops=5 draws=1 unroll=0
      963  render 1280x720 s1 rts1 fmt44 ops=5 draws=1 unroll=1     <- the resolve
      966  render 2560x1440 s1 rts1 fmt44 ops=40 draws=11 unroll=11 <- the compositor
```

**One indexed draw per pass, and none of them unrolled.** The failing work is a plain indexed draw
into a 4-sample RGBA8 target with a `D32_SFLOAT_S8_UINT` companion, reading the guest's own index
and vertex buffers. Three such passes per frame, one cube each.

That matters because the rest of the guest's GL traffic looks nothing like it. Per ten seconds KK
records `unroll_geometry calls=2561`, all issued mid-pass through the `pre_gfx` route, allocating
from a device-wide 128 MiB bump heap whose pointer is reset once per command buffer — a genuinely
suspicious arrangement, and the one this investigation spent four arms on. **It is not the failing
work.** The unrolls belong to the compositor and to vrend's resolve; the antialiased draws that
kill the device never touch that heap. `KK_LIMINA_HEAP_NORESET=1` accordingly does not stop the
loss, and the heap's bump pointer reads 0 in the report.

Nor is it an unclamped vertex fetch. `LIMINA_KK_FORCE_ROBUST=1` lowers robustness2 vertex-attribute
clamping into every pipeline whatever it asked for, and the device is still lost — though note that
this clamps the *attribute* fetch only: a garbage `firstIndex` or `indexCount` is not covered, so
this does not by itself retire out-of-range indices.

### Nothing concurrent, and no bad address

`LIMINA_KK_SERIALIZE_SUBMIT=1` commits one Metal command buffer at a time and blocks until its
completion callback fires — no timeouts, so the wait really held. **The device is still lost.** The
faulting work is therefore one render pass, one draw, alone on the GPU. That retires the whole
family of inter-commit races at once: the poly heap's reset, the `pre_gfx`/`gfx` split,
upload-pool and allocator reuse, a residency removal or a released texture landing under work
already in flight. Whatever the GPU reads badly is put there by the CPU before the pass is
committed.

`LIMINA_KK_ADDR_CHECK=1` looks up every address the draw hands the GPU — root table, index buffer,
the first two vertex bindings — in the BO address registry at encode, with a destroyed BO's entry
dropped so a freed range reads as unknown. On a losing arm it **reports nothing**: every address
the draw binds is live.

`LIMINA_KK_TEX_LEAK=1 LIMINA_KK_VIEW_LEAK=1` makes a stale resource ID unrepresentable — no
texture and no view is ever released — and the device is lost in **4 arms of 4**. A dead-ID scan
agrees from the other side: KK remembers every `MTLResourceID` that dies with its view and walks
the memory of every descriptor set a multisampled draw binds looking for one, and on a losing arm
it finds none.

### The fragment shader's texture sample is required

So the driver-side levers are exhausted, and the workload itself is where the ingredient is.
`?notex=1` swaps the fragment shader for one that returns a gradient and samples nothing,
changing nothing else — same three passes, same 4-sample RGBA8 target, same depth companion, same
resolve on the wire, `granted aa=true SAMPLES=4`.

**It survives, 22,880 frames with the cubes visibly rendering in the capture.** One arm so far, and
one arm decides nothing here — but it is the first lever in this investigation that changed the
outcome at all, and it is on the workload rather than in the driver.

That converges with the one genuine Vulkan violation already in the descriptor log: a
`COMBINED_IMAGE_SAMPLER` naming a `VK_IMAGE_VIEW_TYPE_2D` view of a **4-sample** image, so a
shader declaring `texture2d<float>` samples an `MTLTextureType2DMultisample`. Metal's 2D and
2DMultisample layouts differ, so that read misaddresses — a scattered read fault at an address
with nothing to do with the draw, which is what the kernel reports every time.

## The blit, and how the route was read

`LIMINA_VREND_TRACE=256` on the killing configuration: in a 31-second window, 3,220 draws, 248
framebuffer changes, and **134 BLITs, every one the same shape**:

```
src  res=1209  1280x720  nr_samples=4  format=67  bind=0xa
dst  res=1217/1218/…     1280x720  nr_samples=0  format=1  bind=0x10000a
```

An explicit multisample resolve, one 4-sample colour texture into each of six rotating
single-sample destinations carrying the shared/scanout bind bit. The whole guest run contains
exactly two multisampled resources: that colour target and its 4-sample depth companion.

**Source and destination formats differ, and that decides the route.** On a GLES host,
`vrend_renderer_prepare_blit` returns false for an MS-source RGBA blit whose
`src.format != dst.format`, so the blit falls through to `vrend_renderer_blit_gl`, the shader
blitter, in its own GL context (`third_party/virglrenderer/src/vrend/vrend_renderer.c:12751-12764`,
dispatch at `:12965-12973`). Read directly from the renderer with `LIMINA_VREND_BLIT_LOG=1`:

```
FBO   src=fmt67/s0 2560x1440 -> dst=fmt67/s0 2560x1440  (redblue_or_fmt=0 …)
GLFB  src=fmt67/s4 1280x720  -> dst=fmt1/s0  1280x720   (redblue_or_fmt=1 …)
```

The desktop's compositing blit stays on the FBO path; every MSAA resolve takes the shader blitter.
Since the failing commit never contains the resolve, this now describes *how the content gets
multisampled*, not the fault.

**`LIMINA_VREND_FORCE_FBO_BLIT=1` returns no reading.** Forced onto the FBO path the device dies
within a second — but forcing it is itself invalid usage (an MS-source `glBlitFramebuffer` between
differing formats is what the predicate exists to refuse), so its fault may be the knob's own. A
readable version has to make the blit legal rather than merely allowed: view the destination as
fmt 67 through `vrend_make_view`.

**The probes measured a different path.** `host-msaa-loop.c` and `guest-msaa-present.c` both use
`EXT_multisampled_render_to_texture` — implicit MSAA, zink's shadow-attachment emulation, no
`VIRGL_CCMD_BLIT` at all. Their negatives constrain that path and say nothing about this bug.

## What the validator's reports do and do not say

The dominant report class is a texture-type mismatch (`MTLTextureType2DArray bound … expected
MTLTextureType2D`). It is **chronic** — it fires on many pipelines in every run, a healthy desktop
included — **bidirectional**, and carries no texture identity, address or binding index, so a
report alone can never name the resource that produced it. Nothing in this class currently names a
faulting access.

One write in the descriptor log is a genuine Vulkan violation rather than a curiosity: a
`COMBINED_IMAGE_SAMPLER` naming a `VK_IMAGE_VIEW_TYPE_2D` view of a **4-sample** image, so a shader
declaring `texture2d<float>` samples an `MTLTextureType2DMultisample`. Metal's 2D and
2DMultisample layouts differ, so that read is a genuine misaddress. It is **not** yet tied to the
loss.

**Pipelines are self-identifying.** KosmicKrisp labels each `MTLRenderPipelineState` with a hash of
its generated MSL, and `KK_LIMINA_SHADER_DUMP` names its files by the same hash. The label must be
content-derived: it is hashed into the UID Metal reports against, so a per-run pointer would make
every UID per-run too.

## Arms run

Everything above the rule is a rate over repeats; everything below it was read from single arms and
is weak by the stochastic finding.

| arm | repeats | result |
|---|---|---|
| baseline, unchanged configuration | 5 | 5 lost |
| `KK_LIMINA_HEAP_NORESET=1` — the shared bump heap never recycled | 4 | 1 survived (226k unrolls), 3 lost |
| `LIMINA_KK_FORCE_ROBUST=1` — every vertex fetch clamped to its range | 3 | 1 survived, 2 lost |
| `LIMINA_KK_SERIALIZE_SUBMIT=1` — one command buffer on the GPU at a time | 2 | 2 lost |
| `LIMINA_KK_ADDR_CHECK=1` — every bound address looked up at encode | 1 | lost, **nothing reported** |
| `LIMINA_KK_TEX_LEAK=1 LIMINA_KK_VIEW_LEAK=1` — no texture or view ever released | 4 | 4 lost |
| `?notex=1` — the fragment shader samples nothing | 1 | **survived**, 22,880 frames |
| `KK_LIMINA_BARRIER=widen` (pre_gfx barrier scope ALL) | 1 | lost |
| --- | | |
| KK revision: pinned `552edc3f62f` vs two commits older | 1 each | both die |
| scanout path: windowed vs `--display-capture` | 1 each | both die |
| guest tier: stock F44 vs enhanced F44 | 1 each | both die |
| context attributes: AA vs `{antialias:false}` | 1 each | only AA dies |
| `LIMINA_ZINK_NO_FANS=1` | 1 | dies |
| `LIMINA_KK_ALLOC_DESTROY=0` (34 retirements → 0) | 1 | dies |
| `LIMINA_KK_BO_LEAK=1` — nothing released or de-resident | 1 | dies |
| `LIMINA_KK_VIEW_LEAK=1` — no view released | 1 | dies |
| nil texture views (`mtl_new_texture_view_with` → nil) | 3 | zero |
| Metal render-pass resolve (upstream `db5ab8de776`) | 1 | **never runs** |
| the zink shadow blit alone, no VM | 1 | **no mismatch** |
| `host-msaa-loop`, 2560x1440, 4 samples, no VM | 1 | 20,520 frames, survives |
| the same, `--churn 30` | 1 | 30,712 frames, survives |
| `msaa-loop` in the guest over virgl, MSAA granted | 1 | 226,909 frames, survives |
| `guest-msaa-present --fullscreen --mode surface` | 1 | 144,037 frames, survives |
| `guest-msaa-present --fullscreen --mode msrtt` | 1 | 139,000+ frames, survives |
| Firefox's native compositor (`gfx.webrender.compositor=false`) | 1 | dies in 65 s |

**No Vulkan resolve is involved, in either implementation.** Upstream replaced KK's meta/shader
resolve with Metal's native render-pass resolve (MR 43216); ported onto `limina-kk` it does not
help, and instrumenting it shows why: `kk_attachment_do_renderpass_resolve` never sees an
attachment with `resolve_mode != VK_RESOLVE_MODE_NONE` on this workload. zink emulates
`EXT_multisampled_render_to_texture` with a `util_blitter` draw, so the multisample traffic is
ordinary rendering to and sampling from a 4-sample texture.

## Where to look next

1. **The bindless texture read.** The attachments are alive, the draw is not unrolled, and vertex
   fetch clamping does not help — so of everything a single indexed draw touches, the descriptor's
   texture `gpuResourceID` is the one that can outlive what it names. Log image-view create and
   destroy with their sampled/storage resource ids alongside the existing `img+`/`img-` lines, and
   run a "no texture ever released" arm the way `LIMINA_KK_BO_LEAK` did for buffers.
2. **Get the faulting VA and the allocation log into the same frame.** `LIMINA_KK_ADDR_LOG=1` plus
   the `.ips` VA names the resource outright. The log is in place; the reports arrive late.
3. **Forward zink's debug labels to the Metal encoder.** KK advertises `EXT_debug_utils` but only
   labels encoders for capture; `MESA_TRACE=markers` makes zink emit `blit_resolve(...)` labels,
   and passing them through would make the report name the operation.

Arms no longer worth running: anything varying display size, canvas size or fullscreen.

## Method rules this bug earned

- **One arm decides nothing when the failure is stochastic, and you will not know it is stochastic
  until you repeat an arm.** Every discriminator here was tried once per side and each looked
  decisive until the same configuration produced the opposite outcome.
- **The workload must be shown running before a survival is read.** A survival is the reading a
  broken arm produces for free: no browser, no session, wrong driver, all look like health.
  `run-arm.sh` therefore checks the browser process *and* a multisampled blit on the wire, and
  exits 75 (VOID) if either is missing. Two arms in one session reported "survived 361 s" with no
  browser at all.
- **`sshd` answering is not "the desktop is up".** Wait for `/run/user/1000/wayland-0` before
  launching a client into the session.
- **Never edit a script while it is running.** bash re-reads from a byte offset; an edit mid-run
  corrupts the rest of the arm (`line 76: e: command not found`).
- **Copy the capture before killing the VM.** Killing the supervisor shuts the guest down, and the
  capture is overwritten once a second, so the file left behind otherwise shows systemd stopping
  units — for a survival and a death alike.
- **A validator class that fires on healthy frames explains nothing.** Diff *instances* between a
  failing and a passing half of the same run, and check the halves are what they claim.
- **Relaunching a browser does not reset it.** Session restore makes the second half run both
  pages; give each half `--profile <dir> --new-instance`, and create the directory.
- **Never change two variables to make an arm cheaper.** Dropping the display size to make
  validation affordable silently moved the run to a configuration that does not crash.
- **A label can perturb what it names.** A pipeline label is hashed into the pipeline UID.
- **A liveness check must not match itself.** Bracket the pattern (`'[l]imina-vmm'`), and confirm a
  survival against the worker log, not against the watcher that was supposed to notice.
- **Read the size the workload actually got.** A fullscreen kiosk on a 2560x1440 display at scale 2
  is 1280x720 logical; a table built without the drawing-buffer size inverted its conclusion.
- **Prove the mask is not the slowdown.** Validation costs ~10x, and full shader validation is not
  survivable on this workload for more than a couple of minutes (the slowed GPU makes the allocator
  pool run away and the worker aborts). Use selective validation.

## Reproducing

```
LIMINA_KK_ADDR_LOG=1 spikes/webgl-msaa/run-arm.sh <name>
```

That is the whole arm: it clones `Fedora-Workstation-44.enhanced.test.raw`, boots it at
`LIMINA_RAM_MIB=3072 LIMINA_CPUS=4 --display-size 2560x1440`, waits for the session, launches the
page through `systemd-run --user`, watches for `LIMINA-DEVICE-LOST`, and prints the report with the
failing commit's command buffers marked. Artefacts land in `/tmp/webgl-msaa-<name>/` (worker log,
live capture, `vm_stat` either side). Exit 75 means VOID, not survived.

The KK-side knobs — `LIMINA_KK_ADDR_LOG`, `KK_LIMINA_HEAP_NORESET`, `KK_LIMINA_BARRIER`,
`LIMINA_KK_ALLOC_DESTROY`, `LIMINA_KK_BO_LEAK`, `LIMINA_KK_VIEW_LEAK`, `LIMINA_KK_DESCLOG`,
`KK_LIMINA_SHADER_DUMP`, `LIMINA_KK_LABELS` — all live on the `limina-kk` branch of
`/Volumes/mesa-cs/mesa`, and each prints a line when engaged. Read that line: an arm whose lever
cannot be observed in the log is worse than no arm.

**Survival happens by chance — about one arm in four.** A single survival is not a result and a
rate under five is barely one. Budget three arms per side, and prefer an instrument that reads the
failure deterministically over an A/B that needs repeats.

**Keep the guest small — the arms poison the host, whatever their outcome.** Compressor occupancy
doubled per arm (203k → 406k → 810k → 1,014k pages) on 8 GiB guests and a day of arms panicked the
machine on compressor-segment exhaustion; at 3 GiB it still grows by 40k pages on a short losing
arm and **208k on a full six-minute survival** — the cost tracks how long the VM ran, not how it
ended, and a survival is the *expensive* outcome. Budget about four arms per reboot, record
`vm_stat` around each (`run-arm.sh` does), and ask for a reboot at ~900k pages occupied.

**A fix must show all four:** the page for ≥5 minutes across repeats, no `DEVICE_LOST`, no nil
views, and the cubes visibly rendering in the capture. Black cubes with no fault is the validation
mask, not a fix.
