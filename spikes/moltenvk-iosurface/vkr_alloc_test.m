// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Host test for the REAL vkr_mtl_iosurface_alloc (linked from the built
// venus_vkr_metal_helpers.m.o), proving the limina tier-2 allocator (B) end-to-end:
//   1. vkr_mtl_iosurface_alloc returns a global IOSurface + IOSurface-backed MTLTexture.
//   2. The GPU renders into that texture (render-pass clear to a known color).
//   3. IOSurfaceLookup(id) — exactly how the limina supervisor resolves it for present —
//      finds the same surface, and its bytes show the GPU's write zero-copy.
// MRC (matches vkr_metal_helpers.m). See RESULTS.md / run-vkr-alloc.sh.

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <IOSurface/IOSurface.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

/* Mirror of struct vkr_mtl_iosurface + protos of the REAL functions we link against. */
struct vkr_mtl_iosurface {
   void *io_surface;
   void *mtl_texture;
   uint32_t id;
   uint32_t width;
   uint32_t height;
   uint32_t bytes_per_row;
};
struct vkr_mtl_iosurface *vkr_mtl_iosurface_alloc(void *mtl_device, uint32_t w, uint32_t h,
                                                  uint32_t mtl_fmt, uint32_t io_fmt,
                                                  uint32_t bpe);
void vkr_mtl_iosurface_free(struct vkr_mtl_iosurface *);

/* The translation unit also defines vkr_mtl_shm_alloc, which references this virgl util.
 * We never call the shm path; stub it so the single .o links standalone. */
int
os_create_anonymous_file(size_t size, const char *debug_name)
{
   (void)size;
   (void)debug_name;
   return -1;
}

int
main(void)
{
   @autoreleasepool {
      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      struct vkr_mtl_iosurface *s =
         vkr_mtl_iosurface_alloc((void *)dev, 256, 256, MTLPixelFormatBGRA8Unorm,
                                 (uint32_t)'BGRA', 4);
      if (!s) {
         printf("vkr_mtl_iosurface_alloc -> NULL\n");
         return 1;
      }
      printf("alloc: id=%u %ux%u bpr=%u tex=%p io=%p\n", s->id, s->width, s->height,
             s->bytes_per_row, s->mtl_texture, s->io_surface);
      if (!s->id || !s->mtl_texture || !s->io_surface) {
         printf("FAIL: null member\n");
         return 1;
      }

      /* GPU clears the IOSurface-backed texture to BGRA(204,102,51). */
      id<MTLTexture> tex = (id<MTLTexture>)s->mtl_texture;
      id<MTLCommandQueue> q = [dev newCommandQueue];
      MTLRenderPassDescriptor *rp = [MTLRenderPassDescriptor renderPassDescriptor];
      rp.colorAttachments[0].texture = tex;
      rp.colorAttachments[0].loadAction = MTLLoadActionClear;
      rp.colorAttachments[0].storeAction = MTLStoreActionStore;
      rp.colorAttachments[0].clearColor = MTLClearColorMake(0.20, 0.40, 0.80, 1.0);
      id<MTLCommandBuffer> cb = [q commandBuffer];
      id<MTLRenderCommandEncoder> enc = [cb renderCommandEncoderWithDescriptor:rp];
      [enc endEncoding];
      [cb commit];
      [cb waitUntilCompleted];

      /* Resolve by the GLOBAL id exactly like the supervisor's IOSurfaceLookup, read zero-copy. */
      IOSurfaceRef look = IOSurfaceLookup(s->id);
      if (!look) {
         printf("FAIL: IOSurfaceLookup(%u) -> NULL (not global?)\n", s->id);
         return 1;
      }
      IOSurfaceLock(look, kIOSurfaceLockReadOnly, NULL);
      uint8_t *b = (uint8_t *)IOSurfaceGetBaseAddress(look);
      uint8_t B = b[0], G = b[1], R = b[2], A = b[3];
      IOSurfaceUnlock(look, kIOSurfaceLockReadOnly, NULL);
      int ok = (abs(B - 204) <= 2 && abs(G - 102) <= 2 && abs(R - 51) <= 2);
      printf("after GPU clear, IOSurfaceLookup(%u) base=BGRA(%u,%u,%u,%u) %s\n", s->id, B, G,
             R, A, ok ? "<-- MATCH (GPU render seen via global lookup, zero-copy)" : "(mismatch)");

      vkr_mtl_iosurface_free(s);
      printf("\nVERDICT: real vkr_mtl_iosurface_alloc -> global IOSurface + GPU render visible "
             "via IOSurfaceLookup: %s\n", ok ? "PASS" : "FAIL");
      return ok ? 0 : 1;
   }
}
