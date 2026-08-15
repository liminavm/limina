// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* mtlrp: test render-pass configurations directly against Metal, with no Vulkan and no VM.
 *
 * Why this exists alongside rpcombo.c: rpcombo drives KosmicKrisp through Vulkan, and Vulkan
 * cannot express the configuration the instrumented repro actually caught -- a render pass whose
 * colour attachment is a different SIZE from its depth attachment and from renderTargetWidth/
 * Height. In a 79s reproduction of the AGX `bitcode_url` abort, exactly one render-pass
 * configuration was seen for the first time near the end (spikes/agx-compiler-abort/NOTES.md):
 *
 *   rt=800x600  color[0] RGBA8Unorm 2048x2048 load=Load  depth Depth32Float 800x600 load=Clear
 *
 * Both attachments are ordinary 4-byte formats, so the per-pixel byte total is 8 -- a size Apple
 * ships. What is odd is the mismatch: the render target is a 800x600 window into a 2048x2048
 * colour attachment. The background object is the tile-load program, so a partial-coverage
 * target is exactly the kind of thing that could need a variant Apple does not ship.
 *
 * Each case prints its TRY line and flushes BEFORE creating the encoder, because creating the
 * encoder is what triggers the background-object compile and the abort takes the process down
 * from inside Metal. The last line printed names the trigger.
 *
 * RED:   aborts, and the last TRY line is the culprit.
 * GREEN: "swept N cases, no abort" -- size mismatch alone is not the trigger.
 */
#import <Metal/Metal.h>
#import <Metal/MTL4CommandBuffer.h>
#import <Metal/MTL4CommandQueue.h>
#import <Metal/MTL4RenderPass.h>

#include <stdio.h>
#include <stdlib.h>

static id<MTLDevice> dev;
static id<MTLCommandQueue> queue;
static id<MTL4CommandQueue> q4;
static id<MTL4CommandAllocator> alloc4;
static id<MTL4CommandBuffer> cb4;

/* The observed usages, verbatim from the instrumented repro: colour 0x17
 * (ShaderRead|ShaderWrite|RenderTarget|PixelFormatView), depth 0x4 (RenderTarget only). The
 * first version of this test used 0x5 for depth and found nothing -- if the background object
 * is keyed on attachment usage, an approximated usage tests the wrong thing. */
#define COLOR_USAGE 0x17
#define DEPTH_USAGE 0x4

static id<MTLTexture>
make_tex(MTLPixelFormat fmt, NSUInteger w, NSUInteger h, MTLTextureUsage usage)
{
   MTLTextureDescriptor *d = [MTLTextureDescriptor
      texture2DDescriptorWithPixelFormat:fmt width:w height:h mipmapped:NO];
   d.usage = usage;
   d.storageMode = MTLStorageModePrivate;
   return [dev newTextureWithDescriptor:d];
}

static const char *
load_name(MTLLoadAction a)
{
   switch (a) {
   case MTLLoadActionDontCare: return "DontCare";
   case MTLLoadActionLoad:     return "Load";
   case MTLLoadActionClear:    return "Clear";
   }
   return "?";
}

static int cases_run;

/* One render pass. Sizes are deliberately independent so a mismatch can be expressed. */
static void
try_pass(const char *what, NSUInteger cw, NSUInteger ch, MTLLoadAction cload,
         NSUInteger dw, NSUInteger dh, MTLLoadAction dload,
         NSUInteger rtw, NSUInteger rth)
{
   @autoreleasepool {
      printf("TRY %-22s color=%lux%lu/%-8s depth=%lux%lu/%-8s rt=%lux%lu\n", what,
             (unsigned long)cw, (unsigned long)ch, load_name(cload),
             (unsigned long)dw, (unsigned long)dh, load_name(dload),
             (unsigned long)rtw, (unsigned long)rth);
      fflush(stdout);

      id<MTLTexture> color = make_tex(MTLPixelFormatRGBA8Unorm, cw, ch, COLOR_USAGE);
      id<MTLTexture> depth = make_tex(MTLPixelFormatDepth32Float, dw, dh, DEPTH_USAGE);
      if (!color || !depth) {
         printf("    skipped (texture alloc failed)\n");
         return;
      }

      MTLRenderPassDescriptor *rp = [MTLRenderPassDescriptor renderPassDescriptor];
      rp.colorAttachments[0].texture = color;
      rp.colorAttachments[0].loadAction = cload;
      rp.colorAttachments[0].storeAction = MTLStoreActionStore;
      rp.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1);
      rp.depthAttachment.texture = depth;
      rp.depthAttachment.loadAction = dload;
      rp.depthAttachment.storeAction = MTLStoreActionStore;
      rp.depthAttachment.clearDepth = 1.0;
      rp.renderTargetWidth = rtw;
      rp.renderTargetHeight = rth;

      id<MTLCommandBuffer> cb = [queue commandBuffer];
      id<MTLRenderCommandEncoder> enc =
         [cb renderCommandEncoderWithDescriptor:rp];
      [enc endEncoding];
      [cb commit];
      [cb waitUntilCompleted];
      cases_run++;
   }
}

/* The MTL4 variant. KosmicKrisp encodes through MTL4CommandBuffer /
 * MTL4RenderPassDescriptor, not the classic API, and the background object is built by the
 * driver at encoder creation -- so if the trigger is MTL4-specific, only this path can see it. */
static void
try_pass4(const char *what, NSUInteger cw, NSUInteger ch, MTLLoadAction cload,
          NSUInteger dw, NSUInteger dh, MTLLoadAction dload,
          NSUInteger rtw, NSUInteger rth)
{
   @autoreleasepool {
      printf("TRY4 %-21s color=%lux%lu/%-8s depth=%lux%lu/%-8s rt=%lux%lu\n", what,
             (unsigned long)cw, (unsigned long)ch, load_name(cload),
             (unsigned long)dw, (unsigned long)dh, load_name(dload),
             (unsigned long)rtw, (unsigned long)rth);
      fflush(stdout);

      id<MTLTexture> color = make_tex(MTLPixelFormatRGBA8Unorm, cw, ch, COLOR_USAGE);
      id<MTLTexture> depth = make_tex(MTLPixelFormatDepth32Float, dw, dh, DEPTH_USAGE);
      if (!color || !depth) {
         printf("    skipped (texture alloc failed)\n");
         return;
      }

      MTL4RenderPassDescriptor *rp = [[MTL4RenderPassDescriptor alloc] init];
      rp.colorAttachments[0].texture = color;
      rp.colorAttachments[0].loadAction = cload;
      rp.colorAttachments[0].storeAction = MTLStoreActionStore;
      rp.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1);
      rp.depthAttachment.texture = depth;
      rp.depthAttachment.loadAction = dload;
      rp.depthAttachment.storeAction = MTLStoreActionStore;
      rp.depthAttachment.clearDepth = 1.0;
      rp.renderTargetWidth = rtw;
      rp.renderTargetHeight = rth;

      [alloc4 reset];
      [cb4 beginCommandBufferWithAllocator:alloc4];
      id<MTL4RenderCommandEncoder> enc = [cb4 renderCommandEncoderWithDescriptor:rp];
      [enc endEncoding];
      [cb4 endCommandBuffer];
      [q4 commit:&cb4 count:1];
      /* No drain wait: MTL4CommandQueue has no synchronous drain, and none is needed -- the
       * background object is compiled when the encoder is created, which already happened
       * above. Submission is here only so the pass is a real one, not a discarded encode. */
      cases_run++;
   }
}

int
main(void)
{
   dev = MTLCreateSystemDefaultDevice();
   if (!dev) {
      fprintf(stderr, "no Metal device\n");
      return 2;
   }
   queue = [dev newCommandQueue];
   q4 = [dev newMTL4CommandQueue];
   alloc4 = [dev newCommandAllocator];
   cb4 = [dev newCommandBuffer];
   printf("device: %s (MTL4 %s)\n\n", [[dev name] UTF8String],
          (q4 && alloc4 && cb4) ? "available" : "UNAVAILABLE");

   /* Control: everything matched. If this aborts, the vehicle is wrong, not the config. */
   try_pass("matched", 800, 600, MTLLoadActionClear, 800, 600, MTLLoadActionClear, 800, 600);
   try_pass("matched-load", 800, 600, MTLLoadActionLoad, 800, 600, MTLLoadActionClear, 800, 600);

   /* The configuration the instrumented repro caught, and its neighbours. */
   try_pass("observed", 2048, 2048, MTLLoadActionLoad, 800, 600, MTLLoadActionClear, 800, 600);
   try_pass("observed-clear", 2048, 2048, MTLLoadActionClear, 800, 600, MTLLoadActionClear, 800, 600);
   try_pass("observed-dontcare", 2048, 2048, MTLLoadActionDontCare, 800, 600, MTLLoadActionClear, 800, 600);
   try_pass("color-bigger-rt-full", 2048, 2048, MTLLoadActionLoad, 800, 600, MTLLoadActionClear, 2048, 2048);
   try_pass("depth-bigger", 800, 600, MTLLoadActionLoad, 2048, 2048, MTLLoadActionClear, 800, 600);
   try_pass("both-big-rt-small", 2048, 2048, MTLLoadActionLoad, 2048, 2048, MTLLoadActionClear, 800, 600);

   /* A few odd render-target windows, in case it is the window rather than the mismatch. */
   try_pass("rt-tiny", 2048, 2048, MTLLoadActionLoad, 800, 600, MTLLoadActionClear, 1, 1);
   try_pass("rt-odd", 2048, 2048, MTLLoadActionLoad, 800, 600, MTLLoadActionClear, 33, 17);

   if (q4 && alloc4 && cb4) {
      printf("\n-- same cases through MTL4, the API KosmicKrisp actually uses --\n");
      try_pass4("matched", 800, 600, MTLLoadActionClear, 800, 600, MTLLoadActionClear, 800, 600);
      try_pass4("observed", 2048, 2048, MTLLoadActionLoad, 800, 600, MTLLoadActionClear, 800, 600);
      try_pass4("observed-clear", 2048, 2048, MTLLoadActionClear, 800, 600, MTLLoadActionClear, 800, 600);
      try_pass4("observed-dontcare", 2048, 2048, MTLLoadActionDontCare, 800, 600, MTLLoadActionClear, 800, 600);
      try_pass4("depth-bigger", 800, 600, MTLLoadActionLoad, 2048, 2048, MTLLoadActionClear, 800, 600);
      try_pass4("both-big-rt-small", 2048, 2048, MTLLoadActionLoad, 2048, 2048, MTLLoadActionClear, 800, 600);
      try_pass4("rt-odd", 2048, 2048, MTLLoadActionLoad, 800, 600, MTLLoadActionClear, 33, 17);
   }

   printf("\nswept %d cases, no abort\n", cases_run);
   return 0;
}
