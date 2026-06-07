# Spike: MoltenVK IOSurface export/import — the zero-copy present currency (B+D)

**Question (the gating unknown for tier-2 zero-copy):** can MoltenVK 1.4.1 give us an
IOSurface-backed `VkImage` that the GPU renders into, and can that IOSurface be presented
to a `CALayer` with no copy? If yes, IOSurface is the macOS dmabuf and it closes BOTH
crossing **B** (zero-copy scanout present) and crossing **D** (#30 mutter exportable
images) — see `docs/roadmap.md` M4 ZERO-COPY plan and memory `limina-tier2-venus`.

This is host-only (no VM boot). The probe links MoltenVK in-process **exactly like vkr**
(virglrenderer's venus host side) does, so anything that works here, vkr can do for
venus's scanout/exportable images.

## VERDICT: VIABLE — via the IMPORT path (the one vkr would use)

```
== device: Apple M1 Max (Vulkan 1.2.334) ==
  [YES] VK_EXT_metal_objects
  [YES] VK_EXT_external_memory_metal
  [YES] VK_KHR_external_memory
  [ no] VK_KHR_external_memory_fd
  [ no] VK_EXT_external_memory_dma_buf        <- venus advertises, MoltenVK lacks (#30)
  [ no] VK_EXT_image_drm_format_modifier      <- venus advertises, MoltenVK lacks (#30)
  [ no] VK_EXT_queue_family_foreign           <- venus advertises, MoltenVK lacks (#30)
  [YES] VK_KHR_portability_subset

== EXPORT direction: MoltenVK auto-create IOSurface -> NO (returns NULL) ==

== IMPORT direction: our IOSurface -> MTLTexture -> VkImage -> GPU render -> present ==
  IOSurface 256x256 bpr=1024 alloc=262144
  MTLTexture from IOSurface: OK
  VkImage(import) memreq size=264192 bits=0x3
  GPU cleared VkImage to BGRA(204,102,51,255)
  IOSurface base[0..3]=BGRA(204,102,51,255) <-- MATCH (GPU render seen in IOSurface, zero-copy)
  CALayer.contents=IOSurface: accepted ; re-wrap as MTLTexture: OK
```

## What this establishes

1. **`VK_EXT_metal_objects` is present and the import path works.** We create an IOSurface
   (`IOSurfaceCreate`, BGRA8), wrap it as an `MTLTexture`
   (`newTextureWithDescriptor:iosurface:plane:`), and import that texture as a `VkImage`'s
   backing via **`VkImportMetalTextureInfoEXT`** in `VkImageCreateInfo.pNext`. MoltenVK
   accepts it (memreq still non-zero → bind a placeholder allocation; the texture is the
   real backing).

2. **The GPU renders straight into the IOSurface.** A Vulkan `vkCmdClearColorImage` to
   BGRA(204,102,51,255) is immediately visible in `IOSurfaceGetBaseAddress` bytes with no
   transfer/copy and no explicit sync beyond `vkQueueWaitIdle`. (Consistent with the
   `mtl-shm-coherency` finding that the host CPU is in the GPU's coherency domain.)

3. **The same IOSurface is directly presentable.** `CALayer.contents = (id)ioSurface` is
   accepted, and it re-wraps as an `MTLTexture` (the `CAMetalLayer` drawable route). So
   libkrun's present path can hand the scanout IOSurface to a `CALayer`/`CAMetalLayer` with
   zero readback — replacing today's per-frame `read_2d_resource` CPU copy (crossing B; the
   likely cause of #29's windowed ~40 vs headless ~440).

4. **EXPORT auto-create is NOT supported, and that's fine.** MoltenVK will not conjure an
   IOSurface from `VkExportMetalObjectCreateInfoEXT(IOSURFACE)` on a normally-allocated
   image (returns NULL; with OPTIMAL tiling the texture is GPU-compressed `isCompressed=1`,
   with LINEAR it's MTLBuffer-backed — neither is IOSurface-backed). **vkr owns host image
   creation, so it uses the import path: vkr creates the IOSurface itself.**

5. **The dma_buf family is absent** (`external_memory_dma_buf`, `image_drm_format_modifier`,
   `queue_family_foreign`) — confirms #30's root cause. venus advertises these for the
   guest; MoltenVK can't honor them. The fix is **not** to pass them through but to **map
   the guest's "exportable image" request onto an IOSurface host-side** (import path above).

## Architecture implication (the plan, now de-risked)

IOSurface is the macOS dmabuf. For a venus image the guest wants exportable / for scanout,
**vkr should back it with an IOSurface-imported MTLTexture** instead of an ordinary texture:

- **Crossing D (#30 mutter):** vkr advertises an external-memory image path to the guest
  and, on the host `vkCreateImage`, allocates an IOSurface + imported MTLTexture (instead of
  failing `VK_ERROR_FEATURE_NOT_PRESENT`). mutter's GBM/KMS scanout image-create succeeds.
- **Crossing B (zero-copy present):** `SET_SCANOUT_BLOB` references that IOSurface; libkrun
  hands it to the limina display backend → `CALayer.contents` / `CAMetalLayer` drawable. No
  `read_2d_resource`.

Open follow-ups to pin down next (host-only where possible):
- Which exact venus/vkr code path creates the scanout/exportable image, and where to inject
  the IOSurface import (vkr `vkr_image` / the resource-blob export path).
- Format/modifier negotiation: what venus/mutter request vs BGRA8 IOSurface; multi-plane?
- Lifetime/handle plumbing of the IOSurface from vkr → libkrun → display vtable (the
  `present_texture(scanout_id, surface)` callback in roadmap M4 task 4).
- Does a *guest*-allocated venus host-visible blob need to be IOSurface-backed too, or only
  the scanout image? (Readback/#28 is separate — that's crossing C, not this.)

## Reproduce
`bash spikes/moltenvk-iosurface/run.sh` (builds `probe.m`, points the loader at MoltenVK).
`TILING=0 ./probe` to compare OPTIMAL vs LINEAR for the export-direction experiment.
