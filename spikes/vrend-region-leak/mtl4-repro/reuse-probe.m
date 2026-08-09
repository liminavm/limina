// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// PREMISE PROBE for the structural allocator fix (spikes/vrend-region-leak/).
//
// The proposed fix stands entirely on one sentence of Apple's documentation, from
// -[MTL4CommandBuffer beginCommandBuffer(allocator:)]:
//
//    "You can safely reuse command allocators after ending the command buffer using it by
//     calling endCommandBuffer()."
//
// Note what that does NOT say: it does not require GPU completion. Only reset() does
// ("You are responsible to ensure that all command buffers with memory originating from this
// allocator instance are complete before calling resetting it").
//
// If reuse-after-end really is legal while earlier command buffers are still executing, then an
// allocator can host many command buffers back-to-back, growing additively, and be reset once
// later when all of them have drained. That decouples "which allocator am I encoding into" from
// "when may I reset", which is the whole basis of a budget-retired shared pool that never
// destroys anything. If Metal misbehaves here, that design is void and we want to know in an
// afternoon rather than after a refactor.
//
// Four tests, no VM, seconds to run:
//   T1  reuse-while-in-flight: begin/end/commit cb1 on allocator A, then IMMEDIATELY begin cb2
//       on the same A without waiting for cb1. Correctness + does allocatedSize grow additively?
//   T2  reset-then-reuse: after both drain, reset A and encode the same work again. Does
//       allocatedSize stay flat (heaps genuinely reused) rather than climbing?
//   T3  cross-type reuse: encode compute, reset, encode render on the same allocator. Does a
//       type switch defeat heap reuse? (If it does, a shared pool's budget leaks.)
//   T4  handler accumulation: add ONE tagged feedback handler to a reused MTL4CommitOptions
//       before the first commit only, then commit N more times without adding. Count
//       invocations. 1 => Metal snapshots per commit; N => handlers accumulate and re-fire.
//       This settles the undocumented semantics the pending-at-reset instrument assumed.
//
// Env: DISPATCHES=n (work per cb, default 20000 — enough to keep the GPU busy across T1),
//      CBS=n (command buffers per allocator in T1/T2, default 8), COMMITS=n (T4, default 6).

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <stdatomic.h>

static long envl(const char *k, long d)
{
   const char *v = getenv(k);
   return (v && *v) ? atol(v) : d;
}

static double mib(uint64_t b) { return b / 1048576.0; }

int
main(void)
{
   @autoreleasepool {
      long dispatches = envl("DISPATCHES", 20000);
      long cbs = envl("CBS", 8);
      long commits = envl("COMMITS", 6);

      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      if (!dev) { fprintf(stderr, "no Metal device\n"); return 1; }
      if (![dev respondsToSelector:@selector(newCommandAllocator)]) {
         fprintf(stderr, "no MTL4 on this OS\n");
         return 1;
      }

      NSError *err = nil;
      id<MTL4CommandQueue> queue = [dev newMTL4CommandQueue];
      if (!queue) { fprintf(stderr, "no MTL4 queue\n"); return 1; }

      // A trivially small compute pipeline; the point is command-stream volume, not shader work.
      MTL4CompilerDescriptor *cd = [[MTL4CompilerDescriptor new] autorelease];
      id<MTL4Compiler> compiler = [dev newCompilerWithDescriptor:cd error:&err];
      if (!compiler) { fprintf(stderr, "no compiler: %s\n", err.description.UTF8String); return 1; }
      MTLCompileOptions *co = [[MTLCompileOptions new] autorelease];
      co.languageVersion = MTLLanguageVersion3_2;
      MTL4LibraryDescriptor *ld = [[MTL4LibraryDescriptor new] autorelease];
      ld.source = @"#include <metal_stdlib>\n"
                   "kernel void nop(uint tid [[thread_position_in_grid]]) { }\n";
      ld.options = co;
      id<MTLLibrary> lib = [compiler newLibraryWithDescriptor:ld error:&err];
      if (!lib) { fprintf(stderr, "no library: %s\n", err.description.UTF8String); return 1; }
      MTL4LibraryFunctionDescriptor *fd = [[MTL4LibraryFunctionDescriptor new] autorelease];
      fd.name = @"nop";
      fd.library = lib;
      MTL4ComputePipelineDescriptor *pd = [[MTL4ComputePipelineDescriptor new] autorelease];
      pd.computeFunctionDescriptor = fd;
      id<MTLComputePipelineState> pso =
         [compiler newComputePipelineStateWithDescriptor:pd compilerTaskOptions:nil error:&err];
      if (!pso) { fprintf(stderr, "no pipeline: %s\n", err.description.UTF8String); return 1; }

      MTL4ArgumentTableDescriptor *atd = [[MTL4ArgumentTableDescriptor new] autorelease];
      atd.maxBufferBindCount = 1;
      id<MTL4ArgumentTable> argtable = [dev newArgumentTableWithDescriptor:atd error:&err];
      if (!argtable) { fprintf(stderr, "no argument table\n"); return 1; }

      // A render target for T3's cross-type test.
      MTLTextureDescriptor *td = [MTLTextureDescriptor
         texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                                      width:256 height:256 mipmapped:NO];
      td.usage = MTLTextureUsageRenderTarget;
      td.storageMode = MTLStorageModePrivate;
      id<MTLTexture> rtex = [dev newTextureWithDescriptor:td];

      __block atomic_int completions = 0;

      // Encode `dispatches` no-op dispatches into a fresh command buffer on `a`, end it, and
      // return it uncommitted.
      id<MTL4CommandBuffer> (^encodeCompute)(id<MTL4CommandAllocator>) =
         ^id<MTL4CommandBuffer>(id<MTL4CommandAllocator> a) {
            id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
            [cb beginCommandBufferWithAllocator:a];
            id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];
            [enc setArgumentTable:argtable];
            [enc setComputePipelineState:pso];
            for (long d = 0; d < dispatches; d++)
               [enc dispatchThreads:MTLSizeMake(64, 1, 1)
                 threadsPerThreadgroup:MTLSizeMake(64, 1, 1)];
            [enc endEncoding];
            [cb endCommandBuffer];
            return cb;
         };

      MTL4CommitOptions *opts = [[MTL4CommitOptions new] autorelease];
      void (^commitOne)(id<MTL4CommandBuffer>) = ^(id<MTL4CommandBuffer> cb) {
         MTL4CommitOptions *o = [[MTL4CommitOptions new] autorelease];
         [o addFeedbackHandler:^(id<MTL4CommitFeedback> fb) {
           if (fb.error)
              fprintf(stderr, "  !! commit error: %s\n", fb.error.description.UTF8String);
           atomic_fetch_add(&completions, 1);
         }];
         id<MTL4CommandBuffer> __unsafe_unretained buf[1] = {cb};
         [queue commit:buf count:1 options:o];
      };

      // ---- T1: reuse the allocator while earlier command buffers are still in flight --------
      printf("== T1 reuse-while-in-flight (%ld cbs x %ld dispatches on ONE allocator, no waits)\n",
             cbs, dispatches);
      id<MTL4CommandAllocator> A = [dev newCommandAllocator];
      uint64_t prev = [A allocatedSize];
      printf("   start allocatedSize=%.3f MiB\n", mib(prev));
      NSMutableArray *keep = [NSMutableArray array];
      for (long i = 0; i < cbs; i++) {
         id<MTL4CommandBuffer> cb = encodeCompute(A);
         [keep addObject:cb];
         commitOne(cb); // deliberately NOT waiting: cb i-1 is very likely still executing
         uint64_t now = [A allocatedSize];
         printf("   cb%-2ld committed  allocatedSize=%8.3f MiB  (delta %+.3f)  inflight_done=%d\n",
                i, mib(now), mib(now) - mib(prev), atomic_load(&completions));
         prev = now;
      }
      uint64_t afterT1 = [A allocatedSize];

      // Drain before reset — this is the precondition reset actually has.
      while (atomic_load(&completions) < cbs)
         usleep(2000);
      printf("   all %ld drained. allocatedSize=%.3f MiB\n", cbs, mib(afterT1));

      // ---- T2: reset, then encode the same work again. Flat => heaps genuinely reused. ------
      printf("== T2 reset-then-reuse (same allocator, same work again)\n");
      [A reset];
      uint64_t afterReset = [A allocatedSize];
      printf("   after reset  allocatedSize=%.3f MiB\n", mib(afterReset));
      atomic_store(&completions, 0);
      [keep removeAllObjects];
      for (long i = 0; i < cbs; i++) {
         id<MTL4CommandBuffer> cb = encodeCompute(A);
         [keep addObject:cb];
         commitOne(cb);
      }
      while (atomic_load(&completions) < cbs)
         usleep(2000);
      uint64_t afterT2 = [A allocatedSize];
      printf("   second epoch allocatedSize=%.3f MiB  (T1 was %.3f, growth %+.3f MiB)\n",
             mib(afterT2), mib(afterT1), mib(afterT2) - mib(afterT1));

      // ---- T3: cross-type reuse. Does a render pass on a compute-warmed allocator grow it? --
      printf("== T3 cross-type reuse (render on a compute-warmed allocator)\n");
      [A reset];
      atomic_store(&completions, 0);
      MTL4RenderPipelineDescriptor *rpd = [[MTL4RenderPipelineDescriptor new] autorelease];
      MTL4LibraryDescriptor *rld = [[MTL4LibraryDescriptor new] autorelease];
      rld.source = @"#include <metal_stdlib>\n"
                    "using namespace metal;\n"
                    "vertex float4 vs(uint i [[vertex_id]]) { return float4(0,0,0,1); }\n"
                    "fragment float4 fs() { return float4(1,0,0,1); }\n";
      rld.options = co;
      id<MTLLibrary> rlib = [compiler newLibraryWithDescriptor:rld error:&err];
      uint64_t beforeT3 = [A allocatedSize];
      if (rlib && rtex) {
         MTL4LibraryFunctionDescriptor *vfd = [[MTL4LibraryFunctionDescriptor new] autorelease];
         vfd.name = @"vs"; vfd.library = rlib;
         MTL4LibraryFunctionDescriptor *ffd = [[MTL4LibraryFunctionDescriptor new] autorelease];
         ffd.name = @"fs"; ffd.library = rlib;
         rpd.vertexFunctionDescriptor = vfd;
         rpd.fragmentFunctionDescriptor = ffd;
         rpd.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
         id<MTLRenderPipelineState> rpso =
            [compiler newRenderPipelineStateWithDescriptor:rpd compilerTaskOptions:nil error:&err];
         if (rpso) {
            for (long i = 0; i < cbs; i++) {
               id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
               [cb beginCommandBufferWithAllocator:A];
               MTL4RenderPassDescriptor *rp = [[MTL4RenderPassDescriptor new] autorelease];
               rp.colorAttachments[0].texture = rtex;
               rp.colorAttachments[0].loadAction = MTLLoadActionClear;
               rp.colorAttachments[0].storeAction = MTLStoreActionStore;
               rp.renderTargetWidth = 256; rp.renderTargetHeight = 256;
               id<MTL4RenderCommandEncoder> renc = [cb renderCommandEncoderWithDescriptor:rp];
               [renc setRenderPipelineState:rpso];
               for (long d = 0; d < 200; d++)
                  [renc drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
               [renc endEncoding];
               [cb endCommandBuffer];
               commitOne(cb);
            }
            while (atomic_load(&completions) < cbs)
               usleep(2000);
         } else {
            fprintf(stderr, "   (render pipeline unavailable: %s)\n", err.description.UTF8String);
         }
      }
      printf("   before=%.3f MiB  after render epoch=%.3f MiB  (%+.3f)\n",
             mib(beforeT3), mib([A allocatedSize]), mib([A allocatedSize]) - mib(beforeT3));

      // ---- T4: do feedback handlers accumulate on a reused MTL4CommitOptions? ---------------
      printf("== T4 handler accumulation (one tagged handler, then %ld more commits)\n", commits);
      __block atomic_int tagged = 0;
      MTL4CommitOptions *shared = [[MTL4CommitOptions new] autorelease];
      [shared addFeedbackHandler:^(id<MTL4CommitFeedback> fb) {
        (void)fb;
        atomic_fetch_add(&tagged, 1);
      }];
      id<MTL4CommandAllocator> B = [dev newCommandAllocator];
      for (long i = 0; i < commits; i++) {
         id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
         [cb beginCommandBufferWithAllocator:B];
         id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];
         [enc setArgumentTable:argtable];
         [enc setComputePipelineState:pso];
         [enc dispatchThreads:MTLSizeMake(64, 1, 1)
           threadsPerThreadgroup:MTLSizeMake(64, 1, 1)];
         [enc endEncoding];
         [cb endCommandBuffer];
         id<MTL4CommandBuffer> __unsafe_unretained buf[1] = {cb};
         [queue commit:buf count:1 options:shared]; // NO new handler added
         usleep(30000);
      }
      usleep(400000);
      printf("   tagged handler fired %d time(s) across %ld commits\n", atomic_load(&tagged),
             commits);
      printf("   => 1 means Metal snapshots the handler list per commit;\n");
      printf("      %ld means handlers accumulate and re-fire for later commits.\n", commits);

      printf("\nSUMMARY\n");
      printf("  T1 reuse-while-in-flight: allocatedSize %.3f MiB over %ld cbs, no reset\n",
             mib(afterT1), cbs);
      printf("  T2 second epoch after reset: %.3f MiB (growth %+.3f MiB)\n", mib(afterT2),
             mib(afterT2) - mib(afterT1));
      printf("  T4 handler fires: %d\n", atomic_load(&tagged));
   }
   return 0;
}
