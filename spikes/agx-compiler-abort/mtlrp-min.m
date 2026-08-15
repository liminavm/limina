// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// MINIMAL reproducer for the AGX Metal compiler `bitcode_url is NULL` abort (task #29).
// No Vulkan, no VM, no GPU work -- one render pass descriptor and one encoder.
//
// Creating a render command encoder from a descriptor that has NO attachments and leaves
// `defaultRasterSampleCount` at its default of 0 makes MTLCompilerService assert while building
// the pass's background object fragment shader:
//
//   AGCLLVMObject::readBitcode(...) bitcode_url is NULL for bundle 'com.apple.AGXCompilerCore',
//   filename '<private>', extension 'ds'
//
// The calling process then takes SIGABRT after Metal's 10 XPC retries. The abort is inside Metal,
// so a client cannot catch it -- only avoid it.
//
//   clang -O1 -g -fobjc-arc -o mtlrp-min mtlrp-min.m -framework Metal -framework Foundation
//   ./mtlrp-min          # 0 = did not abort, 134 = reproduced
//   ./mtlrp-min 4        # same case through MTL4, the API KosmicKrisp uses
//   ./mtlrp-min 1        # control: identical but defaultRasterSampleCount = 1
//
// Observed: Apple M1 Max (AGXMetalG13X), macOS 26.5. A dogfood M4 Pro has never hit the in-VM
// abort, so this may be G13-only -- unverified, that machine is not ours to experiment on.

#import <Metal/Metal.h>
#import <Metal/MTL4RenderPass.h>

int
main(int argc, char **argv)
{
   /* argv[1]: "4" selects MTL4; anything else numeric is the sample count for the classic path. */
   bool mtl4 = argc > 1 && strcmp(argv[1], "4") == 0;
   NSUInteger samples = (argc > 1 && !mtl4) ? (NSUInteger)atoi(argv[1]) : 0;

   id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
   if (!dev)
      return 2;

   printf("device: %s -- %s, no attachments, defaultRasterSampleCount=%lu\n",
          [[dev name] UTF8String], mtl4 ? "MTL4" : "classic", (unsigned long)samples);
   fflush(stdout);

   if (mtl4) {
      MTL4RenderPassDescriptor *rp = [[MTL4RenderPassDescriptor alloc] init];
      rp.renderTargetWidth = 800;
      rp.renderTargetHeight = 600;
      /* defaultRasterSampleCount deliberately left at 0 -- that is the trigger. */

      id<MTL4CommandQueue> q = [dev newMTL4CommandQueue];
      id<MTL4CommandAllocator> alloc = [dev newCommandAllocator];
      id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
      [cb beginCommandBufferWithAllocator:alloc];
      id<MTL4RenderCommandEncoder> enc = [cb renderCommandEncoderWithDescriptor:rp];
      if (!enc) {
         printf("rejected (nil encoder)\n");
         return 1;
      }
      [enc endEncoding];
      [cb endCommandBuffer];
      [q commit:&cb count:1];
   } else {
      MTLRenderPassDescriptor *rp = [MTLRenderPassDescriptor renderPassDescriptor];
      rp.renderTargetWidth = 800;
      rp.renderTargetHeight = 600;
      rp.defaultRasterSampleCount = samples;

      id<MTLCommandQueue> q = [dev newCommandQueue];
      id<MTLCommandBuffer> cb = [q commandBuffer];
      id<MTLRenderCommandEncoder> enc = [cb renderCommandEncoderWithDescriptor:rp];
      if (!enc) {
         printf("rejected (nil encoder)\n");
         return 1;
      }
      [enc endEncoding];
      [cb commit];
      [cb waitUntilCompleted];
   }

   printf("survived -- did not reproduce\n");
   return 0;
}
