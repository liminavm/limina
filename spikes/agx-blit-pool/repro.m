/* Does AGX's compute data-buffer pool die at a fixed number of blits on ONE encoder?
 *
 * The dogfood VMM has SIGSEGV'd twice inside AGX with a byte-identical register state:
 *
 *   AGX::ComputeContext::prepareForEnqueue -> str x8, [x9, #0x98], x9 = NULL
 *   x15 = 0x100000    the pool's segment size, 1 MiB
 *   x14 = 0x10001f    the cursor, 31 bytes PAST that segment
 *   x9  = 0x0         the next segment: absent, and AGX does not check it
 *
 * Reached from kk_CmdCopyBufferToImage2 -> mtl_copy_from_buffer_to_texture, i.e. exactly
 * the call this file makes in a loop. Every value register identical across two processes
 * seven hours apart says a deterministic sequence walks a cursor off a fixed-size segment
 * — a capacity boundary, not memory pressure. If that is right, blits on ONE encoder
 * should die at a repeatable count with no VM, no guest and no Firefox in the picture.
 *
 * A clean run to a large N falsifies it just as usefully: the trigger would then need
 * something this vehicle does not model (encoder reuse across allocator resets, mixed
 * render/compute, a particular copy geometry) and we would know to look there instead.
 *
 * Usage:
 *   repro                    ramp blits on ONE encoder (the first hypothesis)
 *   repro blits N            N blits on one encoder
 *   repro encoders N [B]     ONE allocator, N encoders, B blits each — no reset between
 *   repro reuse N [B]        N cycles of reset -> begin -> encode B blits -> commit, one
 *                            allocator reused throughout, which is what KK's pool does
 *                            millions of times (dogfood: resets=5.4M render / 2.5M compute)
 */
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <Metal/MTL4CommandQueue.h>
#import <Metal/MTL4CommandBuffer.h>
#import <Metal/MTL4CommandAllocator.h>
#import <Metal/MTL4ComputeCommandEncoder.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdbool.h>

static id<MTLDevice> dev;
static id<MTL4CommandQueue> queue;
static id<MTLBuffer> src;
static id<MTLTexture> tex;

/* One command buffer, one compute encoder, `blits` copies on it. Returns after committing.
 * The fault we are chasing happens while ENCODING (prepareForEnqueue builds the stream), so
 * a crash arrives before the commit, not on the GPU. */
static void run_encoder(unsigned blits, unsigned w, unsigned h, unsigned bpr)
{
   @autoreleasepool {
      id<MTL4CommandAllocator> alloc = [dev newCommandAllocator];
      id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
      [cb beginCommandBufferWithAllocator:alloc];
      id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];

      for (unsigned i = 0; i < blits; i++) {
         [enc copyFromBuffer:src
                sourceOffset:0
           sourceBytesPerRow:bpr
         sourceBytesPerImage:bpr * h
                  sourceSize:MTLSizeMake(w, h, 1)
                   toTexture:tex
            destinationSlice:0
            destinationLevel:0
           destinationOrigin:MTLOriginMake(0, 0, 0)
                     options:MTLBlitOptionNone];
         if (i && i % 1000 == 0) {
            fprintf(stderr, "    ... %u blits encoded\n", i);
            fflush(stderr);
         }
      }

      [enc endEncoding];
      [cb endCommandBuffer];

      /* Wait for the GPU via a queue event: the fault we are chasing is at ENCODE time, so
       * this only keeps successive steps from piling submissions up behind each other. */
      MTL4CommitOptions *opt = [MTL4CommitOptions new];
      [queue commit:&cb count:1 options:opt];
      id<MTLEvent> ev = [dev newEvent];
      static uint64_t val = 0;
      [queue signalEvent:ev value:++val];
   }
}

/* ONE allocator, many encoders. If AGX's data-buffer pool is per-ALLOCATOR rather than
 * per-encoder, the cursor accumulates here and a per-encoder test would never reach it. */
static void run_many_encoders(unsigned encoders, unsigned blits, unsigned w, unsigned h,
                              unsigned bpr, bool reset_between)
{
   @autoreleasepool {
      id<MTL4CommandAllocator> alloc = [dev newCommandAllocator];
      for (unsigned e = 0; e < encoders; e++) {
         @autoreleasepool {
            if (reset_between)
               [alloc reset];
            id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
            [cb beginCommandBufferWithAllocator:alloc];
            id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];
            for (unsigned i = 0; i < blits; i++) {
               [enc copyFromBuffer:src
                      sourceOffset:0
                 sourceBytesPerRow:bpr
               sourceBytesPerImage:bpr * h
                        sourceSize:MTLSizeMake(w, h, 1)
                         toTexture:tex
                  destinationSlice:0
                  destinationLevel:0
                 destinationOrigin:MTLOriginMake(0, 0, 0)
                           options:MTLBlitOptionNone];
            }
            [enc endEncoding];
            [cb endCommandBuffer];
            MTL4CommitOptions *opt = [MTL4CommitOptions new];
            [queue commit:&cb count:1 options:opt];
            if (e && e % 5000 == 0) {
               fprintf(stderr, "    ... %u encoders, allocatedSize=%llu\n", e,
                       (unsigned long long)[alloc allocatedSize]);
               fflush(stderr);
            }
         }
      }
      fprintf(stderr, "    final allocatedSize=%llu\n",
              (unsigned long long)[alloc allocatedSize]);
   }
}

int main(int argc, char **argv)
{
   @autoreleasepool {
      dev = MTLCreateSystemDefaultDevice();
      if (!dev) { fprintf(stderr, "no Metal device\n"); return 2; }
      queue = [dev newMTL4CommandQueue];
      if (!queue) { fprintf(stderr, "no MTL4 command queue (needs macOS 26+)\n"); return 2; }
      fprintf(stderr, "device: %s\n", [[dev name] UTF8String]);

      const unsigned w = 256;
      const unsigned h = 256, bpr = w * 4;
      src = [dev newBufferWithLength:(NSUInteger)bpr * h options:MTLResourceStorageModeShared];
      MTLTextureDescriptor *td =
         [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm
                                                            width:w height:h mipmapped:NO];
      td.usage = MTLTextureUsageShaderRead | MTLTextureUsageShaderWrite;
      td.storageMode = MTLStorageModePrivate;
      tex = [dev newTextureWithDescriptor:td];
      if (!src || !tex) { fprintf(stderr, "allocation failed\n"); return 2; }

      if (argc > 2 && strcmp(argv[1], "encoders") == 0) {
         unsigned n = (unsigned)atoi(argv[2]);
         unsigned b = argc > 3 ? (unsigned)atoi(argv[3]) : 8;
         fprintf(stderr, "one allocator, %u encoders x %u blits, NO reset...\n", n, b);
         run_many_encoders(n, b, w, h, bpr, false);
         fprintf(stderr, "SURVIVED %u encoders on one allocator\n", n);
         return 0;
      }
      if (argc > 2 && strcmp(argv[1], "reuse") == 0) {
         unsigned n = (unsigned)atoi(argv[2]);
         unsigned b = argc > 3 ? (unsigned)atoi(argv[3]) : 8;
         fprintf(stderr, "one allocator, %u reset/reuse cycles x %u blits...\n", n, b);
         run_many_encoders(n, b, w, h, bpr, true);
         fprintf(stderr, "SURVIVED %u reuse cycles\n", n);
         return 0;
      }
      if (argc > 2 && strcmp(argv[1], "blits") == 0) {
         unsigned n = (unsigned)atoi(argv[2]);
         fprintf(stderr, "one encoder, %u blits of %ux%u...\n", n, w, h);
         run_encoder(n, w, h, bpr);
         fprintf(stderr, "SURVIVED %u blits on one encoder\n", n);
         return 0;
      }

      /* Ramp. Each step is its own encoder, so a step that dies names the per-encoder
       * ceiling rather than a lifetime total. */
      for (unsigned n = 64; n <= 4u * 1024 * 1024; n *= 2) {
         fprintf(stderr, "== %u blits on one encoder\n", n);
         run_encoder(n, w, h, bpr);
         fprintf(stderr, "   survived %u\n", n);
      }
      fprintf(stderr, "ramp completed without a fault\n");
      return 0;
   }
}
