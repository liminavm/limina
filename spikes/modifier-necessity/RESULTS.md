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

## Caveat on the numbers above

The vkmark scores taken during axis 2 (318 / 298 / 230) ran while a kernel build had all eight
cores of another guest. They are evidence that venus *works*, not perf data, and must not be
compared to each other or to the ledger.
