// longfrag4 — what a Metal 4 queue does around a GPU-watchdog hang, the way KosmicKrisp drives it.
//
// Per round, without waiting in between: commit the long draw (the piglit time-elapsed shader at
// `iters`), signal a shared event, commit SHORT draws and a signal after each, then wait for the
// last signal. Prints each commit's feedback (status, GPU time) and which event values arrived.
// It answers what a queue swap on hang has to rely on: do signals and commits queued BEHIND a hung
// command buffer still run, and does that change once the kernel starts ignoring the queue?
// Arguments: [iters] [rounds] [shorts]. LONGFRAG4_SWAP=1 moves to a new queue after any hang,
// ordering it after the old one through the shared event.
// Build: clang -fobjc-arc -framework Metal -framework Foundation -o longfrag4 longfrag4.m
// A GPU hang is host-wide: run it only on a host with no VM.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <stdatomic.h>

static NSString *const src =
   @"#include <metal_stdlib>\n"
    "using namespace metal;\n"
    "struct V { float4 pos [[position]]; };\n"
    "vertex V vs(uint vid [[vertex_id]]) {\n"
    "  float2 p[3] = { float2(-1,-1), float2(3,-1), float2(-1,3) };\n"
    "  V v; v.pos = float4(p[vid], 0, 1); return v;\n"
    "}\n"
    "fragment float4 fs(constant int &iters [[buffer(0)]]) {\n"
    "  int s = 0;\n"
    "  for (int i = 0; i < iters; ++i) {\n"
    "    s += i; if (s >= iters) s -= iters;\n"
    "    s += i; if (s >= iters) s -= iters;\n"
    "  }\n"
    "  return float4(float(s) / float(iters), 1, 0, 0);\n"
    "}\n";

static atomic_int hangs;

int main(int argc, char **argv)
{
   @autoreleasepool {
      int long_iters = argc > 1 ? atoi(argv[1]) : 4194304;
      int rounds = argc > 2 ? atoi(argv[2]) : 3;
      int shorts = argc > 3 ? atoi(argv[3]) : 3;
      bool swap = getenv("LONGFRAG4_SWAP") != NULL;
      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      NSError *err = nil;
      id<MTLLibrary> lib = [dev newLibraryWithSource:src options:nil error:&err];
      if (!lib) { fprintf(stderr, "library: %s\n", err.description.UTF8String); return 1; }
      MTLRenderPipelineDescriptor *pd = [MTLRenderPipelineDescriptor new];
      pd.vertexFunction = [lib newFunctionWithName:@"vs"];
      pd.fragmentFunction = [lib newFunctionWithName:@"fs"];
      pd.colorAttachments[0].pixelFormat = MTLPixelFormatRGBA8Unorm;
      id<MTLRenderPipelineState> ps = [dev newRenderPipelineStateWithDescriptor:pd error:&err];
      if (!ps) { fprintf(stderr, "pipeline: %s\n", err.description.UTF8String); return 1; }
      MTLTextureDescriptor *td = [MTLTextureDescriptor
         texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:160 height:160 mipmapped:NO];
      td.usage = MTLTextureUsageRenderTarget;
      td.storageMode = MTLStorageModePrivate;
      id<MTLTexture> rt = [dev newTextureWithDescriptor:td];

      /* Two constant buffers: the long and the short loop count. */
      id<MTLBuffer> longb = [dev newBufferWithLength:16 options:MTLResourceStorageModeShared];
      id<MTLBuffer> shortb = [dev newBufferWithLength:16 options:MTLResourceStorageModeShared];
      *(int *)longb.contents = long_iters;
      *(int *)shortb.contents = 1024;

      MTLResidencySetDescriptor *rd = [MTLResidencySetDescriptor new];
      id<MTLResidencySet> rs = [dev newResidencySetWithDescriptor:rd error:&err];
      [rs addAllocation:rt];
      [rs addAllocation:longb];
      [rs addAllocation:shortb];
      [rs commit];

      MTL4ArgumentTableDescriptor *ad = [MTL4ArgumentTableDescriptor new];
      ad.maxBufferBindCount = 1;
      id<MTL4ArgumentTable> at_long = [dev newArgumentTableWithDescriptor:ad error:&err];
      id<MTL4ArgumentTable> at_short = [dev newArgumentTableWithDescriptor:ad error:&err];
      [at_long setAddress:longb.gpuAddress atIndex:0];
      [at_short setAddress:shortb.gpuAddress atIndex:0];

      id<MTLSharedEvent> ev = [dev newSharedEvent];
      __block uint64_t value = 0;
      id<MTL4CommandQueue> q = [dev newMTL4CommandQueue];
      [q addResidencySet:rs];
      printf("device %s, MTL4, long iters=%d, %d rounds, %d shorts, swap=%d\n",
             dev.name.UTF8String, long_iters, rounds, shorts, swap);

      void (^commit)(id<MTL4CommandQueue>, bool, NSString *) =
         ^(id<MTL4CommandQueue> qq, bool lng, NSString *what) {
            id<MTL4CommandAllocator> alloc = [dev newCommandAllocator];
            id<MTL4CommandBuffer> cb = [dev newCommandBuffer];
            [cb beginCommandBufferWithAllocator:alloc];
            MTL4RenderPassDescriptor *rp = [MTL4RenderPassDescriptor new];
            rp.colorAttachments[0].texture = rt;
            rp.colorAttachments[0].loadAction = MTLLoadActionClear;
            rp.colorAttachments[0].storeAction = MTLStoreActionStore;
            id<MTL4RenderCommandEncoder> e = [cb renderCommandEncoderWithDescriptor:rp];
            [e setRenderPipelineState:ps];
            [e setArgumentTable:lng ? at_long : at_short atStages:MTLRenderStageFragment];
            [e drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
            [e endEncoding];
            [cb endCommandBuffer];
            MTL4CommitOptions *opt = [MTL4CommitOptions new];
            NSString *label = [what copy];
            [opt addFeedbackHandler:^(id<MTL4CommitFeedback> fb) {
               if (fb.error)
                  atomic_fetch_add(&hangs, 1);
               printf("  feedback %-14s gpu=%.6f s error=%s\n", label.UTF8String,
                      fb.GPUEndTime - fb.GPUStartTime,
                      fb.error ? fb.error.localizedDescription.UTF8String : "none");
               fflush(stdout);
               (void)alloc;
            }];
            const id<MTL4CommandBuffer> cbs[1] = {cb};
            [qq commit:cbs count:1 options:opt];
         };

      for (int r = 0; r < rounds; r++) {
         int before = atomic_load(&hangs);
         printf("round %d\n", r);
         commit(q, true, @"long");
         [q signalEvent:ev value:++value];
         uint64_t after_long = value;
         for (int s = 0; s < shorts; s++) {
            commit(q, false, [NSString stringWithFormat:@"short %d", s]);
            [q signalEvent:ev value:++value];
         }
         NSDate *t0 = [NSDate date];
         while (ev.signaledValue < value && -[t0 timeIntervalSinceNow] < 10.0)
            usleep(1000);
         usleep(200000); /* let the feedback handlers print */
         printf("  event: reached %llu of %llu (the long draw's signal is %llu) after %.3f s\n",
                (unsigned long long)ev.signaledValue, (unsigned long long)value,
                (unsigned long long)after_long, -[t0 timeIntervalSinceNow]);
         if (ev.signaledValue < value) {
            printf("  the queue stopped: a signal behind the hang never fired\n");
            /* Move past it, so the next round measures a new queue rather than this stall. */
         }
         if (swap && atomic_load(&hangs) > before) {
            id<MTL4CommandQueue> nq = [dev newMTL4CommandQueue];
            [nq addResidencySet:rs];
            [nq waitForEvent:ev value:value];
            q = nq;
            printf("  swapped to a new queue, ordered after event value %llu\n",
                   (unsigned long long)value);
         }
         sleep(1);
      }
   }
   return 0;
}
