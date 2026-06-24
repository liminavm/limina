#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Variant of the 0008 venus patch (option A): expose VK_EXT_external_memory_dma_buf on an
# opaque-fd-only host renderer (KosmicKrisp/Metal), but do NOT force
# EXT_image_drm_format_modifier — because advertising modifiers makes mutter-50 allocate
# render-target images with a DRM modifier, and KK can't build a Metal render target from a
# modifier image (newRenderCommandEncoderWithDescriptor -> nil). Without modifier support
# mutter falls back to implicit/optimal render targets KK CAN render into, while dma-buf
# export (the gbm crash fix) still works. queue_family_foreign is kept (queue-ownership
# transfer for dma-buf, unrelated to render targets).
import sys, io
f = "/build/mesa/src/virtio/vulkan/vn_physical_device.c"
src = io.open(f, encoding="utf-8").read()
if "macOS with KosmicKrisp" in src:
    print("vn_physical_device already patched", file=sys.stderr); sys.exit(0)

edits = [
 # Edit A: opaque-fd renderer branch (identical to 0008)
 ("""      physical_dev->external_memory.supported_handle_types =
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT |
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
#endif
   }
}""",
  """      physical_dev->external_memory.supported_handle_types =
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT |
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
#endif
   } else if (physical_dev->renderer_extensions.KHR_external_memory_fd) {
      /* Renderer supports opaque fd export (e.g. macOS with KosmicKrisp) but not
       * DMA-buf.  Use opaque fd as the renderer handle type.  We still advertise
       * DMA-buf to the guest because in a virtualized environment the guest
       * kernel's DMA-buf fds are backed by virtio-gpu GEM objects which map to
       * host blob resources; the actual host handle type is opaque fd — the
       * translation happens in vn_device_memory_fix_alloc_info.
       */
      physical_dev->external_memory.renderer_handle_type =
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
      physical_dev->external_memory.supported_handle_types =
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT |
         VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
   }
}"""),

 # Edit B: force ONLY queue_family_foreign (NOT drm_format_modifier — option A)
 ("""   memset(exts, 0, sizeof(*exts));

   if (physical_dev->instance->renderer->info.has_external_sync &&""",
  """   memset(exts, 0, sizeof(*exts));

#if !DETECT_OS_WINDOWS
   /* queue_family_foreign is just a constant (VK_QUEUE_FAMILY_FOREIGN_EXT), no
    * new functions.  Needed for DMA-buf queue-ownership transfer with compositors.
    * NOTE: we deliberately do NOT force EXT_image_drm_format_modifier — KK cannot
    * build a Metal render target from a modifier image, and mutter-50 would pick
    * modifier render targets if we advertised it (newRenderCommandEncoderWithDescriptor
    * -> nil). Implicit/optimal layouts render fine and dma-buf export still works.
    */
   exts->EXT_queue_family_foreign = true;
#endif

   if (physical_dev->instance->renderer->info.has_external_sync &&"""),
]
for old, new in edits:
    if old not in src:
        print("ANCHOR NOT FOUND:\n" + old[:120], file=sys.stderr); sys.exit(2)
    src = src.replace(old, new, 1)
io.open(f, "w", encoding="utf-8").write(src)
print("patched vn_physical_device.c (option A: dma-buf via opaque-fd, NO drm_format_modifier)")
