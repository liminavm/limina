// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Standalone host-side repro for the IOAccelerator (graphics) region ratchet.
// NO VM, NO virglrenderer, NO vrend, NO guest — just Metal, in the shape KosmicKrisp drives it.
//
// WHY THIS VEHICLE. The in-VM repro costs a boot plus minutes per vmmap and drags the entire
// stack (guest, virtio-gpu, virglrenderer, vrend, host zink, KK, Metal) into every measurement.
// ioclasscount established that the leaked objects are AGX *kernel* resources with no
// bridge-minted Objective-C wrapper — i.e. Metal-internal. If that is right, the whole stack
// above Metal is irrelevant and the ratchet must reproduce from a few dozen lines. If it does
// NOT reproduce here, that is equally informative: it puts the fault back above Metal.
//
// THE SHAPE, mirrored from KosmicKrisp (verify before trusting — kk_queue.c / kk_cmd_buffer.c):
//   - ONE MTL4CommandQueue and ONE MTL4CommitOptions, both created once at queue init
//     (kk_queue.c:152) and reused for the process lifetime.
//   - THREE MTL4CommandAllocators per command buffer — pre_gfx, gfx, post_gfx
//     (kk_init_encoder_state, kk_cmd_buffer.c:~103) — created once and `reset` for reuse
//     (kk_reset_encoder_state, :172), never destroyed while the VkCommandBuffer is pooled.
//   - Per submit: a FRESH MTL4CommandBuffer per encoder (kk_cmd_buffer.c:314), and a FRESH
//     feedback handler appended to the queue-lifetime commit options (kk_queue.c:91).
//
// It self-measures with vmmap --summary on its own pid, so attribution needs no assumption:
// whatever it grows, it grew itself.
//
// ⚠ It is a GPU client, so it perturbs system-wide ioclasscount. Run it when no VM snapshot
// series is in flight, and trust its own vmmap over any system-wide counter.
//
// A/B knobs — change ONE at a time, that is the entire point:
//   ITERS=n         iterations (default 3000)
//   ENCODERS=n      command buffers (and allocators) per iteration (default 3, as KK)
//   NOHANDLER=1     skip addFeedbackHandler — tests handler accumulation on reused options
//   FRESHOPTS=1     new MTL4CommitOptions every iteration instead of reusing one
//   RECREATE=1      new allocators every iteration instead of `reset`
//   NORESET=1       neither reset nor recreate (expected to grow; sanity anchor)
//   NOCOMMIT=1      build and release command buffers but never commit
//
//   ALLOC=n         per iteration, run n full kk_alloc_bo/kk_destroy_bo cycles: create an
//                   MTLHeap (placement, shared, untracked, sparse-16 — KK_MTL_RESOURCE_OPTIONS
//                   + mtl_new_heap) plus a buffer from it, add the heap to a residency set,
//                   commit + requestResidency, then remove and release both. This is the
//                   variant that tests the actual hypothesis: the ObjC sentinel census says
//                   these objects DIE, while ioclasscount says ~25k AGX kernel resources per
//                   cycle do NOT. If releasing a heap fails to return the kernel resource, it
//                   shows up here with nothing else in the process.
//   SIZE=n          heap size in bytes (default 32768 — the dominant leaked region size)
//   NORESIDENCY=1   skip the residency set entirely (isolates it as the retainer)
//   TEX=n           per iteration, create and release n MTLTextures (the other resource type
//                   KK mints; a texture carries more kernel state than a heap — compression
//                   metadata, per-level descriptors — so it is the natural second suspect once
//                   plain heaps come back clean)
//   TEXDIM=n        texture edge in pixels (default 256)
//   ENCODE=1        actually ENCODE: a compute encoder with a real pipeline and dispatch in
//                   every command buffer, instead of begin/end with nothing between. This is
//                   the variant that matters — an MTL4CommandAllocator only consumes chunk
//                   memory when commands are recorded into it, so empty command buffers
//                   exercise none of the machinery the 32K regions are suspected to be.
//   DISPATCHES=n    dispatches per encoder (default 1) — scales chunk consumption
//   RENDER=1        do a real RENDER PASS per iteration into a colour attachment, with a
//                   vertex+fragment pipeline and a draw. The prime suspect: the leaked bytes
//                   are 2 394 × 1024K + 2 393 × 768K regions tracking each other 1:1, i.e. ~1 197
//                   PAIRS per workload cycle — and 40 s of the aquarium at ~30 fps is ~1 200
//                   frames. A tiler allocates parameter/tile buffers per render pass, they are
//                   Metal-internal (no bridge-minted wrapper, matching the census), and the
//                   pair shape is exactly right. RENDERDIM sets the attachment size — tiler
//                   buffer size scales with render-target area, so this is the knob that should
//                   move the 1024K/768K sizes if the hypothesis holds.
//   RENDERDIM=n     render target edge (default 1024)
//   FRESHRT=1       render into a NEWLY CREATED target every iteration (and release it), instead
//                   of reusing one. THE KEY DIFFERENTIAL. Two earlier variants each missed half
//                   of this: TEX=n created and released textures but never RENDERED into them,
//                   and RENDER=1 rendered but reused a single target forever. A tiled renderer
//                   allocates tiler/parameter state lazily on first use and caches it against
//                   the render target, so neither variant could expose state that is minted
//                   per-target and never reclaimed. The real stack presents a fresh target
//                   constantly (guest FBOs, the rotating scanout surface).
//
// Build/run: spikes/vrend-region-leak/mtl4-repro/run.sh

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static int envflag(const char *n) { const char *e = getenv(n); return e && *e && strcmp(e, "0"); }
static long envnum(const char *n, long dflt) { const char *e = getenv(n); return (e && *e) ? strtol(e, NULL, 10) : dflt; }

// Region + footprint for OUR pid, PLUS the kernel-side AGXResource count.
//
// Two lenses on purpose. "regions flat" only ever proves nothing leaked INTO THAT VMMAP TAG —
// an allocation landing under a different tag, or one with no user-space mapping at all, would
// read as flat while still ratcheting kernel objects. AGXResource is the counter that actually
// tracked the in-VM leak 1:1 (spikes/vrend-region-leak/data/), so it is the one that must stay
// flat before "clean" means anything. It is SYSTEM-WIDE, so read its delta, not its value, and
// only trust deltas far above the ±33 host noise floor measured in ioclass-cycle.sh.
static void sample(const char *tag)
{
   char cmd[768];
   snprintf(cmd, sizeof cmd,
            "vmmap --summary %d 2>/dev/null | awk '/^IOAccelerator \\(graphics\\)/{printf \"regions=%%-8s size=%%-8s \", $NF, $3} "
            "/Physical footprint:/{printf \"footprint=%%-8s\", $3}'; "
            "ioclasscount 2>/dev/null | awk '/^AGXResource = /{printf \"AGXResource=%%s\", $3}'",
            getpid());
   printf("  %-14s ", tag);
   fflush(stdout);
   int rc = system(cmd);
   (void)rc;
   printf("\n");
   fflush(stdout);
}

int main(void)
{
   @autoreleasepool {
      const long iters    = envnum("ITERS", 3000);
      const long encoders = envnum("ENCODERS", 3);
      const int  nohandler = envflag("NOHANDLER");
      const int  freshopts = envflag("FRESHOPTS");
      const int  recreate  = envflag("RECREATE");
      const int  noreset   = envflag("NORESET");
      const int  nocommit  = envflag("NOCOMMIT");
      const long alloc     = envnum("ALLOC", 0);
      const long size      = envnum("SIZE", 32768);
      const int  noresidency = envflag("NORESIDENCY");
      const long tex       = envnum("TEX", 0);
      const long texdim    = envnum("TEXDIM", 256);
      const int  encode    = envflag("ENCODE");
      const long dispatches = envnum("DISPATCHES", 1);
      const int  render    = envflag("RENDER");
      const long renderdim = envnum("RENDERDIM", 1024);
      const int  freshrt   = envflag("FRESHRT");
      /* RESET_EVERY=n resets the allocators once per n iterations instead of every one. This is
       * the knob that separates a BOUNDED leak from an unbounded one: if the pool's high-water
       * is what persists, regions plateau at roughly n render passes' worth and stay there no
       * matter how long the run goes; if they keep climbing, reset is not reclaiming at all.
       * KK's shape is "reset once per vkResetCommandBuffer, arbitrarily many render passes in
       * between", which is exactly large n. */
      const long reset_every = envnum("RESET_EVERY", 1);

      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      if (!dev) { fprintf(stderr, "no Metal device\n"); return 1; }

      id<MTL4CommandQueue> queue = [dev newMTL4CommandQueue];
      if (!queue) { fprintf(stderr, "no MTL4 command queue\n"); return 1; }

      MTL4CommitOptions *opts = [[MTL4CommitOptions alloc] init];

      // One device-lifetime residency set, exactly as kk_device holds (kk_device.c).
      id<MTLResidencySet> residency = nil;
      if (alloc > 0 && !noresidency) {
         MTLResidencySetDescriptor *rd = [[[MTLResidencySetDescriptor alloc] init] autorelease];
         rd.initialCapacity = 100;
         NSError *err = nil;
         residency = [dev newResidencySetWithDescriptor:rd error:&err];
         if (!residency) { fprintf(stderr, "no residency set: %s\n", err.description.UTF8String); return 1; }
         [queue addResidencySet:residency];
      }

      // Allocators created once and reused, exactly as KK pools them.
      NSMutableArray *allocators = [NSMutableArray array];
      for (long i = 0; i < encoders; i++) {
         id<MTL4CommandAllocator> a = [dev newCommandAllocator];
         if (!a) { fprintf(stderr, "no command allocator\n"); return 1; }
         [allocators addObject:a];
      }

      // Pipeline built once, as any driver would — we are testing per-submit cost, not
      // compilation. Mirrors mtl_new_library / mtl_new_compute_pipeline_state in mtl_compiler.m.
      id<MTLComputePipelineState> pso = nil;
      id<MTL4ArgumentTable> argtable = nil;
      if (encode) {
         NSError *err = nil;
         MTL4CompilerDescriptor *cd = [[MTL4CompilerDescriptor new] autorelease];
         id<MTL4Compiler> compiler = [dev newCompilerWithDescriptor:cd error:&err];
         if (!compiler) { fprintf(stderr, "no MTL4Compiler: %s\n", err.description.UTF8String); return 1; }

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
         pso = [compiler newComputePipelineStateWithDescriptor:pd compilerTaskOptions:nil error:&err];
         if (!pso) { fprintf(stderr, "no pipeline: %s\n", err.description.UTF8String); return 1; }

         MTL4ArgumentTableDescriptor *atd = [[MTL4ArgumentTableDescriptor new] autorelease];
         atd.maxBufferBindCount = 1;
         argtable = [dev newArgumentTableWithDescriptor:atd error:&err];
         if (!argtable) { fprintf(stderr, "no argument table: %s\n", err.description.UTF8String); return 1; }
      }

      // Render-pass vehicle: one persistent colour attachment, one pipeline, built once.
      id<MTLRenderPipelineState> rpso = nil;
      id<MTLTexture> rtex = nil;
      if (render) {
         NSError *err = nil;
         MTL4CompilerDescriptor *cd = [[MTL4CompilerDescriptor new] autorelease];
         id<MTL4Compiler> rcompiler = [dev newCompilerWithDescriptor:cd error:&err];
         if (!rcompiler) { fprintf(stderr, "no MTL4Compiler: %s\n", err.description.UTF8String); return 1; }

         MTLCompileOptions *co = [[MTLCompileOptions new] autorelease];
         co.languageVersion = MTLLanguageVersion3_2;
         MTL4LibraryDescriptor *ld = [[MTL4LibraryDescriptor new] autorelease];
         ld.source = @"#include <metal_stdlib>\n"
                      "using namespace metal;\n"
                      "vertex float4 vmain(uint v [[vertex_id]]) {\n"
                      "  float2 p[3] = {float2(-1,-1), float2(3,-1), float2(-1,3)};\n"
                      "  return float4(p[v], 0, 1);\n"
                      "}\n"
                      "fragment float4 fmain() { return float4(0.25, 0.5, 0.75, 1); }\n";
         ld.options = co;
         id<MTLLibrary> rlib = [rcompiler newLibraryWithDescriptor:ld error:&err];
         if (!rlib) { fprintf(stderr, "no render library: %s\n", err.description.UTF8String); return 1; }

         MTL4LibraryFunctionDescriptor *vfd = [[MTL4LibraryFunctionDescriptor new] autorelease];
         vfd.name = @"vmain"; vfd.library = rlib;
         MTL4LibraryFunctionDescriptor *ffd = [[MTL4LibraryFunctionDescriptor new] autorelease];
         ffd.name = @"fmain"; ffd.library = rlib;

         MTL4RenderPipelineDescriptor *rpd = [[MTL4RenderPipelineDescriptor new] autorelease];
         rpd.vertexFunctionDescriptor = vfd;
         rpd.fragmentFunctionDescriptor = ffd;
         rpd.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
         rpso = [rcompiler newRenderPipelineStateWithDescriptor:rpd compilerTaskOptions:nil error:&err];
         if (!rpso) { fprintf(stderr, "no render pipeline: %s\n", err.description.UTF8String); return 1; }

         MTLTextureDescriptor *rtd = [MTLTextureDescriptor
            texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                                         width:(NSUInteger)renderdim
                                        height:(NSUInteger)renderdim
                                     mipmapped:NO];
         rtd.usage = MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead;
         rtd.storageMode = MTLStorageModePrivate;
         rtex = [dev newTextureWithDescriptor:rtd];
         if (!rtex) { fprintf(stderr, "no render target\n"); return 1; }
         if (residency) { [residency addAllocation:rtex]; [residency commit]; [residency requestResidency]; }
      }

      printf("mtl4cycle: iters=%ld encoders=%ld%s%s%s%s%s\n", iters, encoders,
             nohandler ? " NOHANDLER" : "", freshopts ? " FRESHOPTS" : "",
             recreate ? " RECREATE" : "", noreset ? " NORESET" : "",
             nocommit ? " NOCOMMIT" : "");
      printf("  device=%s\n", dev.name.UTF8String);
      sample("start");

      const long step = iters / 6 > 0 ? iters / 6 : 1;

      for (long it = 0; it < iters; it++) {
         @autoreleasepool {
            // kk_alloc_bo -> residency -> kk_destroy_bo, in full, n times per iteration.
            for (long a = 0; a < alloc; a++) {
               MTLHeapDescriptor *hd = [[MTLHeapDescriptor new] autorelease];
               hd.type = MTLHeapTypePlacement;
               hd.resourceOptions = MTLResourceStorageModeShared |
                                    MTLResourceCPUCacheModeDefaultCache |
                                    MTLResourceHazardTrackingModeUntracked;
               hd.size = (NSUInteger)size;
               hd.sparsePageSize = MTLSparsePageSize16;
               id<MTLHeap> heap = [dev newHeapWithDescriptor:hd];
               if (!heap) { fprintf(stderr, "heap alloc failed at iter %ld\n", it); return 1; }
               id<MTLBuffer> buf = [heap newBufferWithLength:(NSUInteger)size options:hd.resourceOptions];

               if (residency) {
                  [residency addAllocation:heap];
                  [residency commit];
                  [residency requestResidency];
               }

               // The free path, mirroring kk_destroy_bo: remove from the set, release both.
               if (residency) {
                  [residency removeAllocation:heap];
                  [residency commit];
               }
               [buf release];
               [heap release];
            }

            for (long t = 0; t < tex; t++) {
               MTLTextureDescriptor *td = [MTLTextureDescriptor
                  texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                                               width:(NSUInteger)texdim
                                              height:(NSUInteger)texdim
                                           mipmapped:NO];
               td.usage = MTLTextureUsageShaderRead | MTLTextureUsageRenderTarget;
               td.storageMode = MTLStorageModePrivate;
               id<MTLTexture> tx = [dev newTextureWithDescriptor:td];
               if (!tx) { fprintf(stderr, "texture alloc failed at iter %ld\n", it); return 1; }
               if (residency) { [residency addAllocation:tx]; [residency commit]; [residency requestResidency]; }
               if (residency) { [residency removeAllocation:tx]; [residency commit]; }
               [tx release];
            }

            if (recreate) {
               [allocators removeAllObjects];
               for (long i = 0; i < encoders; i++)
                  [allocators addObject:[dev newCommandAllocator]];
            }

            NSMutableArray *cmds = [NSMutableArray array];
            for (long i = 0; i < encoders; i++) {
               // Fresh command buffer per encoder, as kk_start_compute_encoder does.
               id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
               [cb beginCommandBufferWithAllocator:allocators[i]];
               if (render) {
                  id<MTLTexture> target = rtex;
                  if (freshrt) {
                     MTLTextureDescriptor *ftd = [MTLTextureDescriptor
                        texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                                                     width:(NSUInteger)renderdim
                                                    height:(NSUInteger)renderdim
                                                 mipmapped:NO];
                     ftd.usage = MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead;
                     ftd.storageMode = MTLStorageModePrivate;
                     target = [[dev newTextureWithDescriptor:ftd] autorelease];
                     if (!target) { fprintf(stderr, "fresh RT alloc failed at iter %ld\n", it); return 1; }
                     if (residency) { [residency addAllocation:target]; [residency commit]; [residency requestResidency]; }
                  }
                  MTL4RenderPassDescriptor *rp = [[MTL4RenderPassDescriptor new] autorelease];
                  rp.colorAttachments[0].texture = target;
                  rp.colorAttachments[0].loadAction = MTLLoadActionClear;
                  rp.colorAttachments[0].storeAction = MTLStoreActionStore;
                  rp.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1);
                  rp.renderTargetWidth = (NSUInteger)renderdim;
                  rp.renderTargetHeight = (NSUInteger)renderdim;
                  id<MTL4RenderCommandEncoder> renc = [cb renderCommandEncoderWithDescriptor:rp];
                  [renc setRenderPipelineState:rpso];
                  [renc drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
                  [renc endEncoding];
               }
               if (encode) {
                  // kk_start_compute_encoder -> ... -> kk_stop_encoder, in miniature.
                  id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];
                  [enc setArgumentTable:argtable];
                  [enc setComputePipelineState:pso];
                  for (long d = 0; d < dispatches; d++)
                     [enc dispatchThreads:MTLSizeMake(64, 1, 1)
                       threadsPerThreadgroup:MTLSizeMake(64, 1, 1)];
                  [enc barrierAfterQueueStages:MTLStageAll beforeStages:MTLStageAll
                                visibilityOptions:MTL4VisibilityOptionDevice];
                  [enc endEncoding];
               }
               [cb endCommandBuffer];
               [cmds addObject:cb];
            }

            if (!nocommit) {
               MTL4CommitOptions *use = opts;
               if (freshopts) use = [[[MTL4CommitOptions alloc] init] autorelease];
               if (!nohandler) {
                  // KK appends a NEW handler to the SAME queue-lifetime options on every
                  // submit (kk_queue.c:91). Whether Metal clears the list at commit is
                  // undocumented — NOHANDLER=1 is the A/B that answers it.
                  [use addFeedbackHandler:^(id<MTL4CommitFeedback> fb) { (void)fb; }];
               }
               id<MTL4CommandBuffer> __unsafe_unretained buf[16];
               NSUInteger n = cmds.count < 16 ? cmds.count : 16;
               for (NSUInteger i = 0; i < n; i++) buf[i] = cmds[i];
               [queue commit:buf count:n options:use];
            }

            if (!noreset && !recreate && ((it + 1) % reset_every) == 0)
               for (long i = 0; i < encoders; i++)
                  [allocators[i] reset];
         }

         if ((it + 1) % step == 0) {
            char tag[32];
            snprintf(tag, sizeof tag, "iter %ld", it + 1);
            sample(tag);
         }
      }

      // Settle: the in-VM protocol waits 30 s after close before reading, so anything deferred
      // has run. Match it, or a "leak" here may just be work Metal has not retired yet.
      sleep(10);
      sample("settled");
   }
   return 0;
}
