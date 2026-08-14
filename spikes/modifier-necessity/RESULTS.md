# Are the DRM format/modifier patches actually needed?

**2026-08-04.** Pressure test, in the spirit of the blob-scanout fence (which we carried on
inference for a day and then measured at 86% of frames lost). The question: of the patches we
carry to make DRM formats and format modifiers work across the stack, which are load-bearing,
which are redundant residual, and which are dead code?

The patches, and why they are one story:

| tier | patch | what it does |
|---|---|---|
| guest kernel | linux 0002 | widen the virtio-gpu primary plane format list beyond XRGB8888 |
| guest kernel | linux 0003 | advertise `DRM_FORMAT_MOD_LINEAR` on planes (`IN_FORMATS`) |
| guest mesa | mesa 0010 | force-advertise `VK_EXT_image_drm_format_modifier` in venus; fabricate a LINEAR modifier |
| host | virgl 0005 | unlink the modifier structs and normalize `DRM_FORMAT_MODIFIER_EXT` tiling on scanout images |
| host | kk 0002 | lay a modifier-tiled *attachment* image out tiled, not linear (else Metal can't make a render encoder → nil-encoder assert → worker SIGABRT) |

Two independent axes hide in that list, and testing them together would let one mask the other:

- **Axis 1, KMS.** linux 0002 + 0003 decide whether a guest compositor engages *direct scanout*
  at all. This is a performance question, and the gnome-shell-rs rig is the instrument.
- **Axis 2, Vulkan.** mesa 0010 → virgl 0005 → kk 0002 is a chain rooted in venus advertising an
  extension the renderer never negotiated. This is a *does venus work at all* question.

## Axis 2 — method: instrument, don't amputate

Removing a patch and seeing what breaks answers "is it load-bearing" with a crash. Counting how
often its code path is *entered* answers the same question, plus "how much of it is live traffic",
and costs one boot instead of one rebuild per arm. Both host patches got a hit counter; the virgl
one additionally got two env gates so its behaviour could be switched at runtime:

- `LIMINA_VKR_KEEP_MODIFIER_TILING=1` — skip normalizing `DRM_FORMAT_MODIFIER_EXT` → `OPTIMAL`
- `LIMINA_VKR_KEEP_MODIFIER_STRUCTS=1` — leave the modifier LIST/EXPLICIT structs chained
- `LIMINA_KK_NO_MODIFIER_TILED=1` — turn kk 0002's carve-out off

Diffs against the gitignored trees are kept here as `virgl-vkr-modifier-gates.patch` and
`kk-image-layout-probe.patch`.

Vehicle: `Fedora-Workstation-44.enhanced.test.raw` clone, EFI + venus + KK, boot to a seated GNOME
session, launch gnome-terminal and nautilus, run vkmark.

## Axis 2 — results

| arm | env | `[LIMINA-VKRMOD]` | `[LIMINA-KKMOD]` | session |
|---|---|---|---|---|
| baseline | — | 4 (3 LIST alloc, 1 EXPLICIT import) | **0** | seated, venus live, vkmark OK |
| tiling gate | `KEEP_MODIFIER_TILING=1` | 3 | **0** | seated, venus live, vkmark OK |
| struct gate | `KEEP_MODIFIER_STRUCTS=1` | 4 | **0** | seated, venus live, vkmark OK |

### kk 0002 is unreachable, and not by accident

Zero hits in every arm — including the arm that deliberately stops virgl from normalizing the
tiling. The mechanism, read from `vkr_image.c` rather than guessed: on the KosmicKrisp path
(no `VK_EXT_metal_objects`), any scanout-capable format gets

```c
mci->tiling = VK_IMAGE_TILING_LINEAR;   /* vkr_image.c, the KK branch */
```

**unconditionally**, after the modifier normalization. So the tiling gate is shadowed — a
scanout-format image can never arrive at `kk_image_layout_init` still carrying
`DRM_FORMAT_MODIFIER_EXT`. The carve-out is reachable only by a modifier image with *no*
external-memory struct (which skips the whole vkr block) or one whose format is not
scanout-capable. Neither occurs in a real session.

That is a stronger statement than "unused today": the crash class kk 0002 was written for (June
2026, nil render encoder → `kk_encoder.c` assert → worker SIGABRT) was closed *upstream of it* by
the vkr KK winsys work. It is dead code. It is also a guard against a guest-triggerable worker
abort, so the honest framing is: **the shadowing is what retired it, and it returns the moment
vkr stops forcing LINEAR.** Whatever disposition it gets, that condition belongs in the note.

> **That prediction was tested on 2026-08-04 and is wrong** — instructively. The symmetric
> MTLTEXTURE build *does* stop forcing LINEAR, and kk 0002 still counted **0 hits** against 4
> modifier images reaching vkr. The reason: vkr now assigns `VK_IMAGE_TILING_OPTIMAL` explicitly
> on both the export and import branches, so the shadow **moved rather than lifted**. kk 0002
> becomes reachable only if vkr stops normalizing the tiling *at all*, which no shipping
> configuration does. Re-read as: the carve-out is dead under any tiling normalization, not
> specifically under LINEAR.

### …but only because *our own* forced LINEAR shadows it — and that is load-bearing

The shadowing above rests on `vkr_image.c`'s KK branch forcing LINEAR, which is **our**
customization (virglrenderer 0011), not upstream behaviour — so it is fair game for this test too.
Gated off (`LIMINA_VKR_NO_KK_FORCE_LINEAR=1`), the picture inverts: **kk 0002 fires 3 times**, KK
lays the modifier-tiled attachments out as heap-backed textures, and the guest renders without an
abort. So kk 0002 is not dead weight — it is precisely the guard that makes a non-forced-LINEAR
world work, i.e. the enabling half of the architecture we would move to.

What breaks in that arm is the **present** side, not the render side:

```
SET_SCANOUT_BLOB scanout=0 res=8 not IOSurface-backed; using readback
transfer_read failed for scanout resource 8 (blob/3D, no 2D readback): error 22
```

vkmark *rose* to 389 (from ~250) — the tell, not good news: the guest renders freely because
nothing is being presented. The user confirmed the window frozen on an old frame.

**Why the forced LINEAR exists** — and it is not about modifiers at all. It is a *pitch*
requirement: we can only alias host memory we can describe as a flat buffer with a known stride,
so we make the image linear, ask KK for its `rowPitch`, and allocate an IOSurface with exactly
that pitch so the bytes back the image (`vkr_image.c:215-244`). A tiled Metal texture has an
opaque layout, hence no pitch, hence no aliasing. It even has a live failure mode: when IOSurface
overrides the pitch for alignment, the code drops the surface and falls back to the readback that
we just watched fail.

**The exit, and it is entirely ours.** Twenty lines up, the MoltenVK branch chains
`VkImportMetalIOSurfaceInfoEXT` and never forces LINEAR — MVK can build a VkImage over an existing
IOSurface, so the pitch problem never arises. KK forces LINEAR only because it had no equivalent
import. We own KosmicKrisp. See "Follow-on work" below.

### virgl 0005 is live traffic, but mostly redundant residual

The counter proves the path is real — 4 modifier-tiled images per session, the scanout buffers.
But the two halves fare differently:

- **The struct unlink is not load-bearing against KK.** With the modifier structs left chained
  straight through to a driver that never advertises the extension, the session came up seated
  with venus live and vkmark rendering. Vulkan requires drivers to ignore unrecognized `pNext`
  structs, and KK does. The unlink was written for MoltenVK, which is retired.
- **The tiling normalization is shadowed** for exactly the images that reach it, by the
  unconditional LINEAR assignment quoted above.

So 0005's residual value is confined to modifier images that are *not* scanout-capable — a case
we did not observe. It is a shrink candidate, not a drop-on-sight.

## Axis 1 — KMS: the two kernel patches are a functional requirement, not a perf knob

`probe/no-modifiers` on `liminavm/linux` = the `limina` branch minus `1f4c2049` (widen the plane
format list) and `74ae69ad` (advertise LINEAR), built as `7.1.6-limina16knm`. Both 7.1.6 kernels
installed into one rig clone (`nirirepro.nm.raw`) and flipped with `grubby`, so disk, userspace,
host build and window set are held fixed.

| | arm C: `7.1.6-limina16k` (patches in) | arm D: `7.1.6-limina16knm` (patches out) |
|---|---|---|
| frames | 8351 | 47239 |
| missed vblanks | 111 = **1.33%** | 0 = 0% |
| elements (median) | 40 | 40 |
| **draws** | **27–57** | **0** |
| scanout | `1 scanout in 0.00ms` | absent |
| gpu | 1.26–4.05 ms | absent |

**Do not read that 0% as a win.** Arm D emits 5.6x the frames with *zero draws* and no scanout
line — it is not compositing instead of scanning out, it is producing empty frames as fast as it
can. You cannot miss a vblank you never aim for. The guest names the cause itself:

```
WARN niri::backend::tty: error rendering frame: present-blit target (DrmFourcc(XR24)):
this device does not support DRM modifier 0x00ffffffffffffff for B8G8R8A8_UNORM
(it enumerates others), so an image imported with it would be undefined
```

`0x00ffffffffffffff` is `DRM_FORMAT_MOD_INVALID`. With no LINEAR in the plane's `IN_FORMATS`,
niri has no modifier to name, falls back to MOD_INVALID, and its Vulkan renderer refuses the
import — **every frame fails**. Confirmed on the glass: the user reports the arm D window frozen
on an old frame.

**Verdict: carry, and upgrade the reason.** The ledger justified 0003 as an optimization that lets
compositors "engage direct scanout at all", with rubric item (b) claiming a stock guest merely
"composites fullscreen — correct, one extra guest-GPU pass per frame". For a Vulkan/async-scanout
compositor that graceful-degradation claim is **false**: there is no fallback, the session freezes.

Scope that correctly, though — it is not "no kernel patch means no desktop":

- This is the *enhanced* kernel minus the patch, driving a **Vulkan** direct-scanout compositor.
- A **stock** Fedora kernel also lacks these patches and stock guests work fine, because stock
  mutter renders through GL/EGL and never asks for an explicit scanout modifier. The two-tier
  guarantee is intact.
- So the precise statement is: **for the Vulkan-renderer compositor we are building toward, the
  LINEAR advertisement is a hard functional dependency.** That strengthens the case for carrying
  0003 and for making M15's host-negotiated modifiers its real exit.

**Not isolated:** 0002 and 0003 were removed together. The failure names a *modifier*, so 0003 is
almost certainly the whole cause, but a third arm (0002 out, 0003 in) would be needed to say so.
Left open deliberately rather than asserted.

### Arm E — fix the compositor instead, and the kernel patches become droppable (for 0003)

The user's push-back on the arm D verdict was the right one: niri's refusal is a *compositor*
policy, not a hardware limit, and we control that compositor. One line does it
(`niri-mod-invalid-linear-fallback.patch`): when the plane advertises no modifiers the dmabuf comes
back tagged `MOD_INVALID`, and every check and import in the Vulkan renderer reads a single
`modifier` binding — so labelling it LINEAR there is the whole change. Truthful here, because niri
already hard-asserts at startup that LINEAR carries the features it needs (LINEAR is all this
driver exposes). 41-second rebuild in the guest.

Arm E = the arm D kernel (neither patch) + that fix:

| | arm C (patches in) | arm D (patches out) | arm E (patches out + fix) |
|---|---|---|---|
| frames | 8351 | 47239 | 8339 |
| missed vblanks | 111 = **1.33%** | 0 (meaningless) | 88 = **1.06%** |
| render errors | 0 | every frame | **0** |
| elements (med) | 41 | 40 | 41 |
| draws (med / p90) | 35 / 131 | 0 / 0 | **45** / 131 |
| gpu (med) | 3.09 ms | — | **3.48 ms** |
| scanout | present | absent | present, every frame |

Comparability holds between C and E: same elements, same draws p90, frame counts within 0.1%.

**Frame pacing is not degraded.** 1.06% vs 1.33%, against arm C's own 1.2% the day before — that
spread is run-to-run noise, not signal.

**But the work per frame went up:** median draws 35 → 45 (+29%) and median GPU 3.09 → 3.48 ms
(+13%). That is the signature of content that used to reach the plane directly now being
composited — precisely what dropping the *format widening* (0002) predicts, since the plane falls
back to XRGB8888-only and niri's own scanout comment says an RGBA-order client then "falls back to
compositing". It costs no frames today because 3.5 ms sits well inside a 16.67 ms budget; it is
headroom spent, not free. **Caveat: arm E moves two variables at once** (kernel patches out *and*
compositor patched), so the draw/GPU delta is not cleanly attributed — the compositor change only
relabels a modifier and should add no draws, which points at the kernel side, but it is not
isolated.

**What arm E does NOT answer.** The rig workload is workspace/overview transitions — the
*compositor's own* present path. It never exercises a fullscreen client's buffer going straight to
the plane, which is what 0002 is really about. So this prices 0003 and leaves 0002 unpriced.

### Dispositions after arm E

- **0003 (LINEAR advertisement) — droppable**, at the cost of carrying the one-line fallback in our
  compositor. No pacing penalty measured. The trade is that only compositors we patch keep working
  well: stock mutter and upstream niri guests would lose the modifier path. Given the compositor
  replacement direction that may be exactly the right trade, but it is a product call, not a
  technical one.
- **0002 (format widening) — keep pending measurement.** Not priced by this rig, and the +29%
  draws / +13% GPU in arm E is a live hint that something stopped direct-scanning-out. The test
  that would settle it is a fullscreen client (ARGB8888 and an RGBA-order Vulkan swapchain) with
  0002 out and 0003 in, watching whether the client's buffer reaches the plane.

## Follow-on work: KK gains the Metal import, and the forced LINEAR retires

Decision (2026-08-04): implement **`VK_EXT_external_memory_metal`** in KosmicKrisp rather than
MoltenVK's `VK_EXT_metal_objects`. It is ratified, authored by LunarG (who write KK), and is
verbatim the convergence item in `docs/upstreaming/divergence-decisions.md` §A — aligning us with
virglrenderer !1617 (`VIRGL_RESOURCE_METAL_HEAP`) / !1618 (VMM-facing Metal-texture scanout) for
the *whole* macOS memory-sharing fork, not just scanout.

Better starting point than expected: KK **already advertises the extension** and implements import
and export — for `MTLHEAP` only (`kk_device_memory.c`, and the assert that pinned it there). So
this completes an extension KK already claims rather than adding one.

Done (builds clean, `kk-external-memory-metal-mtltexture.patch`): the `MTLTEXTURE` handle type —
bridge `mtl_texture_get_props`, `kk_bo.texture`, texture residency helpers, import in
`kk_AllocateMemory`, export in `kk_GetMemoryMetalHandleEXT`, `kk_mtltexture_mem_props`
(DEDICATED_ONLY), the external-image format query, and `kk_image_plane_bind` adopting the imported
texture verbatim after validating it against the image.

The vkr side is now wired too, behind `LIMINA_KK_MTLTEXTURE_SCANOUT=1`
(`virgl-kk-mtltexture-scanout.patch`), default off so the shipping path is untouched:

- `vkr_image.c` KK branch: allocate the IOSurface **before** create (no rowPitch needed), leave
  tiling `OPTIMAL`, strip `INPUT_ATTACHMENT` (it would push KK's layout type to 2DArray), and set
  `gkvm_surf` so the existing post-create block attaches it to the image. Falls back to forced
  LINEAR if the allocation fails.
- `vkr_device_memory.c`: chain `VkImportMemoryMetalHandleInfoEXT{MTLTEXTURE, surf->mtl_texture}`
  instead of the host-pointer import — the `VkMemoryDedicatedAllocateInfo{image}` linkage that
  finds the surface already existed.
- `vkr_metal_helpers.m`: the IOSurface texture gains `MTLTextureUsageShaderWrite`. KK derives its
  Metal usage from the guest's `VkImageUsageFlags`, and a scanout image carrying `TRANSFER_DST` or
  `STORAGE` maps to `SHADER_WRITE`; the bind requires the texture's usage to be a superset.

**A warning taken seriously, from `vkr_image.c`'s own comment.** The MoltenVK equivalent of this
path is recorded there as *proven dangerous*: importing a raw MTLTexture as the bound memory "uses
our texture verbatim and any usage/format mismatch silently no-ops the render, leaving the IOSurface
untouched (a magenta-prefilled scanout surface stayed pure magenta — the GPU never wrote it)". My
first validation checked extent/levels/layers but **not format or usage — the two things that
actually failed there**. It now checks pixel format, usage superset, sample count and texture type
as well, and fails the bind loudly, because a wrong texture here is invisible at every later layer.

### First end-to-end run: aborts, loudly (2026-08-04)

Booted `enhanced.test` with the gate on. The session came up seated, but running vkmark killed the
worker:

```
Assertion failed: (internal_encoder && internal_encoder->encoder),
  kk_encoder_internal_end_encoding, kk_encoder.c:226   → SIGABRT
```

A nil-encoder abort — the *same class* kk 0002 exists for, so the adopted texture is not yielding a
usable render encoder. Not yet root-caused. Note the vkr `vkr_log` lines are not emitted at the
default log level (the pre-existing "KK linear scanout" line does not appear either), so the gate
telemetry needs `VKR_DEBUG`-level logging or an `fprintf` before it can confirm which path ran.

Leads, in order of suspicion:

1. **`plane->addr = 0`.** The normal bind sets `plane->addr = mem->bo->gpu + offset`; a texture
   import has no buffer and hence no GPU address. If attachment or descriptor setup derives
   anything from `addr`, zero is wrong — this is the first thing to check.
2. **Whether the bind validation even ran.** `vk_errorf` may also be swallowed at this log level,
   so "no mismatch reported" is not evidence the texture matched. Confirm with a temporary
   unconditional `fprintf` before blaming anything downstream.
3. `layout.optimized_layout` under `OPTIMAL` tiling, which an IOSurface-backed texture cannot honor.

**Do not judge this by "the desktop came up" or by frame counters** — per the same recorded lesson,
the characteristic failure is a render that no-ops into an untouched surface. The acceptance test is
the magenta oracle: prefill the IOSurface with a known colour and prove the GPU overwrites it
(`spikes/venus-draw-probe/iosdump.swift`).

**Open question inherited from §A, unresolved:** our #28 root-caused upstream's fd double-mmap as
CPU/GPU-incoherent, yet upstream ships that path at ~1770 FPS on an M4 Max. Probe before
committing further.

### Root cause of the abort: sRGB folded onto the UNORM base (2026-08-04)

The `fprintf` probes (leads 2 and 3 above were the right instinct — the validation *was* running,
its complaint was just swallowed) printed the answer directly:

```
[LIMINA-KK-IMPORT] REJECTED MTLTexture (geom=1 fmt=0 usage=1): tex fmt=70 | image fmt=71
```

70 is `MTLPixelFormatRGBA8Unorm`, 71 is `..._sRGB`. vkr allocated the IOSurface through
`gkvm_vkformat_to_iosurface`, which deliberately folds an sRGB VkFormat onto its UNORM base — fine
when the surface is only ever aliased as raw bytes, wrong when the texture is adopted *verbatim* by
an image whose format is the sRGB one. The bind then rejected the texture, the plane kept no usable
texture, and the nil encoder surfaced two frames later at `kk_encoder.c:226`.

Fix: `gkvm_vkformat_to_mtl_exact()` in `vkr_image.c`, used only on the import path, which preserves
sRGB. `plane->addr = 0` (lead 1) turned out to be harmless — nothing on this path derives from it.

This is the failure mode the pre-existing `vkr_image.c` warning predicted, and it is worth
restating: **the adopted texture is used verbatim, so every property must match, not merely be
compatible.** The bind now checks extent, mip levels, array layers, sample count, texture type,
pixel format, and a usage superset — the last two being exactly the ones documented to fail
silently.

### The A/B: the import works, and it is perf-neutral (2026-08-04)

Same disk (`mtltex-arm.raw`, a clone of `enhanced.test`), same 6 vCPU / 8 GiB VM, rebooted between
arms, vkmark `-s 1280x720` on a quiet seated session:

| arm | scores | mean |
|---|---|---|
| `LIMINA_KK_MTLTEXTURE_SCANOUT=1` (adopt MTLTexture, `OPTIMAL` tiling) | 1501, 1514, 1517, 1569, 1495, 1515 | **1518** |
| gate off (our forced `LINEAR`) | 1535, 1534, 1513 | **1527** |

No direction, well inside run-to-run spread. **The import is a correctness/architecture change, not
a perf win** — and equally, the forced LINEAR is not costing anything vkmark can see.

Comparability is established rather than assumed: adoption counts went 3 → 16 across the three
gate-on runs, and the twelve new ones are `1280x720 type=2 fmt=71 usage=0x17 (image usage=0x6)` —
i.e. **vkmark's own swapchain images took the import path**, so the differential really did reach
the system under test. The other four are the 2560×1440 `fmt=80 usage=0x17` scanouts, whose image
usage matches the texture's exactly. Zero rejections, zero nil encoders, session alive throughout;
the desktop was human-confirmed live and updating, and subsequently ran WebGL aquarium at 20k fish.

**Retracted:** an earlier gate-on score of 1768 read as a 5–7× jump against a remembered "250–390
baseline". That baseline belongs to a different configuration; against the arm actually measured
here there is no speedup. Two later repeats (1025, 1792) were taken while the aquarium was running
and are discarded. The one number that survives is that both arms sit at ~1520.

### The rig arm: the compositing instrument agrees — still no difference (2026-08-04)

vkmark is not a compositing benchmark, so the question went to the gnome-shell-rs rig, which is
where LINEAR-vs-OPTIMAL scanout tiling would show up if anywhere. One disk (`nirirepro.work.raw`),
rebooted between arms, `NIRI_VK_ASYNC_SCANOUT=1`, `NIRI_FRAME_LOG=all,gpu`, 3840×2160, the same
three windows respawned in the same order, `heavy` profile, scored with `score-frame-log.py`:

| phase | arm | gpu p50 | flips | missed | rate |
|---|---|---|---|---|---|
| workspace transitions | MTLTEXTURE | 2.14 ms | 4173 | 0 | 0.00% |
| | forced LINEAR | 2.16 ms | 3866 | 0 | 0.00% |
| overview transitions | MTLTEXTURE | 7.10 ms | 4110 | 80 | 1.95% |
| | forced LINEAR | 7.13 ms | 4034 | 69 | 1.71% |
| **aggregate** | **MTLTEXTURE** | | 8283 | 80 | **0.97%** |
| | **forced LINEAR** | | 7900 | 69 | **0.87%** |

Both phases pass the scorer's element/bake comparability gate. The 0.10 pp miss-rate difference is
inside this rig's documented run-to-run spread (arm C reproduced as 1.33% and 1.2% on different
days), and GPU p50 is marginally *lower* on the MTLTEXTURE arm in both phases — i.e. the two arms
are indistinguishable at n=1 per arm, with no consistent direction.

Per the scoring rule earned by arm D, the draws were checked before the miss column: both arms
drew, the gate passed, and the gate-on worker log shows the compositor's 3840×2160 scanout plus all
three window surfaces adopting (`fmt=80 usage=0x17`, image usage identical), 0 rejections, 0 nil
encoders.

### The import was only half-done: every GTK4 window sheared (2026-08-04)

Booting the rig with the gate on and *looking at it* — which neither A/B above did — showed every
GTK4 client window sheared by a per-row offset, the classic stride skew. The compositor's own
background was pixel-perfect. Gate off: pixel-perfect everywhere. Screenshots via in-guest `grim`.

The asymmetry in the pixels named the bug exactly. The block the gate lives in fires for **every**
image carrying `VkExternalMemoryImageCreateInfo` — every shared buffer, not just the scanout, the
env var's name notwithstanding — and it only ever touched the **exporter** half:

- **Exporter** (a client's window buffer; modifier LIST form) got an `OPTIMAL` image whose memory
  is an adopted `MTLTexture`, i.e. content in the texture's native layout.
- **Importer** (the compositor sampling that window; modifier EXPLICIT form) is a different branch
  the gate never reached. It still aliased the same IOSurface bytes as
  `VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT` (`vkr_device_memory.c:323`) and read
  them **linearly at the importing image's own rowPitch**.

One end writes in the texture's layout, the other reads flat rows at a stride that no longer
describes it. The compositor's scanout was spared because it has no guest-side importer — which is
precisely why the background looked fine and made the bug look format-related rather than layout-related.

**This also corrects the standing understanding of what the forced LINEAR buys.** It is not merely
"a pitch requirement for aliasing host memory" that an MTLTexture import obviates. It is what keeps
the guest-visible stride and the host IOSurface rowBytes in agreement across *both* ends of a
shared buffer. Removing it on one end only is incoherent by construction.

### The symmetric import (2026-08-04)

The importer now adopts the *same* `MTLTexture` instead of aliasing bytes:

- `vkr_metal_helpers.m`: `vkr_mtl_texture_from_iosurface()` builds a retained `id<MTLTexture>` over
  an **existing** IOSurface, with a descriptor mirroring the alloc path's exactly (usage `0x17`,
  `MTLStorageModeShared`) so KK's superset-usage check passes.
- `vkr_image.h/.c`: `vkr_image` remembers its create `VkFormat`. The import is `DEDICATED_ONLY`
  (KK advertises the handle type that way), so `VkMemoryDedicatedAllocateInfo` names the image, and
  the image names the format the adopted texture must match — sRGB included.
  `gkvm_vkformat_to_mtl_exact()` is no longer static.
- `vkr_device_memory.c`: under the gate, a dedicated import with a resolvable IOSurface and an
  exactly-mappable format chains `VkImportMemoryMetalHandleInfoEXT{MTLTEXTURE}` instead of the
  host-pointer import. KK retains the texture, so vkr drops its own reference after the call.
- `vkr_image.c`: under the gate, an imported image stays `OPTIMAL` rather than being forced
  `LINEAR` — KK's bind refuses a LINEAR image for a texture import.

Result: pixel-identical to the gate-off capture. Telemetry shows the import half live
(`dedicated=1 vkfmt=44 mtlfmt=80`), 28 adoptions, 0 rejections, 0 nil encoders.

### The A/B, re-taken with both arms rendering correctly

The earlier numbers are **not** retracted: the compositor performed identical work in both arms, and
the shear was a read-offset, not a change in what was drawn. But they could not show a difference
that depends on layout, so the run was repeated on the fixed build — same disk, rebooted between
arms, same three windows, `heavy`:

| phase | arm | gpu p50 | flips | missed | rate |
|---|---|---|---|---|---|
| workspace transitions | MTLTEXTURE | 2.11 ms | 4180 | 0 | 0.00% |
| | forced LINEAR | 2.14 ms | 3910 | 0 | 0.00% |
| overview transitions | MTLTEXTURE | **6.47 ms** | 4053 | 51 | **1.26%** |
| | forced LINEAR | **7.22 ms** | 4035 | 79 | **1.96%** |
| **aggregate** | **MTLTEXTURE** | | 8233 | 51 | **0.62%** |
| | **forced LINEAR** | | 7945 | 79 | **0.99%** |

Both phases pass the comparability gate.

Repeated once more per arm (reboot between, same disk, same windows):

| run | MTLTEXTURE | forced LINEAR |
|---|---|---|
| #1 | 51 / 8233 = **0.62%** | 79 / 7945 = 0.99% |
| #2 | 4 / 8003 = **0.05%** | 75 / 7927 = 0.95% |
| combined | 55 / 16236 = **0.34%** | 154 / 15872 = **0.97%** |

The forced-LINEAR arm is remarkably steady across everything measured today — 0.87%, 0.95%, 0.99%
— and **both** MTLTEXTURE runs sit below the bottom of that range. The effect is real: roughly a
third of the misses overall, and in the better run almost none.

**But it is not "rendering got faster."** Overview GPU p50 went 6.47 vs 7.22 ms in run #1 and 7.02
vs 6.70 ms in run #2 — no consistent direction, so the earlier ~10% reading was noise and should
not be repeated. What improves is *punctuality*: the flip count barely moves while misses collapse.
That is consistent with the mechanism — an adopted texture removes the layout mismatch the importer
otherwise has to reconcile — but the causal chain has not been instrumented, only inferred from the
miss column, so treat it as the leading explanation rather than an established one.

The MTLTEXTURE arm's own spread is wide (0.62% vs 0.05%), so the *size* of the win is not pinned
down; its existence is.

### Verdict

The MTLTEXTURE import is **functionally complete on both ends** and renders correctly. The case for
retiring the forced LINEAR is architectural and strong — it deletes a limina-only customization,
lets shared images keep `OPTIMAL` tiling, and moves us onto the ratified
`VK_EXT_external_memory_metal` path §A identified as upstream's convergence direction. On the
symmetric build it is *also* measurably more punctual — 0.34% missed vs 0.97% over two runs per
arm, with both import runs below the forced-LINEAR arm's entire observed range — though GPU time
itself does not move.

Remaining before the forced LINEAR can actually retire:

- The import path is still gated off by default (`LIMINA_KK_MTLTEXTURE_SCANOUT`); flipping the
  default is a separate, reviewable change.
- Stock-tier behaviour is unprobed: a guest without our components must still scan out. The import
  is host-side only, so it *should* be tier-independent, but that is an assumption, not a measurement.
- Only the KK backend is covered. `vkr_image.c`'s fallback must stay for any non-KK host Vulkan.

### Rig trap: the driver picks the *first* niri socket, not the live one

New, and it fired here. `drive-workload.sh` resolves `SOCK=$(ls "$RUNTIME"/niri.wayland-*.sock |
head -1)` and then **re-exports `NIRI_SOCKET` itself**, so the env cannot override it. After a
`systemctl restart gdm` the runtime dir holds the dead session's socket *and* the live one, and
`head -1` takes the dead one. The run then reproduces the documented silent-no-op exactly: every
phase marker prints, the script exits 0, and nothing is driven — the tell is `BASELINE workspace=
windows=` (both empty) instead of `workspace=1 windows=3`. Fix: `rm` the stale socket before
driving. Always read the BASELINE line before trusting a run.

## The kk 0002 origin-class checkpoint (2026-08-04, evening): the nil-encoder class does NOT reproduce

Pre-code checkpoint from `docs/design/drm-format-modifier-for-real.md` §UNBLOCKED: can today's KK
produce a *renderable* linear layout for a modifier attachment image, or does the June 2026
nil-encoder abort (the incident kk 0002 was written for) come back when the carve-out is off?

Arm: `LIMINA_VKR_KEEP_MODIFIER_TILING=1` (modifier tiling reaches KK) +
`LIMINA_KK_NO_MODIFIER_TILED=1` (carve-out off → attachments go linear) +
`LIMINA_VKR_NO_KK_FORCE_LINEAR=1`, KK rebuilt with `kk-image-layout-probe.patch`, `modprobe-arm.raw`
(enhanced.test clone), EFI + venus, seated session.

Gate order matters and bit once: with only `NO_KK_FORCE_LINEAR`, vkr's *normalization*
(modifier → `OPTIMAL`) still runs and the carve-out is unreachable — `[LIMINA-VKRMOD]` fires,
`[LIMINA-KKMOD]` never does. `KEEP_MODIFIER_TILING` is what lets the image reach
`kk_image_layout_init` still modifier-tiled.

Result: 3 modifier attachments (`fmt=44` B8G8R8A8, 2560x1440, `usage=0x80097` incl.
COLOR_ATTACHMENT) logged `PASSTHROUGH(linear)`; gnome-shell composited into them for the whole
session; a full vkmark suite (13/13 scenes, score 1814 — inflated, nothing presents in this arm)
drove continuous compositor redraws. **Zero asserts, zero nil encoders, zero bind rejections,
worker alive throughout.** Present is broken as expected (no IOSurface aliasing in this arm — the
window freezes on the boot text; the user confirmed on the glass), which is the arm's known cost,
not a probe signal.

**Verdict: for the color-attachment class this whole design is about, KK genuinely can lay out and
render LINEAR.** The June incident's exact trigger remains unidentified (predates the vkr KK winsys
work; possibly the INPUT_ATTACHMENT→2DArray type interaction, since vkr strips that bit on its
LINEAR paths), but it does not reproduce for the observed format/usage on today's KK. Consequence
for `docs/design/drm-format-modifier-for-real.md`: the carve-out can narrow — advertise LINEAR for
IOSurface-shareable color formats with confidence; keep depth/stencil out of the modifier tables
entirely rather than keeping the tiled carve-out for them.

## IMPLEMENTED (2026-08-04, evening): KK's VK_EXT_image_drm_format_modifier, LINEAR-only

The design's task #2, written the same evening the checkpoint above unblocked it. On the mesa
fork's `limina-kk` branch: `0529c9bb766` (vulkan/runtime: compile the extension's runtime support
on Darwin — the `vk_image.h` guard the BLOCKED memo priced as a blocker is a 4-line widening) +
`befa0f2731e` (kosmickrisp: the implementation — format-properties modifier lists, image-format
validation, LIST/EXPLICIT create parsing with explicit-pitch adoption, truthful MEMORY_PLANE_0
layouts, linear-images-never-array types, and the kk 0002 carve-out retired per the checkpoint).

Acceptance: `kk-modifier-probe.c` (here) drives the ICD directly per the recorded dispatch traps
(dlopen + `vk_icdGetInstanceProcAddr`; really submits, since KK replays at submit). All green:

- enumeration, `[LINEAR]`×1-plane list with COLOR_ATTACHMENT features, depth offers nothing
- accept LINEAR+attachment usage; reject a bogus modifier and a depth format
- LIST create at 250×131 — **reported rowPitch 1008 where the guest-side fabrication (mesa
  0010(b)) computes 1000**: a live instance of the stride-divergence class that sheared GTK4,
  eliminated by construction now that the allocator answers the query
- the June nil-encoder class run for real: a render pass cleared the linear modifier image and
  the CPU readback **at the reported pitch** saw the clear color in all four corners (memory
  prefilled 0x77, so a no-op render would have been caught)
- EXPLICIT create round-trips the adopted pitch; a bogus pitch fails with
  `VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`

Regression: same build, EFI+venus seated boot, no gates — no aborts, no `degrading`, vkmark
13/13, desktop human-confirmed live. (Guest behavior is unchanged by design: 0010(b) still owns
the extension in-guest until it is deleted; vkr still normalizes tiling until its own change.)

## IMPLEMENTED (2026-08-04, night): vkr passes modifier creates through — the rewrite era ends

Task #3. virgl fork `limina` branch `0cc513fd` + mesa `limina-kk` `d918b98d869`. On the KK path a
modifier-tiled create now reaches the driver **verbatim** (structs chained, tiling intact,
INPUT_ATTACHMENT kept — safe because KK keeps linear images non-array now); the old
normalize/force/strip survives only for MoltenVK and pre-modifier drivers, keyed on the runtime
`EXT_image_drm_format_modifier` flag vkr now detects. LIST/exports reuse the post-create
query-pitch → IOSurface-at-exactly-that-pitch machinery; EXPLICIT/imports are validated and
adopted by KK itself. `EXT_queue_family_foreign` is advertised natively by KK (constant-only,
honest for one queue family on shared storage), and vkCreateDevice stops stripping both
extensions when the driver has them — the passthrough plumbing upstream venus needs after 0010
deletes. The 2026-08-04 probe instrumentation (VKR-HT/VKRMOD/VKRMODLIST prints, the three env
gates) is retired; this file is its record.

**A transition hazard, predicted then observed.** With mesa 0010(b) still in the guest, EXPLICIT
imports carry the guest's fabricated tight-packed pitch (`width*4`), wrong whenever it misses
Metal's 16-byte row alignment. Strict spec-style rejection would have failed every odd-width
client window until task #4 lands. KK instead defines the invalid-layout UB as
adopt-the-computed-pitch with a loud `[KK-MODIFIER]` log — coherent by construction (the exporter
at the same width computes the same pitch), and the queries never echo the lie. First seated boot
confirmed it live: `EXPLICIT rowPitch 4700 unusable (minimum 4704, alignment 16)` ×4 from a
1175-px window, absorbed. **The log's disappearance after 0010 deletion is the tripwire that the
guest went truthful.**

Validation (guest unchanged, still 0010(b)): seated venus session on `modprobe-arm.raw`, zero
readback fallbacks, zero bind rejections, zero aborts, vkmark 13/13 (1667), gnome-terminal +
nautilus at odd widths, desktop **human-confirmed pixel-correct — no shear**. The standalone KK
probe re-run green with the graceful semantics (bogus pitch tolerated, true pitch reported).

## Caveat on the numbers above

The vkmark scores taken during axis 2 (318 / 298 / 230) ran while a kernel build had all eight
cores of another guest. They are evidence that venus *works*, not perf data, and must not be
compared to each other or to the ledger.

## Delivery validation crash (2026-08-04, task #4): two latent bugs the truthful path exposed

The first `enhanced.raw` refresh with the 0010-less guest (mesa 26.1.5-5.limina) seated a
pixel-correct desktop — and then **starting firefox killed the whole VMM**:

```
[LIMINA-VKR-HT] export handleTypes=0x200 (DMA_BUF )
Assertion failed: (image->tiling == VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT),
  function vk_common_GetImageDrmFormatModifierPropertiesEXT, vk_image.c:190
```

(The DMA_BUF handle type is itself the good news: upstream venus's own export branch fired,
0010(a) confirmed dead code.)

**Bug 1 — host: an assert reachable by guest input.** Guest zink queries
`vkGetImageDrmFormatModifierPropertiesEXT` on images that were never modifier-created (an
upstream valid-usage looseness every release-built Linux driver silently tolerates — the
entrypoint just returns `image->drm_format_mod`). KK's assert-enabled devenv build aborted
instead, taking the VMM with it. Fixed on `limina-kk` (b778250986b): the common entrypoint now
returns the field with a loud `[KK-MODIFIER] ... non-modifier image` stderr diagnostic instead of
asserting. A guest VU slip must degrade, never kill the host. The diagnostic immediately
identified the caller: firefox's **1×1 BGRA LINEAR WSI test surface** (`tiling=1 format=44
extent=1x1`).

**Bug 2 — guest: mesa 0015's DRM_MOD→OPTIMAL WSI rewrite, obsolete by its own comment.**
`vn_wsi_create_image` rewrote modifier-tiled swapchain creates to OPTIMAL because "the renderer
doesn't speak VK_EXT_image_drm_format_modifier ... which then trips wsi_create_native_image_mem".
That premise died when KK grew the extension — and the hunk became the crash it once prevented:
wsi_common still walks the modifier path (`supports_modifiers=true`), but the OPTIMAL-rewritten
image makes the now-REAL host query return `DRM_FORMAT_MOD_INVALID` (nothing stamped), and
`wsi_create_native_image_mem` SIGSEGVs dereferencing plane info for a modifier it never
negotiated. In the 0010(b) era the guest's *fabricated local* query answered LINEAR and masked
the mismatch — deleting 0010 unmasked it. Fix: the rewrite hunk is REMOVED from 0015 (guest RPM
respun as 26.1.5-6.limina); swapchain images keep DRM_FORMAT_MODIFIER tiling end-to-end and ride
vkr's verbatim native path. `treat_invalid_modifier_as_linear` and `block_16f` stay (their own
measurement arms later; the residual-0015 section above).

Lesson, again: **the masked path is the one that breaks.** 0010(b) didn't just fabricate an
extension table — it locally answered queries the host was never asked, and every consumer of
those answers was untested against the real host until the fabrication died.

## 2026-08-14 — re-measuring the MTLTEXTURE gate before flipping its default (task #26)

The 08-04 A/B above (gate-on 1518 vs gate-off 1527 on vkmark, "adoption counts 3→16") was taken
the same day KK's **native** `VK_EXT_image_drm_format_modifier` landed, and one day before the
MTL4 rebase. Re-measured both from scratch rather than re-read, and the shape has changed.

### The gate has two halves and only ONE is still reachable

`LIMINA_KK_MTLTEXTURE_SCANOUT` is read at five sites: two in `vkr_image.c` (the create-side
export half + its import twin) and three in `vkr_device_memory.c` (the memory-import half).
In `vkr_image.c` the `else if (limina_native_mod)` branch now sits **ahead** of the `else` that
holds the create-side gate, so on KK every modifier-tiled create — i.e. every scanout image a
GBM/venus client allocates — takes the native-modifier branch and the create-side gate is
never evaluated.

Measured on the F44 enhanced GNOME image, seated desktop, gate ON:

| path (log line)                            | count |
|--------------------------------------------|-------|
| `KK linear scanout image` (native-mod)     | 3     |
| `KK MTLTEXTURE scanout` (create-side gate) | **0** |
| `scanout memory <- MTLTEXTURE` (mem half)  | 3     |
| `LIMINA-KK-IMPORT adopted MTLTexture` (KK) | 3     |
| `no zero-copy` (pitch-mismatch drop)       | 0     |

So the live configuration is a **hybrid**: the image is created by the native-modifier path
(LINEAR, KK's own truthful pitch, IOSurface allocated to match), and then the memory-side gate
hands KK that IOSurface's **MTLTexture** instead of host-pointer-importing its bytes. KK adopts
it (`linear=1`) and the desktop renders. This hybrid — not the create-side path — is what the
08-04 "3→16 adoptions" actually measured.

### The A/B at an unaligned width: no differential

The gate's design claim is "no pitch to match, so the *IOSurface pitch != image rowPitch → no
zero-copy* failure mode goes away". That claim belongs to the **create-side** half, which is
now dead. Tested at `--display-resolution 1974x1200` (1974*4 = 7896, not 256-aligned — the
width class that sheared GTK4 windows on 08-04):

| arm      | create path | rowPitch | IOSurface bpr | drops | KK adoptions |
|----------|-------------|---------:|--------------:|------:|-------------:|
| gate OFF | native-mod  | 7904     | 7904          | 0     | 0            |
| gate ON  | native-mod  | 7904     | 7904          | 0     | 3            |

Identical create-side behaviour; the pitches **match in both arms**. They match because the
stride fix (virglrenderer limina `5c76245`) forces the IOSurface's `bytesPerRow` to Metal's
minimum linear alignment, which is what KK computes too. The failure mode MTLTEXTURE was meant
to design away has already been closed by a different fix, on the path that is still live.

**Conclusion: flipping the default is a mechanism swap, not a coverage or perf win.** Off-gate
the memory aliases the IOSurface's bytes via `VK_EXT_external_memory_host`; on-gate it adopts
the IOSurface's texture via `VK_EXT_external_memory_metal`. Both work at both widths. The one
real argument for the swap is failure behaviour, not capability: KK validates the texture
against the image at bind and **fails loudly** on a mismatch, whereas the host-pointer path is
the one whose mismatches Metal can silently no-op (the MoltenVK note in `vkr_image.c`).
The stride fix stays load-bearing either way — the flip does not retire it.

### Measurement trap: TWO log filters, and only `fprintf` is trustworthy

The first three runs of this measurement read "create-side dead AND native-mod dead", which is
false. `vkr_log` is INFO-level (`vkr_common.c:261`) and gets dropped **twice**: virglrenderer's
own `virgl_log_level` defaults to WARNING/ERROR (`virgl_util.c:196`) and drops the message
before it is ever handed to rutabaga, and whatever survives then meets the Rust `RUST_LOG=warn`
default. `vkr_log_error` is the ERROR twin and always shows, which is why the GPU-budget lines
were visible and made the log look healthy. Both filters must be opened —
`VIRGL_LOG_LEVEL=info RUST_LOG=krun_rutabaga_gfx=info` — and `RUST_LOG=debug` alone is worse
than useless here: it floods with per-MMIO `EC_DATAABORT` traces and strangles the boot.

The only lines that never lie are the raw `fprintf(stderr, ...)` ones, which is what made the
create-side-vs-memory-side asymmetry provable: same env var, same process, memory half fired
3×, create half 0×, with no logging in the path.

### Independent corroboration: the 300-flip IOSurface site census

`spikes/venus-churn-retention/RESULTS.md` had already tagged every IOSurface allocation site in
vkr and counted them across a 300-flip churn, months before this pass:

| site | allocations |
|---|---|
| B `vkr_image.c` mtltexture (create-side gate) | **0** |
| C `vkr_image.c` kk_linear (native modifier)   | **602** |

That was read at the time as "0 because the gate is off". It is 0 for a *structural* reason —
row C claims every modifier-tiled create before row B is reached — and would still read 0 with
the gate on, which is exactly what this pass measured directly. Worth noting as a reading
failure, not a measurement failure: the number was right and sitting in the repo, but a gated
feature next to a zero invites the lazy explanation and nobody checked which one was true.

### What was flipped, and what was deliberately NOT

Flipped: the default is now ON, `LIMINA_KK_MTLTEXTURE_SCANOUT=0` opts out. All four parse sites
were replaced by ONE cached parse (`vkr_limina_mtltex_scanout()` in `vkr_common.c`) — the halves
must agree, since an image created down the MTLTEXTURE branch bound to host-pointer memory is
corruption rather than a fallback, and a single answer makes that unrepresentable.

NOT done, on purpose: the two shadowed create-side branches were left in place. They are dead
under every client measured (GNOME + synoik), but they are the only path for an external-memory
create carrying no modifier struct, and "measured on two clients" is not "cannot happen".
Retiring them is a candidate for its own pass — the `LIMINA_KK_SLIMROOT` lesson (a flag forwarded
long after anything read it) argues for doing it eventually, and equally for not bundling it into
an unrelated change.

### Verification of the flip

| arm | scanout creates | MTLTEX mem imports | KK adoptions | create-side | drops | ring FATAL |
|---|---:|---:|---:|---:|---:|---:|
| F44 GNOME, default (no env)   | 3 | 3 | 3 | 0 | 0 | 0 |
| F44 GNOME, `=0` override      | 3 | 0 | 0 | 0 | 0 | 0 |
| F44 GNOME, `=1` @1974 wide    | 3 | 3 | 3 | 0 | 0 | 0 |
| F44 GNOME, off @1974 wide     | 3 | 0 | 0 | 0 | 0 | 0 |
| synoik (Vulkan compositor)    | 2 | 4 | 4 | 0 | 0 | 0 |

The `=0` arm is the override proof: the gate genuinely reaches the worker in the direction that
now matters. Full HVF suite run after the flip; result read from the log, not from the launch
command's exit status.

### Decision (2026-08-14)

Shipped ON despite the neutral A/B, on error-handling grounds: a mismatch that returns
`VK_ERROR_INVALID_EXTERNAL_HANDLE` from `vkBindImageMemory` — naming which of geometry/format/
usage failed, with both sides' dimensions, type, format and usage masks — is worth real debugging
time later, against a failure mode where every layer reports success and the only evidence is a
window that renders stale or sheared. The magenta-surface incident is the precedent: a
prefilled scanout stayed pure magenta while the guest believed it had drawn.

Note what "loud" does and does not buy. It is an **error return, not a host abort** — the VMM
survives (checked deliberately: an abort would have been a worse trade than sheared pixels). But
it is diagnosable, not recoverable: a rejected bind fails the compositor's surface import, so the
guest-visible result is a failed window, just one with a precise cause attached.

Also corrected while here: the create-side comment claimed "the guest's own tiling survives". It
does not and cannot. The texture is built with `-newTextureWithDescriptor:iosurface:plane:`, so
its storage IS the IOSurface's — linear, with a `bytesPerRow`. Declaring OPTIMAL buys KK's
plain-2D layout bookkeeping, not a tiled surface, and nothing that crosses to the compositor as
an IOSurface can be truly tiled. Related: the native-modifier path did NOT make scanout
non-linear either — KK's modifier extension is LINEAR-only, so what 08-04 changed is that the
guest now *negotiates* linear instead of vkr rewriting its create behind its back. The lie went
away; the linearity did not.
