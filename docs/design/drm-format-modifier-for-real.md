# Implementing `VK_EXT_image_drm_format_modifier` for real (virgl/KK)

Status: **design, pre-code** (2026-08-04). Task #19. Written after the modifier pressure test
(`spikes/modifier-necessity/RESULTS.md`), which dropped the two kernel patches and left the Vulkan
axis standing.

## What this is supposed to buy

The ledger's stated durable fix for three carried patches:

| patch | what it does | collapses? |
|---|---|---|
| mesa 0010 | force-advertises the modifier ext in venus + answers it in-guest | **half of it** — see below |
| mesa 0015 | venus WSI present fix; residual is modifier-shaped | partially |
| virgl 0005 | normalizes `DRM_FORMAT_MODIFIER` tiling → `OPTIMAL` for KK | yes, if KK accepts modifier tiling |

## Findings that change the plan

### 1. mesa 0010 is two patches wearing one hat, and only one of them is about modifiers

Read the diff, not the title. It does two independent things:

- **(a) External-memory handle type for a renderer that has `KHR_external_memory_fd` but no
  dma-buf.** Advertises dma-buf *to the guest* while using opaque-fd *to the renderer*. This is the
  half the ledger's "without it: `caps.dmabuf=0` → gbm dumb buffer → gnome-shell SIGSEGV" note is
  about. **Absent from upstream tip** (`vn_physical_device.c` still only has the dma-buf branch).
  Load-bearing, and not modifier-related at all.
- **(b) The modifier fiction.** Moves `EXT_image_drm_format_modifier` into venus's *native*
  extension table, answers `vkGetImageDrmFormatModifierPropertiesEXT` locally with
  `DRM_FORMAT_MOD_LINEAR`, and overrides `vkGetImageSubresourceLayout` with a computed linear
  layout.

### 1a. (a) is NOT stale — but it collapses by the same lever as (b)

Tested 2026-08-04 (the hypothesis was that (a) predates dma-buf working and is now dead code).
It is not. On venus tip, `vn_physical_device_init_external_memory` sets `renderer_handle_type`
**only** under `renderer_extensions.EXT_external_memory_dma_buf` (line 1042), and vkr on macOS
never advertises that extension — it *injects* `KHR_external_memory_fd` instead
(`vkr_physical_device.c:329`, because KK has no native fd support and vkr emulates it over Metal
shared memory). So without 0010(a), `renderer_handle_type` stays 0, the gate at line 1132 fails,
and venus advertises neither fd nor dma-buf to the guest — the documented `caps.dmabuf=0` → gbm
dumb buffer → gnome-shell SIGSEGV path. **(a) is live and load-bearing.**

The real asymmetry is one layer down, in vkr: it already injects one emulated extension and not
the other. If vkr also advertised `EXT_external_memory_dma_buf` and accepted that handle type —
translating to opaque-fd host-side, which is exactly what 0010(a)'s own comment says currently
happens *in the guest* (`vn_device_memory_fix_alloc_info`) — then upstream's branch at line 1042
fires and **0010(a) deletes itself**.

So both halves of 0010 reduce to one principle:

> **Make the renderer advertise honestly, instead of patching the guest driver to compensate.**

(a) → vkr advertises + accepts dma-buf (emulated, as it already does for fd).
(b) → vkr/KK advertises the modifier extension, and upstream's passthrough gate does the rest.

Both land in virglrenderer/KK, which we own and are already upstreaming into — a better place for
the delta than Mesa's guest driver.

**Terminology, because it is easy to garble:** "(a)" and "(b)" always mean the two halves of
*mesa 0010*, i.e. guest-side venus patches. The vkr dma-buf advertisement is the **replacement**
for (a), not (a) itself. So (a) gets **deleted**, and what becomes upstreamable is the new
virglrenderer patch. An earlier draft of this doc said "(a) is upstreamable on its own merits" —
that was written while (a) still looked load-bearing, and it no longer holds: fixing this renderer-
side means upstream venus never needs a branch for a case only non-Linux renderers reach.

**Unconfirmed:** the vkr injection carries `TODO: Remove after mesa!40478 has had sufficient distro
uptake`. A search of mesa tip found a related *"venus: relax SYNC_FD semaphore export requirement"*
commit but nothing identifiable as !40478 — do not treat that TODO as ripe without checking.

### 1b. PROVEN (2026-08-04): the (a) half works, and 0010(a) is now dead code

Implemented and measured the same day. vkr's macOS injection block now advertises
`VK_EXT_external_memory_dma_buf` alongside the `VK_KHR_external_memory_fd` it already injected
(`spikes/modifier-necessity/virgl-inject-dmabuf-ext.patch`). Nothing else changed — no guest mesa
rebuild, 0010 still fully applied.

The oracle is direct rather than inferred: venus stamps `renderer_handle_type` into
`VkExternalMemoryImageCreateInfo::handleTypes`, so a probe at vkr's image-create read it back:

```
[LIMINA-VKR-HT] image external handleTypes=0x200 (DMA_BUF)
```

`0x200` is `DMA_BUF`. Before the change it would be `0x2` (`OPAQUE_FD`). Since 0010(a) is an
`else if` *after* upstream's dma-buf branch, upstream's branch firing means **0010(a) no longer
executes** — proven without rebuilding guest mesa.

Session verified on the enhanced.test image: venus live in the seated session (`Virtio-GPU Venus`),
gnome-shell up with **0** segfaults in the boot journal, guest sees
`VK_EXT_external_memory_dma_buf`, no `degrading to software-2D` / `ComponentError` in the worker,
and the desktop **human-confirmed correct** (the health checks alone would not have caught a
shear-class fault, per the same morning's lesson).

That the change was this small is itself evidence for the "advertise honestly" reading: vkr's memory
paths already accepted both handle types interchangeably (the export strip takes
`OPAQUE_FD|DMA_BUF`; imports arrive via the handle-agnostic `VkImportMemoryResourceInfoMESA`). Only
the advertisement was missing.

**Remaining to actually retire 0010(a):** promote the vkr change from spike patch to a real
`patches/virglrenderer/` entry (minus the probe `fprintf`), delete the (a) hunks from
`patches/mesa/0010`, rebuild the guest mesa RPM, refresh the enhanced images, and re-validate. The
ledger row for 0010 should split into (a) and (b) at the same time.

### 2. Upstream venus is strict passthrough, and that is the whole lever

`EXT_image_drm_format_modifier` is in upstream's **passthrough** table
(`vn_physical_device_get_passthrough_extensions`, tip line ~1337), and passthrough entries are
intersected with renderer support at `vn_physical_device.c:1409-1410`:

```c
} else if (passthrough.extensions[i] &&
           physical_dev->renderer_extensions.extensions[i]) {
```

`native_extensions` — where 0010(b) inserts — bypasses that gate. So:

> **If KK advertises the extension, stock upstream venus advertises it to the guest by itself.**
> 0010(b) then has nothing left to do and deletes itself, using upstream code rather than a
> replacement patch of ours.

That is a materially better outcome than "shrink the patch", and it is the actual argument for
doing this work.

### 3. KK already has `EXT_external_memory_metal`; it has no modifier support at all

Verified on mesa tip: `kk_physical_device.c` advertises `EXT_external_memory_metal = true`
upstream. Our contribution there was the `MTLTEXTURE` *handle type* inside it, not the extension.
`EXT_image_drm_format_modifier` appears nowhere in `src/kosmickrisp` on tip — this is a from-scratch
feature, not a gap-fill.

### 4. 0010's comments describe a world that does not exist

The patch says *"KK now supports VK_EXT_image_drm_format_modifier and LINEAR COLOR_ATTACHMENT
directly"*. KK does not, on tip or in our tree. Whatever happens to the patch, the comment is
actively misleading and should not be trusted by the next reader.

## The design question, and why it is not obvious

0010's own rationale is a real architectural position, not just a hack:

> *"In a VM, DRM modifiers are metadata for the guest DRM stack — the host renderer doesn't need
> the extension."*
> *"Guest memory mapping goes through blob resources, so the actual host layout is irrelevant."*

Today's MTLTEXTURE work is **evidence for** that position: the host layout is now genuinely
non-linear (an IOSurface-backed `MTLTexture`), the guest is still told `LINEAR`, and everything
renders correctly. The decoupling holds.

So "implement it for real" must answer: *if the host layout is deliberately opaque to the guest,
what does a host-side modifier actually mean?*

**Proposed answer.** A DRM modifier is an *opaque token identifying a layout* — that is its entire
job. KK can honestly expose exactly two:

- `DRM_FORMAT_MOD_LINEAR` — for images KK genuinely lays out linearly.
- one vendor/opaque token meaning "Metal's private tiling for this format" — which is a true
  statement about a layout KK controls and can reproduce.

This is not the same fiction relocated. The current fiction claims *linear* for something that is
not linear; the proposal names an opaque layout opaquely, which is what modifiers are for. The
guest keeps getting a predictable stride for DRM framebuffer bookkeeping via the linear modifier
when it needs one, and a real token when it does not.

## Measure before coding

Assumptions to falsify first — the modifier axis has already burned us once by building on
inferred premises:

1. **Does the guest ever use the claimed linear stride for actual pixel access**, or only for DRM
   framebuffer bookkeeping? 0010's comment asserts the latter; today's MTLTEXTURE result is
   consistent with it but does not prove it. Instrument `vn_GetImageSubresourceLayout` callers.
2. **How many modifiers does the guest WSI actually negotiate?** 0010 hardcodes one. If one is
   genuinely enough, the two-token design above is over-built.
3. **Does virgl 0005's normalization become unnecessary, or merely wrong?** KK accepting modifier
   tiling is necessary but maybe not sufficient — vkr still strips the modifier structs for other
   reasons (the `EXTERNAL_MEMORY_IMAGE_CREATE_INFO` unlink is a separate concern).
4. **What does mesa 0015's residual actually depend on?** The ledger calls it modifier-shaped; that
   has not been tested by removing 0010(b) and watching what breaks.

Recommended order: (2) then (1) — both are cheap counter/probe work on the existing rig — before
any KK code.

## Non-goals

- Reviving the two dropped kernel patches (linux 0002/0003). Those are punted to the "additional
  hardware planes" release and nothing here depends on them.
- Retiring the forced LINEAR. That is the MTLTEXTURE gate's job (separate, and already measured
  better on the compositor rig); it is not blocked on this work.
