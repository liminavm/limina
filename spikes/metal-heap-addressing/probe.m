// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * metal-heap-addressing: which Metal-chosen values could a guest driver compute
 * instead of asking the host for them?
 *
 * The shape under test is a guest driver that sub-allocates out of a few large
 * host-owned placement heaps mapped into guest-physical space (the way venus
 * host-visible blobs are mapped today: the KK heap's alias-buffer contents,
 * hv_vm_map'd into the virtio-gpu SHM window). KosmicKrisp allocates exactly
 * this way per BO: a placement heap, StorageModeShared | untracked, plus one
 * buffer aliasing the whole heap at offset 0 (kk_bo.c, mtl_heap.m).
 *
 * Sections (each prints its own facts; no verdicts):
 *   limits   device limits; largest shared placement heap that can be created
 *   addr     placed buffer gpuAddress/contents vs alias base + offset, both creation orders,
 *            several heaps; then a GPU kernel dereferences base+offset addresses it was
 *            never given a buffer for, and the CPU checks the writes
 *   align    heapBufferSizeAndAlign over lengths; heapTextureSizeAndAlign over descriptors
 *   ids      gpuResourceID for textures, views, samplers: values, reuse after release
 *   layout   bytes of an optimized-layout shared heap texture, read through the alias
 *   tput     GPU copy throughput, shared vs private heap textures (indicative)
 *   hv       hv_vm_map a large heap alias into a VM in one call (needs the entitlement)
 *
 * Usage: ./probe [section...]   (default: all but hv; build.sh signs for hv)
 */

#include <Hypervisor/Hypervisor.h>
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include <mach/mach_time.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MiB (1ULL << 20)
#define GiB (1ULL << 30)

static const MTLResourceOptions KK_OPTS = MTLResourceStorageModeShared |
                                          MTLResourceCPUCacheModeDefaultCache |
                                          MTLResourceHazardTrackingModeUntracked;
static const MTLResourceOptions PRIV_OPTS =
    MTLResourceStorageModePrivate | MTLResourceHazardTrackingModeUntracked;

static id<MTLDevice> dev;
static id<MTLCommandQueue> queue;

static double now_ms(void)
{
   static mach_timebase_info_data_t tb;
   if (!tb.denom)
      mach_timebase_info(&tb);
   return (double)mach_absolute_time() * tb.numer / tb.denom / 1e6;
}

static id<MTLHeap> new_heap(uint64_t size, MTLResourceOptions opts)
{
   MTLHeapDescriptor *d = [MTLHeapDescriptor new];
   d.type = MTLHeapTypePlacement;
   d.resourceOptions = opts;
   d.size = size;
   return [dev newHeapWithDescriptor:d];
}

/* ------------------------------------------------------------------ limits */

static void sec_limits(void)
{
   printf("== limits\n");
   printf("device=%s maxBufferLength=%llu MiB recommendedMaxWorkingSetSize=%llu MiB "
          "hasUnifiedMemory=%d argumentBuffersSupport=%ld\n",
          dev.name.UTF8String, (unsigned long long)(dev.maxBufferLength / MiB),
          (unsigned long long)(dev.recommendedMaxWorkingSetSize / MiB), dev.hasUnifiedMemory,
          (long)dev.argumentBuffersSupport);
   for (uint64_t sz = 256 * MiB; sz <= 32 * GiB; sz *= 2) {
      double t0 = now_ms();
      id<MTLHeap> h = new_heap(sz, KK_OPTS);
      double t1 = now_ms();
      id<MTLBuffer> alias = h ? [h newBufferWithLength:sz options:KK_OPTS offset:0] : nil;
      double t2 = now_ms();
      printf("heap %6llu MiB: heap=%s (%.2f ms) alias=%s (%.2f ms) contents=%p align16k=%d "
             "gpu=0x%llx currentAllocatedSize=%llu MiB\n",
             (unsigned long long)(sz / MiB), h ? "ok" : "nil", t1 - t0, alias ? "ok" : "nil",
             t2 - t1, alias ? alias.contents : NULL,
             alias ? (((uintptr_t)alias.contents & 0x3fff) == 0) : -1,
             alias ? (unsigned long long)alias.gpuAddress : 0ULL,
             h ? (unsigned long long)(h.currentAllocatedSize / MiB) : 0ULL);
      if (!alias)
         break;
   }
}

/* -------------------------------------------------------------------- addr */

static NSString *const ADDR_MSL =
   @"#include <metal_stdlib>\n"
    "using namespace metal;\n"
    "struct Target { device uint *p; };\n"
    "kernel void poke(device const Target *t [[buffer(0)]], uint i [[thread_position_in_grid]]) {\n"
    "   t[i].p[0] = 0xC0DE0000u | i;\n"
    "   t[i].p[1] = ~(0xC0DE0000u | i);\n"
    "}\n";

static void sec_addr(void)
{
   printf("== addr\n");
   const uint64_t HS = 256 * MiB;
   enum { NH = 3, NB = 64, NT = NH * 32 };
   id<MTLHeap> heaps[NH];
   id<MTLBuffer> alias[NH];
   int mism_gpu = 0, mism_cpu = 0, total = 0;
   NSMutableArray *placed = [NSMutableArray array];
   uint64_t offs[NH][NB];
   srandom(42);

   for (int h = 0; h < NH; h++) {
      heaps[h] = new_heap(HS, KK_OPTS);
      /* heap 1: place buffers BEFORE the alias exists, to see the address does not
       * depend on the alias having been made first. */
      bool alias_first = (h != 1);
      if (alias_first)
         alias[h] = [heaps[h] newBufferWithLength:HS options:KK_OPTS offset:0];
      uint64_t off = 0;
      id<MTLBuffer> bufs[NB];
      int nb = 0;
      for (int b = 0; b < NB; b++, nb++) {
         uint64_t len = (uint64_t)(random() % (2 * MiB)) + 1;
         MTLSizeAndAlign sa = [dev heapBufferSizeAndAlignWithLength:len options:KK_OPTS];
         off = (off + sa.align - 1) / sa.align * sa.align;
         if (off + sa.size > HS)
            break;
         bufs[b] = [heaps[h] newBufferWithLength:len options:KK_OPTS offset:off];
         offs[h][b] = off;
         [placed addObject:bufs[b]];
         off += sa.size + (uint64_t)(random() % 3) * sa.align;
      }
      if (!alias_first)
         alias[h] = [heaps[h] newBufferWithLength:HS options:KK_OPTS offset:0];
      for (int b = 0; b < nb; b++) {
         id<MTLBuffer> buf = bufs[b];
         total++;
         if (buf.gpuAddress != alias[h].gpuAddress + offs[h][b])
            mism_gpu++;
         if ((uint8_t *)buf.contents != (uint8_t *)alias[h].contents + offs[h][b])
            mism_cpu++;
      }
      printf("heap %d (alias %s): alias gpu=0x%llx cpu=%p; heap-to-heap gpu delta=%lld MiB\n", h,
             alias_first ? "first" : "last", (unsigned long long)alias[h].gpuAddress,
             alias[h].contents,
             h ? (long long)((int64_t)(alias[h].gpuAddress - alias[h - 1].gpuAddress) /
                             (int64_t)MiB)
               : 0LL);
   }
   printf("placed buffers: %d; gpuAddress != alias+offset: %d; contents != alias+offset: %d\n",
          total, mism_gpu, mism_cpu);

   /* The GPU dereferences addresses computed as alias.gpuAddress + offset. It is given
    * no MTLBuffer for any target: residency comes from useHeap, as a guest driver's
    * would from the heap list, and the pointer from arithmetic. */
   NSError *err = nil;
   id<MTLLibrary> lib = [dev newLibraryWithSource:ADDR_MSL options:nil error:&err];
   id<MTLComputePipelineState> ps =
      [dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"poke"] error:&err];
   if (!ps) {
      printf("pipeline failed: %s\n", err.localizedDescription.UTF8String);
      return;
   }

   id<MTLBuffer> targets = [dev newBufferWithLength:NT * 8 options:MTLResourceStorageModeShared];
   uint64_t *tp = targets.contents;
   uint8_t *cpu_at[NT];
   for (int i = 0; i < NT; i++) {
      int h = i % NH;
      uint64_t off = ((uint64_t)(random() % (HS / 16 - 1))) * 16;
      tp[i] = alias[h].gpuAddress + off;
      cpu_at[i] = (uint8_t *)alias[h].contents + off;
      memset(cpu_at[i], 0, 8);
   }
   id<MTLCommandBuffer> cb = [queue commandBuffer];
   id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
   [enc setComputePipelineState:ps];
   [enc setBuffer:targets offset:0 atIndex:0];
   for (int h = 0; h < NH; h++)
      [enc useHeap:heaps[h]];
   [enc dispatchThreads:MTLSizeMake(NT, 1, 1) threadsPerThreadgroup:MTLSizeMake(32, 1, 1)];
   [enc endEncoding];
   [cb commit];
   [cb waitUntilCompleted];
   int bad = 0;
   for (int i = 0; i < NT; i++) {
      uint32_t *w = (uint32_t *)cpu_at[i];
      if (w[0] != (0xC0DE0000u | i) || w[1] != ~(0xC0DE0000u | i))
         bad++;
   }
   printf("GPU writes through computed addresses: %d targets over %d heaps, %d wrong (cb status "
          "%ld%s%s)\n",
          NT, NH, bad, (long)cb.status, cb.error ? " error: " : "",
          cb.error ? cb.error.localizedDescription.UTF8String : "");
}

/* ------------------------------------------------------------------- align */

static void sec_align(void)
{
   printf("== align\n");
   const uint64_t lens[] = {1, 16, 255, 256, 4095, 4096, 4097, 16383, 16384, 16385,
                            65536, 65537, 1 * MiB + 1, 16 * MiB + 3, 256 * MiB};
   MTLResourceOptions opts[] = {KK_OPTS, PRIV_OPTS};
   const char *on[] = {"shared", "private"};
   for (int o = 0; o < 2; o++)
      for (unsigned i = 0; i < sizeof(lens) / sizeof(lens[0]); i++) {
         MTLSizeAndAlign sa = [dev heapBufferSizeAndAlignWithLength:lens[i] options:opts[o]];
         printf("buffer %-7s len=%-10llu size=%-10llu align=%llu\n", on[o],
                (unsigned long long)lens[i], (unsigned long long)sa.size,
                (unsigned long long)sa.align);
      }

   struct {
      MTLPixelFormat f;
      int w, h, mips, samples;
      MTLTextureUsage u;
      const char *n;
   } td[] = {
      {MTLPixelFormatRGBA8Unorm, 1, 1, 1, 1, MTLTextureUsageShaderRead, "rgba8 1x1"},
      {MTLPixelFormatRGBA8Unorm, 256, 256, 1, 1, MTLTextureUsageShaderRead, "rgba8 256"},
      {MTLPixelFormatRGBA8Unorm, 256, 256, 9, 1, MTLTextureUsageShaderRead, "rgba8 256 mips"},
      {MTLPixelFormatRGBA8Unorm, 1920, 1080, 1, 1,
       MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead, "rgba8 1080p rt"},
      {MTLPixelFormatRGBA8Unorm, 1921, 1081, 1, 1,
       MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead, "rgba8 1921x1081 rt"},
      {MTLPixelFormatRGBA8Unorm, 1920, 1080, 1, 4, MTLTextureUsageRenderTarget, "rgba8 1080p 4x"},
      {MTLPixelFormatDepth32Float, 1920, 1080, 1, 1, MTLTextureUsageRenderTarget, "d32 1080p"},
      {MTLPixelFormatRGBA16Float, 1024, 1024, 11, 1, MTLTextureUsageShaderRead, "rgba16f 1k mips"},
      {MTLPixelFormatBC1_RGBA, 1024, 1024, 11, 1, MTLTextureUsageShaderRead, "bc1 1k mips"},
   };
   for (int o = 0; o < 2; o++)
      for (unsigned i = 0; i < sizeof(td) / sizeof(td[0]); i++) {
         MTLTextureDescriptor *d = [MTLTextureDescriptor new];
         d.pixelFormat = td[i].f;
         d.width = td[i].w;
         d.height = td[i].h;
         d.mipmapLevelCount = td[i].mips;
         d.sampleCount = td[i].samples;
         d.textureType = td[i].samples > 1 ? MTLTextureType2DMultisample : MTLTextureType2D;
         d.usage = td[i].u;
         d.resourceOptions = opts[o];
         if (o == 1 || td[i].samples > 1 || td[i].f == MTLPixelFormatDepth32Float)
            d.storageMode = MTLStorageModePrivate;
         MTLSizeAndAlign sa = [dev heapTextureSizeAndAlignWithDescriptor:d];
         MTLSizeAndAlign sb = [dev heapTextureSizeAndAlignWithDescriptor:d];
         printf("texture %-7s %-20s size=%-10llu align=%-6llu repeatable=%d linear-bytes=%llu\n",
                on[o], td[i].n, (unsigned long long)sa.size, (unsigned long long)sa.align,
                sa.size == sb.size && sa.align == sb.align,
                (unsigned long long)td[i].w * td[i].h * td[i].samples *
                   (td[i].f == MTLPixelFormatRGBA16Float ? 8 : td[i].f == MTLPixelFormatBC1_RGBA ? 0 : 4));
      }
}

/* --------------------------------------------------------------------- ids */

static void sec_ids(void)
{
   printf("== ids\n");
   id<MTLHeap> h = new_heap(64 * MiB, KK_OPTS);
   MTLTextureDescriptor *d = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:
                                                      MTLPixelFormatRGBA8Unorm
                                                                            width:64
                                                                           height:64
                                                                        mipmapped:NO];
   d.resourceOptions = KK_OPTS;
   d.usage = MTLTextureUsageShaderRead | MTLTextureUsagePixelFormatView;
   MTLSizeAndAlign sa = [dev heapTextureSizeAndAlignWithDescriptor:d];
   NSMutableArray *tex = [NSMutableArray array];
   printf("16 textures:");
   for (int i = 0; i < 16; i++) {
      id<MTLTexture> t = [h newTextureWithDescriptor:d offset:i * sa.size];
      [tex addObject:t];
      printf(" %llu", (unsigned long long)t.gpuResourceID._impl);
   }
   printf("\n8 views of texture 0:");
   NSMutableArray *views = [NSMutableArray array];
   for (int i = 0; i < 8; i++) {
      id<MTLTexture> v = [tex[0] newTextureViewWithPixelFormat:MTLPixelFormatRGBA8Unorm_sRGB];
      [views addObject:v];
      printf(" %llu", (unsigned long long)v.gpuResourceID._impl);
   }
   printf("\nrelease textures 2,5,9 and all views, then 6 new textures:");
   tex[2] = [NSNull null];
   tex[5] = [NSNull null];
   tex[9] = [NSNull null];
   [views removeAllObjects];
   for (int i = 0; i < 6; i++) {
      id<MTLTexture> t = [h newTextureWithDescriptor:d offset:(16 + i) * sa.size];
      [tex addObject:t];
      printf(" %llu", (unsigned long long)t.gpuResourceID._impl);
   }
   printf("\nsame offset re-placed (alias of texture 0):");
   id<MTLTexture> again = [h newTextureWithDescriptor:d offset:0];
   printf(" %llu (texture 0 is %llu)\n", (unsigned long long)again.gpuResourceID._impl,
          (unsigned long long)((id<MTLTexture>)tex[0]).gpuResourceID._impl);
   printf("8 samplers:");
   NSMutableArray *samp = [NSMutableArray array];
   for (int i = 0; i < 8; i++) {
      MTLSamplerDescriptor *sd = [MTLSamplerDescriptor new];
      sd.supportArgumentBuffers = YES;
      sd.lodMaxClamp = 1.0f + i;
      id<MTLSamplerState> s = [dev newSamplerStateWithDescriptor:sd];
      [samp addObject:s];
      printf(" %llu", (unsigned long long)s.gpuResourceID._impl);
   }
   MTLSamplerDescriptor *sd = [MTLSamplerDescriptor new];
   sd.supportArgumentBuffers = YES;
   sd.lodMaxClamp = 1.0f;
   id<MTLSamplerState> dup = [dev newSamplerStateWithDescriptor:sd];
   printf("\nsampler with the same descriptor as sampler 0: %llu\n",
          (unsigned long long)dup.gpuResourceID._impl);

   /* cost of the query itself, host-side, for scale */
   double t0 = now_ms();
   uint64_t acc = 0;
   for (int i = 0; i < 100000; i++)
      acc += ((id<MTLTexture>)tex[0]).gpuResourceID._impl;
   double t1 = now_ms();
   printf("gpuResourceID host cost: %.1f ns/call (acc %llu)\n", (t1 - t0) * 1e6 / 100000,
          (unsigned long long)(acc & 1));
   t0 = now_ms();
   for (int i = 0; i < 1000; i++) {
      id<MTLTexture> v = [tex[0] newTextureViewWithPixelFormat:MTLPixelFormatRGBA8Unorm_sRGB];
      acc += v.gpuResourceID._impl;
   }
   t1 = now_ms();
   printf("texture view create + id: %.2f us each\n", (t1 - t0) * 1e3 / 1000);
}

/* ------------------------------------------------------------------ layout */

static void sec_layout(void)
{
   printf("== layout\n");
   const int W = 256, H = 256;
   id<MTLHeap> h = new_heap(16 * MiB, KK_OPTS);
   id<MTLBuffer> alias = [h newBufferWithLength:16 * MiB options:KK_OPTS offset:0];
   for (int opt = 1; opt >= 0; opt--) {
      MTLTextureDescriptor *d =
         [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatR32Uint
                                                            width:W
                                                           height:H
                                                        mipmapped:NO];
      d.resourceOptions = KK_OPTS;
      d.allowGPUOptimizedContents = opt;
      d.usage = MTLTextureUsageShaderRead;
      memset(alias.contents, 0, 16 * MiB);
      id<MTLTexture> t = [h newTextureWithDescriptor:d offset:0];
      uint32_t *px = malloc(W * H * 4);
      for (int i = 0; i < W * H; i++)
         px[i] = (uint32_t)i;
      [t replaceRegion:MTLRegionMake2D(0, 0, W, H) mipmapLevel:0 withBytes:px bytesPerRow:W * 4];
      uint32_t *raw = alias.contents;
      int linear = 0, found = 0;
      for (int i = 0; i < W * H; i++) {
         linear += raw[i] == (uint32_t)i;
         found += raw[i] < (uint32_t)(W * H);
      }
      printf("allowGPUOptimizedContents=%d: words at linear position %d/%d; first 16 raw:", opt,
             linear, W * H);
      for (int i = 0; i < 16; i++)
         printf(" %u", raw[i]);
      printf(" | row 1 starts with %u\n", raw[W]);
      (void)found;
      free(px);
   }
}

/* -------------------------------------------------------------------- tput */

static NSString *const TPUT_MSL =
   @"#include <metal_stdlib>\n"
    "using namespace metal;\n"
    "kernel void cp(texture2d<float, access::read> a [[texture(0)]],\n"
    "               texture2d<float, access::write> b [[texture(1)]],\n"
    "               uint2 g [[thread_position_in_grid]]) {\n"
    "   b.write(a.read(g) * 0.5f + 0.25f, g);\n"
    "}\n";

static double tput_run(MTLResourceOptions opts, MTLStorageMode sm, int iters, double *gbps)
{
   const int N = 4096;
   MTLTextureDescriptor *d =
      [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm
                                                         width:N
                                                        height:N
                                                     mipmapped:NO];
   d.resourceOptions = opts;
   d.storageMode = sm;
   d.usage = MTLTextureUsageShaderRead | MTLTextureUsageShaderWrite | MTLTextureUsageRenderTarget;
   MTLSizeAndAlign sa = [dev heapTextureSizeAndAlignWithDescriptor:d];
   uint64_t sz = (sa.size + sa.align - 1) / sa.align * sa.align;
   id<MTLHeap> h = new_heap(2 * sz, opts);
   id<MTLTexture> a = [h newTextureWithDescriptor:d offset:0];
   id<MTLTexture> b = [h newTextureWithDescriptor:d offset:sz];
   NSError *err = nil;
   id<MTLLibrary> lib = [dev newLibraryWithSource:TPUT_MSL options:nil error:&err];
   id<MTLComputePipelineState> ps =
      [dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"cp"] error:&err];
   double best = 1e9;
   for (int rep = 0; rep < 5; rep++) {
      id<MTLCommandBuffer> cb = [queue commandBuffer];
      id<MTLComputeCommandEncoder> e = [cb computeCommandEncoder];
      [e setComputePipelineState:ps];
      [e useHeap:h];
      for (int i = 0; i < iters; i++) {
         [e setTexture:(i & 1) ? b : a atIndex:0];
         [e setTexture:(i & 1) ? a : b atIndex:1];
         [e dispatchThreads:MTLSizeMake(N, N, 1) threadsPerThreadgroup:MTLSizeMake(16, 16, 1)];
         [e memoryBarrierWithScope:MTLBarrierScopeTextures];
      }
      [e endEncoding];
      [cb commit];
      [cb waitUntilCompleted];
      double ms = (cb.GPUEndTime - cb.GPUStartTime) * 1e3;
      if (ms < best)
         best = ms;
   }
   *gbps = (double)N * N * 4 * 2 * iters / (best / 1e3) / 1e9;
   return best;
}

static void sec_tput(void)
{
   printf("== tput (indicative: peers share the GPU)\n");
   for (int round = 0; round < 2; round++) {
      double g1, g2;
      double s = tput_run(KK_OPTS, MTLStorageModeShared, 50, &g1);
      double p = tput_run(PRIV_OPTS, MTLStorageModePrivate, 50, &g2);
      printf("round %d: 50x 4096^2 rgba8 read+write: shared heap %.2f ms (%.1f GB/s), private "
             "heap %.2f ms (%.1f GB/s)\n",
             round, s, g1, p, g2);
   }
}

/* ---------------------------------------------------------------------- hv */

static void sec_hv(void)
{
   printf("== hv\n");
   hv_return_t r = hv_vm_create(NULL);
   if (r != HV_SUCCESS) {
      printf("hv_vm_create: 0x%x (unsigned or sandboxed?)\n", r);
      return;
   }
   uint64_t gpa = 0x100000000ULL;
   for (uint64_t sz = 1 * GiB; sz <= 8 * GiB; sz *= 2) {
      id<MTLHeap> h = new_heap(sz, KK_OPTS);
      id<MTLBuffer> alias = h ? [h newBufferWithLength:sz options:KK_OPTS offset:0] : nil;
      if (!alias) {
         printf("heap %llu GiB: not created\n", (unsigned long long)(sz / GiB));
         break;
      }
      double t0 = now_ms();
      r = hv_vm_map(alias.contents, gpa, sz, HV_MEMORY_READ | HV_MEMORY_WRITE);
      double t1 = now_ms();
      printf("heap %llu GiB alias at %p -> gpa 0x%llx: hv_vm_map 0x%x (%.2f ms)", (unsigned long long)(sz / GiB),
             alias.contents, (unsigned long long)gpa, r, t1 - t0);
      if (r == HV_SUCCESS) {
         t0 = now_ms();
         r = hv_vm_unmap(gpa, sz);
         printf(", unmap 0x%x (%.2f ms)", r, now_ms() - t0);
      }
      printf("\n");
      gpa += sz;
   }
   hv_vm_destroy();
}

int main(int argc, char **argv)
{
   @autoreleasepool {
      dev = MTLCreateSystemDefaultDevice();
      queue = [dev newCommandQueue];
      struct {
         const char *n;
         void (*f)(void);
         bool dflt;
      } secs[] = {{"limits", sec_limits, true}, {"addr", sec_addr, true},
                  {"align", sec_align, true},   {"ids", sec_ids, true},
                  {"layout", sec_layout, true}, {"tput", sec_tput, true},
                  {"hv", sec_hv, false}};
      for (unsigned i = 0; i < sizeof(secs) / sizeof(secs[0]); i++) {
         bool run = argc == 1 && secs[i].dflt;
         for (int a = 1; a < argc; a++)
            run |= !strcmp(argv[a], secs[i].n);
         if (run) {
            @autoreleasepool {
               secs[i].f();
            }
            fflush(stdout);
         }
      }
   }
   return 0;
}
