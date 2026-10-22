// longfrag — does a long-running fragment shader hit the same cliff on bare Metal?
//
// The piglit ext_timer_query@time-elapsed shader (a loop of `iters` iterations per fragment) over a
// 160x160 target, one draw per command buffer, waited on. Prints the GPU time of every command
// buffer and stops at the first error. Arguments: [start_iters] [max_iters] [repeats] [size].
// Build: clang -fobjc-arc -framework Metal -framework Foundation -o longfrag longfrag.m
// A GPU hang is host-wide: run it only on a host with no VM.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

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

int main(int argc, char **argv)
{
   @autoreleasepool {
      int start = argc > 1 ? atoi(argv[1]) : 1;
      int max = argc > 2 ? atoi(argv[2]) : (1 << 23);
      int repeats = argc > 3 ? atoi(argv[3]) : 5;
      int size = argc > 4 ? atoi(argv[4]) : 160;
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
         texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:size height:size mipmapped:NO];
      td.usage = MTLTextureUsageRenderTarget;
      td.storageMode = MTLStorageModePrivate;
      id<MTLTexture> rt = [dev newTextureWithDescriptor:td];
      id<MTLCommandQueue> q = [dev newCommandQueue];
      printf("device %s, %dx%d target\n", dev.name.UTF8String, size, size);
      int recover = getenv("LONGFRAG_RECOVER") ? atoi(getenv("LONGFRAG_RECOVER")) : 0;
      if (recover > 0) {
         /* After each hang: does a short draw still run, on the hung queue and on a new one? */
         for (int h = 0; h < recover; h++) {
            int sizes[3] = {start, 1024, 1024};
            for (int k = 0; k < 3; k++) {
               id<MTLCommandQueue> qq = k == 2 ? [dev newCommandQueue] : q;
               int it = sizes[k];
               MTLRenderPassDescriptor *rp = [MTLRenderPassDescriptor renderPassDescriptor];
               rp.colorAttachments[0].texture = rt;
               rp.colorAttachments[0].loadAction = MTLLoadActionClear;
               rp.colorAttachments[0].storeAction = MTLStoreActionStore;
               id<MTLCommandBuffer> cb = [qq commandBuffer];
               id<MTLRenderCommandEncoder> e = [cb renderCommandEncoderWithDescriptor:rp];
               [e setRenderPipelineState:ps];
               [e setFragmentBytes:&it length:sizeof(it) atIndex:0];
               [e drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
               [e endEncoding];
               [cb commit];
               [cb waitUntilCompleted];
               printf("round %d %s iters=%d gpu=%.6f s status=%ld error=%s\n", h,
                      k == 0 ? "long " : k == 1 ? "short/same-queue" : "short/new-queue", it,
                      cb.GPUEndTime - cb.GPUStartTime, (long)cb.status,
                      cb.error ? cb.error.localizedDescription.UTF8String : "none");
               fflush(stdout);
            }
            sleep(2);
         }
         return 0;
      }
      for (int iters = start; iters > 0 && iters <= max; iters *= 2) {
         for (int r = 0; r < repeats; r++) {
            MTLRenderPassDescriptor *rp = [MTLRenderPassDescriptor renderPassDescriptor];
            rp.colorAttachments[0].texture = rt;
            rp.colorAttachments[0].loadAction = MTLLoadActionClear;
            rp.colorAttachments[0].storeAction = MTLStoreActionStore;
            id<MTLCommandBuffer> cb = [q commandBuffer];
            id<MTLRenderCommandEncoder> e = [cb renderCommandEncoderWithDescriptor:rp];
            [e setRenderPipelineState:ps];
            [e setFragmentBytes:&iters length:sizeof(iters) atIndex:0];
            [e drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
            [e endEncoding];
            NSDate *t0 = [NSDate date];
            [cb commit];
            [cb waitUntilCompleted];
            double wall = -[t0 timeIntervalSinceNow];
            printf("iters=%d rep=%d gpu=%.6f s wall=%.6f s status=%ld\n", iters, r,
                   cb.GPUEndTime - cb.GPUStartTime, wall, (long)cb.status);
            fflush(stdout);
            if (cb.error) {
               printf("error: %s\n", cb.error.description.UTF8String);
               return 2;
            }
         }
      }
   }
   return 0;
}
