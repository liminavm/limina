// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// EFFICACY PROBE: does RELEASING an MTL4CommandAllocator return its heap memory?
//
// WHY. The shipped allocator pool (kk_device.c) bounds memory but never returns it: nothing is
// destroyed before vkDestroyDevice. On the venus tier that is fine — the guest app's
// vkDestroyDevice reaches KK and the pool dies with it. On the vrend tier the KK VkDevice belongs
// to zink's *screen*, created at EGL init and destroyed only by eglTerminate at worker exit, so
// no gate exists between workloads and ~1 GiB of bounded pool is held for the process lifetime.
//
// The proposed fix is to destroy a draining+idle allocator in kk_alloc_pool_acquire instead of
// resetting it, above a floor. That whole idea rests on ONE unverified assumption:
//
//     releasing an MTL4CommandAllocator actually gives the memory back.
//
// If MTL4CommandBuffer (or the queue, or the device) retains the allocator, or if AGX recycles
// the heaps into a device-level cache, then release is safe but INEFFECTIVE and we should not
// write the policy at all. An afternoon here beats a refactor.
//
// ── Four ways this probe could lie, and what is done about each ────────────────────────────────
//
// 1. WRONG HEAP CLASS. Compute self-regulates and plateaus (~7 MiB); RENDER is what ratchets
//    (the 753 MiB allocator, the 11 GiB) and T3 proved the two heap kinds do not interchange.
//    Growing with compute because the plumbing is easy would answer a question nobody asked.
//    => CLASS=render is the DEFAULT. CLASS=compute is available only as a contrast.
//
// 2. RELEASED != DEALLOCATED. If the queue or Metal's commit machinery holds an internal
//    reference, "memory did not come back" is a fact about retention, not about what dealloc
//    does. => every allocator and command buffer carries an associated-object SENTINEL whose
//    dealloc is counted. Memory numbers are only interpretable once the sentinels confirm the
//    objects actually died; the probe says so explicitly when they have not.
//
// 3. DEFERRED CLEANUP. AGX defers unmap work, often until the next kernel roundtrip rather than
//    on a timer, so one fixed sleep can manufacture a false "not returned". => every teardown
//    step samples a CURVE: immediately, +1s, +5s, and again after committing and draining a
//    dummy command buffer (the roundtrip that flushes deferred deletion).
//
// 4. CACHE REUSE MASQUERADING AS EITHER VERDICT. If release returns heaps to a per-device cache,
//    vmmap can stay flat while the memory is perfectly reusable — in which case destroy buys
//    nothing over reset even though it looks like a failure. => MODE=d regrows after releasing
//    and reads the SHAPE across cycles (flat = cache reuse, doubling = real growth).
//
// Also: three lenses, never one. vmmap regions are address space; phys_footprint is what the
// user feels; AGXResource is the kernel-object counter that tracked the original leak 1:1. And
// the decision compares vmmap-delta(grow) against vmmap-delta(release) — the instrument against
// itself — with allocatedSize demoted to a cross-check, because per-allocator sizes can
// double-count if allocators suballocate from shared chunks.
//
// ── Modes (each a SEPARATE process, so no phase contaminates another) ──────────────────────────
//   MODE=a    staged teardown: release cbs -> allocators -> queue -> device, sampling at each.
//             Attribution, not yes/no: if the QUEUE is the retainer, destroy is dead on vrend
//             specifically (the queue is process-lifetime there too) — same reading, different
//             lesson.
//   MODE=b    the real KK ordering: release allocators while their COMPLETED cbs are still
//             retained (KK releases cbs in kk_cmd_release_resources, at Vulkan cb reset, which
//             can be much later than the allocator going idle).
//   MODE=c    control: reset() only, never release. Expect no return. Proves the instrument can
//             tell the two apart.
//   MODE=d    regrow cycles: grow -> release -> regrow, CYCLES times. Shape over verdict.
//   MODE=big  one allocator grown to hundreds of MiB, then released. The policy's real payoff is
//             clawing back a post-heavy-epoch monster, and wholesale teardown of a many-region
//             allocator may take a different driver path than small ones.
//
// Env: MODE (a|b|c|d|big), CLASS (render|compute), ALLOCS, PASSES, DRAWS, CYCLES, BIGPASSES.
//
// ── Decision thresholds, fixed BEFORE the first run ───────────────────────────────────────────
// Stated here so a middling result cannot be rationalised into whichever answer is convenient:
//   >= 80% of the grown vmmap delta returns  => build the destroy policy.
//   <= 20% returns                           => the idea is dead; say so and stop.
//   in between                               => attribute before deciding; no verdict from this
//                                               probe alone.

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <objc/runtime.h>
#import <mach/mach.h>
#import <stdatomic.h>
#import <unistd.h>

static long envl(const char *k, long d)
{
   const char *v = getenv(k);
   return (v && *v) ? atol(v) : d;
}

static const char *envs(const char *k, const char *d)
{
   const char *v = getenv(k);
   return (v && *v) ? v : d;
}

static double mib(uint64_t b) { return b / 1048576.0; }

/* ── dealloc sentinels ────────────────────────────────────────────────────────────────────────
 * An associated object is released when its host deallocs, so counting sentinel deallocs counts
 * host deallocs — without which "memory did not return" is uninterpretable (see lie #2). */
static atomic_int g_dead_allocs = 0;
static atomic_int g_dead_cbs = 0;

@interface LSentinel : NSObject
@property(nonatomic, assign) atomic_int *counter;
@end

@implementation LSentinel
- (void)dealloc
{
   if (_counter)
      atomic_fetch_add(_counter, 1);
   [super dealloc];
}
@end

static char g_sentinel_key;

static void
attach_sentinel(id obj, atomic_int *counter)
{
   LSentinel *s = [LSentinel new];
   s.counter = counter;
   objc_setAssociatedObject(obj, &g_sentinel_key, s, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
   [s release]; /* the association is now the only owner */
}

/* ── measurement ──────────────────────────────────────────────────────────────────────────────
 * Three lenses. vmmap regions/bytes are address space under the IOAccelerator (graphics) tag;
 * phys_footprint is what the user actually feels; AGXResource is the SYSTEM-WIDE kernel-object
 * counter that tracked the in-VM leak 1:1 — read its delta, never its value, and only trust
 * deltas well above the +/-33 host noise floor measured in ioclass-cycle.sh. */
struct snap {
   long regions;
   double size_mib;      /* IOAccelerator (graphics) */
   double footprint_mib; /* phys_footprint, from the kernel not from vmmap parsing */
   long agxres;
};

static double
parse_size_token(const char *tok)
{
   /* vmmap prints e.g. "1.2G", "512K", "17.4M". */
   char *end = NULL;
   double v = strtod(tok, &end);
   if (!end)
      return 0.0;
   switch (*end) {
   case 'K': return v / 1024.0;
   case 'M': return v;
   case 'G': return v * 1024.0;
   case 'T': return v * 1024.0 * 1024.0;
   default:  return v / 1048576.0; /* bare bytes */
   }
}

static struct snap
take_snap(void)
{
   struct snap s = {0, 0.0, 0.0, 0};

   char cmd[512];
   snprintf(cmd, sizeof cmd,
            "vmmap --summary %d 2>/dev/null | awk '/^IOAccelerator \\(graphics\\)/{print $NF\" \"$3; exit}'",
            getpid());
   FILE *f = popen(cmd, "r");
   if (f) {
      char regbuf[64] = {0}, szbuf[64] = {0};
      if (fscanf(f, "%63s %63s", regbuf, szbuf) == 2) {
         s.regions = atol(regbuf);
         s.size_mib = parse_size_token(szbuf);
      }
      pclose(f);
   }

   f = popen("ioclasscount 2>/dev/null | awk '/^AGXResource = /{print $3}'", "r");
   if (f) {
      char buf[64] = {0};
      if (fscanf(f, "%63s", buf) == 1)
         s.agxres = atol(buf);
      pclose(f);
   }

   task_vm_info_data_t vmi;
   mach_msg_type_number_t cnt = TASK_VM_INFO_COUNT;
   if (task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&vmi, &cnt) == KERN_SUCCESS)
      s.footprint_mib = mib(vmi.phys_footprint);

   return s;
}

static void
print_snap(const char *tag, struct snap s, struct snap base)
{
   printf("  %-22s regions=%-7ld (%+7ld)  ioaccel=%8.1f MiB (%+8.1f)  "
          "footprint=%8.1f MiB (%+8.1f)  AGXResource=%-8ld (%+ld)\n",
          tag, s.regions, s.regions - base.regions, s.size_mib, s.size_mib - base.size_mib,
          s.footprint_mib, s.footprint_mib - base.footprint_mib, s.agxres,
          s.agxres - base.agxres);
   fflush(stdout);
}

/* ── Metal scaffolding ────────────────────────────────────────────────────────────────────── */
static id<MTLDevice> g_dev;
static id<MTL4CommandQueue> g_queue;
static id<MTL4ArgumentTable> g_argtable;
static id<MTLComputePipelineState> g_cpso;
static id<MTLRenderPipelineState> g_rpso;
static id<MTLTexture> g_rtex;
static atomic_int g_completions = 0;

static void
commit_one(id<MTL4CommandBuffer> cb)
{
   MTL4CommitOptions *o = [MTL4CommitOptions new];
   /* NOTE: this block must capture NOTHING that could retain the cb or allocator, or the
    * sentinel census in lie #2 would be measuring the handler's own reference. */
   [o addFeedbackHandler:^(id<MTL4CommitFeedback> fb) {
     if (fb.error)
        fprintf(stderr, "  !! commit error: %s\n", fb.error.description.UTF8String);
     atomic_fetch_add(&g_completions, 1);
   }];
   id<MTL4CommandBuffer> __unsafe_unretained buf[1] = {cb};
   [g_queue commit:buf count:1 options:o];
   [o release];
}

static void
drain_to(int n)
{
   while (atomic_load(&g_completions) < n)
      usleep(2000);
}

/* Encode one pass on `a` in a FRESH command buffer — this mirrors KK, where cs_start_render mints
 * a new Metal command buffer per render pass but begins them all on the same allocator. Returns
 * the cb RETAINED; the caller owns it, which is what makes the T5a/T5b ordering testable. */
static id<MTL4CommandBuffer>
encode_pass(id<MTL4CommandAllocator> a, int render, long draws)
{
   /* `new` already returns +1 — the caller owns the cb. The first data run (2026-08-09 morning)
    * had an extra [.. retain] here, which leaked one reference per cb and manufactured the
    * "committed cbs are never deallocated" reading: 0/N sentinel deallocs in every mode was the
    * probe's own leak, not Metal retention. The flush_roundtrip cbs proved it internally — they
    * go through the full release path yet never dealloc'd either. */
   id<MTL4CommandBuffer> cb = [g_dev newCommandBuffer];
   attach_sentinel(cb, &g_dead_cbs);
   [cb beginCommandBufferWithAllocator:a];

   if (render) {
      MTL4RenderPassDescriptor *rp = [MTL4RenderPassDescriptor new];
      rp.colorAttachments[0].texture = g_rtex;
      rp.colorAttachments[0].loadAction = MTLLoadActionClear;
      rp.colorAttachments[0].storeAction = MTLStoreActionStore;
      rp.renderTargetWidth = 256;
      rp.renderTargetHeight = 256;
      id<MTL4RenderCommandEncoder> enc = [cb renderCommandEncoderWithDescriptor:rp];
      [enc setRenderPipelineState:g_rpso];
      for (long d = 0; d < draws; d++)
         [enc drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
      [enc endEncoding];
      [rp release];
   } else {
      id<MTL4ComputeCommandEncoder> enc = [cb computeCommandEncoder];
      [enc setArgumentTable:g_argtable];
      [enc setComputePipelineState:g_cpso];
      for (long d = 0; d < draws; d++)
         [enc dispatchThreads:MTLSizeMake(64, 1, 1) threadsPerThreadgroup:MTLSizeMake(64, 1, 1)];
      [enc endEncoding];
   }

   [cb endCommandBuffer];
   return cb;
}

/* Commit and drain one trivial cb on a fresh allocator. This is the kernel ROUNDTRIP that flushes
 * AGX's deferred destruction work — without it a "nothing came back" reading may simply mean
 * cleanup had not been processed yet (lie #3). */
static void
flush_roundtrip(void)
{
   @autoreleasepool {
      id<MTL4CommandAllocator> a = [g_dev newCommandAllocator];
      int before = atomic_load(&g_completions);
      id<MTL4CommandBuffer> cb = encode_pass(a, 0, 1);
      commit_one(cb);
      drain_to(before + 1);
      [cb release];
      [a release];
   }
}

/* Sample the settle CURVE after a teardown step, so no single sleep can manufacture a verdict. */
static struct snap
settle_curve(const char *what, struct snap base)
{
   struct snap s;
   char tag[64];

   snprintf(tag, sizeof tag, "%s t+0", what);
   s = take_snap();
   print_snap(tag, s, base);

   sleep(1);
   snprintf(tag, sizeof tag, "%s t+1s", what);
   s = take_snap();
   print_snap(tag, s, base);

   sleep(4);
   snprintf(tag, sizeof tag, "%s t+5s", what);
   s = take_snap();
   print_snap(tag, s, base);

   flush_roundtrip();
   snprintf(tag, sizeof tag, "%s +roundtrip", what);
   s = take_snap();
   print_snap(tag, s, base);

   return s;
}

static void
report_sentinels(const char *where, long expect_allocs, long expect_cbs)
{
   printf("  [sentinels] %-18s allocators dealloc'd=%d/%ld   cbs dealloc'd=%d/%ld%s\n", where,
          atomic_load(&g_dead_allocs), expect_allocs, atomic_load(&g_dead_cbs), expect_cbs,
          (atomic_load(&g_dead_allocs) < expect_allocs) ? "   <-- NOT ALL DEAD" : "");
   fflush(stdout);
}

static int
setup_metal(void)
{
   NSError *err = nil;

   g_dev = MTLCreateSystemDefaultDevice();
   if (!g_dev) { fprintf(stderr, "no Metal device\n"); return 0; }
   if (![g_dev respondsToSelector:@selector(newCommandAllocator)]) {
      fprintf(stderr, "no MTL4 on this OS\n");
      return 0;
   }
   g_queue = [g_dev newMTL4CommandQueue];
   if (!g_queue) { fprintf(stderr, "no MTL4 queue\n"); return 0; }

   MTL4CompilerDescriptor *cd = [[MTL4CompilerDescriptor new] autorelease];
   id<MTL4Compiler> compiler = [g_dev newCompilerWithDescriptor:cd error:&err];
   if (!compiler) { fprintf(stderr, "no compiler: %s\n", err.description.UTF8String); return 0; }

   MTLCompileOptions *co = [[MTLCompileOptions new] autorelease];
   co.languageVersion = MTLLanguageVersion3_2;

   MTL4LibraryDescriptor *ld = [[MTL4LibraryDescriptor new] autorelease];
   ld.source = @"#include <metal_stdlib>\n"
                "using namespace metal;\n"
                "kernel void nop(uint tid [[thread_position_in_grid]]) { }\n"
                "vertex float4 vs(uint i [[vertex_id]]) { return float4(0,0,0,1); }\n"
                "fragment float4 fs() { return float4(1,0,0,1); }\n";
   ld.options = co;
   id<MTLLibrary> lib = [compiler newLibraryWithDescriptor:ld error:&err];
   if (!lib) { fprintf(stderr, "no library: %s\n", err.description.UTF8String); return 0; }

   MTL4LibraryFunctionDescriptor *nfd = [[MTL4LibraryFunctionDescriptor new] autorelease];
   nfd.name = @"nop"; nfd.library = lib;
   MTL4ComputePipelineDescriptor *pd = [[MTL4ComputePipelineDescriptor new] autorelease];
   pd.computeFunctionDescriptor = nfd;
   g_cpso = [compiler newComputePipelineStateWithDescriptor:pd compilerTaskOptions:nil error:&err];
   if (!g_cpso) { fprintf(stderr, "no compute pipeline\n"); return 0; }

   MTL4ArgumentTableDescriptor *atd = [[MTL4ArgumentTableDescriptor new] autorelease];
   atd.maxBufferBindCount = 1;
   g_argtable = [g_dev newArgumentTableWithDescriptor:atd error:&err];
   if (!g_argtable) { fprintf(stderr, "no argument table\n"); return 0; }

   MTLTextureDescriptor *td =
      [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                                                         width:256
                                                        height:256
                                                     mipmapped:NO];
   td.usage = MTLTextureUsageRenderTarget;
   td.storageMode = MTLStorageModePrivate;
   g_rtex = [g_dev newTextureWithDescriptor:td];

   MTL4LibraryFunctionDescriptor *vfd = [[MTL4LibraryFunctionDescriptor new] autorelease];
   vfd.name = @"vs"; vfd.library = lib;
   MTL4LibraryFunctionDescriptor *ffd = [[MTL4LibraryFunctionDescriptor new] autorelease];
   ffd.name = @"fs"; ffd.library = lib;
   MTL4RenderPipelineDescriptor *rpd = [[MTL4RenderPipelineDescriptor new] autorelease];
   rpd.vertexFunctionDescriptor = vfd;
   rpd.fragmentFunctionDescriptor = ffd;
   rpd.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
   g_rpso = [compiler newRenderPipelineStateWithDescriptor:rpd compilerTaskOptions:nil error:&err];
   if (!g_rpso) { fprintf(stderr, "no render pipeline: %s\n", err.description.UTF8String); return 0; }

   return 1;
}

/* Grow `n` allocators to roughly the pool budget by encoding `passes` passes on each, every pass
 * in its own command buffer (KK's shape). Returns the summed allocatedSize; the allocators land
 * in `allocs` and the retained command buffers in `cbs`. */
static uint64_t
grow(id<MTL4CommandAllocator> *allocs, NSMutableArray *cbs, long n, long passes, int render,
     long draws)
{
   int before = atomic_load(&g_completions);
   for (long i = 0; i < n; i++) {
      id<MTL4CommandAllocator> a = [g_dev newCommandAllocator];
      attach_sentinel(a, &g_dead_allocs);
      allocs[i] = a;
      for (long p = 0; p < passes; p++) {
         id<MTL4CommandBuffer> cb = encode_pass(a, render, draws);
         [cbs addObject:cb];
         commit_one(cb);
         [cb release]; /* the array holds it now */
      }
   }
   drain_to(before + (int)(n * passes));

   uint64_t sum = 0;
   for (long i = 0; i < n; i++)
      sum += [allocs[i] allocatedSize];
   return sum;
}

int
main(void)
{
   const char *mode = envs("MODE", "a");
   const char *cls = envs("CLASS", "render");
   const int render = strcmp(cls, "compute") != 0;
   const long nallocs = envl("ALLOCS", 32);
   const long passes = envl("PASSES", 3);
   const long draws = envl("DRAWS", 200);
   const long cycles = envl("CYCLES", 4);
   const long bigpasses = envl("BIGPASSES", 150);

   @autoreleasepool {
      if (!setup_metal())
         return 1;

      printf("== destroy-probe MODE=%s CLASS=%s allocs=%ld passes=%ld draws=%ld\n", mode, cls,
             nallocs, passes, draws);
      printf("   (does releasing an MTL4CommandAllocator return its heaps?)\n\n");

      /* Baseline AFTER all pipelines/textures/queue exist, so the grow delta is attributable to
       * allocator heaps and not to one-time compiler and pipeline allocations. */
      flush_roundtrip();
      struct snap base = take_snap();
      print_snap("baseline", base, base);

      /* ── MODE=big: one monster, the case the policy actually exists to claw back ──────────── */
      if (strcmp(mode, "big") == 0) {
         id<MTL4CommandAllocator> one[1];
         NSMutableArray *cbs = [NSMutableArray array];
         uint64_t sum;
         @autoreleasepool {
            sum = grow(one, cbs, 1, bigpasses, render, draws);
         }
         printf("\n  one allocator grown to %.1f MiB over %ld passes\n", mib(sum), bigpasses);
         struct snap grown = take_snap();
         print_snap("grown", grown, base);

         @autoreleasepool { [cbs removeAllObjects]; }
         [one[0] release];
         struct snap after = settle_curve("released", base);
         report_sentinels("after release", 1, bigpasses);

         double gained = grown.size_mib - base.size_mib;
         double back = grown.size_mib - after.size_mib;
         printf("\n  grew %+.1f MiB, returned %.1f MiB (%.0f%%)\n", gained, back,
                gained > 0 ? 100.0 * back / gained : 0.0);
         return 0;
      }

      /* ── MODE=d: regrow cycles. Shape over verdict — flat across cycles means the heaps are
       * being recycled from a device cache, in which case destroy buys nothing over reset even
       * if a single before/after pair looked like a win. ───────────────────────────────────── */
      if (strcmp(mode, "d") == 0) {
         for (long c = 0; c < cycles; c++) {
            id<MTL4CommandAllocator> *allocs = calloc(nallocs, sizeof(*allocs));
            NSMutableArray *cbs = [NSMutableArray array];
            uint64_t sum;
            @autoreleasepool {
               sum = grow(allocs, cbs, nallocs, passes, render, draws);
            }
            struct snap grown = take_snap();
            char tag[64];
            snprintf(tag, sizeof tag, "cycle %ld grown", c);
            print_snap(tag, grown, base);
            printf("    (allocatedSize sum %.1f MiB)\n", mib(sum));

            @autoreleasepool { [cbs removeAllObjects]; }
            for (long i = 0; i < nallocs; i++)
               [allocs[i] release];
            free(allocs);
            flush_roundtrip();
            sleep(1);
            snprintf(tag, sizeof tag, "cycle %ld released", c);
            print_snap(tag, take_snap(), base);
         }
         report_sentinels("end", nallocs * cycles, nallocs * passes * cycles);
         printf("\n  Read the SHAPE: 'released' returning to baseline each cycle => release works.\n");
         printf("  'grown' flat across cycles with no return => heaps recycled from a cache, and\n");
         printf("  destroy buys nothing over reset.\n");
         return 0;
      }

      /* ── MODE=a / b / c ──────────────────────────────────────────────────────────────────── */
      id<MTL4CommandAllocator> *allocs = calloc(nallocs, sizeof(*allocs));
      NSMutableArray *cbs = [NSMutableArray array];
      uint64_t sum;
      @autoreleasepool {
         sum = grow(allocs, cbs, nallocs, passes, render, draws);
      }
      printf("\n  %ld allocators, allocatedSize sum %.1f MiB (mean %.2f MiB each)\n", nallocs,
             mib(sum), mib(sum) / (double)nallocs);
      struct snap grown = take_snap();
      print_snap("grown", grown, base);
      double gained = grown.size_mib - base.size_mib;

      struct snap after = grown;

      if (strcmp(mode, "c") == 0) {
         /* Control: reset only. allocatedSize is known never to shrink; the question here is
          * only whether the instrument can distinguish reset from release at all. */
         for (long i = 0; i < nallocs; i++)
            [allocs[i] reset];
         after = settle_curve("reset-only", base);
         uint64_t after_sum = 0;
         for (long i = 0; i < nallocs; i++)
            after_sum += [allocs[i] allocatedSize];
         printf("  allocatedSize sum after reset: %.1f MiB (was %.1f)\n", mib(after_sum), mib(sum));
         report_sentinels("after reset", 0, nallocs * passes);
      } else if (strcmp(mode, "b") == 0) {
         /* The real KK ordering: the allocator goes idle and is released while its COMPLETED
          * command buffers are still alive, because KK releases those in
          * kk_cmd_release_resources at Vulkan command-buffer reset — potentially much later. */
         for (long i = 0; i < nallocs; i++)
            [allocs[i] release];
         after = settle_curve("allocs released", base);
         report_sentinels("allocs only", nallocs, 0);

         @autoreleasepool { [cbs removeAllObjects]; }
         after = settle_curve("then cbs", base);
         report_sentinels("cbs too", nallocs, nallocs * passes);
      } else {
         /* Staged teardown. Attribution, not yes/no: WHICH layer was holding it. */
         @autoreleasepool { [cbs removeAllObjects]; }
         after = settle_curve("cbs released", base);
         report_sentinels("cbs only", 0, nallocs * passes);

         for (long i = 0; i < nallocs; i++)
            [allocs[i] release];
         after = settle_curve("allocs released", base);
         report_sentinels("allocs too", nallocs, nallocs * passes);

         [g_queue release];
         g_queue = nil;
         /* No roundtrip possible without a queue, so sample directly. */
         sleep(2);
         after = take_snap();
         print_snap("queue released", after, base);
         /* The first run drew "the queue retains the cbs" from a footprint delta alone; the
          * sentinel count after queue release is the actual test of that claim. */
         report_sentinels("queue too", nallocs, nallocs * passes);
      }

      double back = grown.size_mib - after.size_mib;
      double pct = gained > 0 ? 100.0 * back / gained : 0.0;
      /* Mode c never releases anything, so the release thresholds do not apply to it: there it
       * is an INSTRUMENT CHECK, and 0% is the pass. Saying "the policy is DEAD" under mode c
       * would be a labelling artifact, and this record has to survive being re-read later. */
      const int is_control = (strcmp(mode, "c") == 0);
      printf("\n%s (mode %s, class %s)\n", is_control ? "INSTRUMENT CHECK" : "VERDICT", mode, cls);
      printf("  grew  %+8.1f MiB of IOAccelerator (%+ld regions)\n", gained,
             grown.regions - base.regions);
      printf("  back  %+8.1f MiB (%.0f%% of the growth), %+ld regions\n", back, pct,
             after.regions - grown.regions);
      printf("  allocatedSize sum was %.1f MiB (cross-check only — per-allocator sizes can\n"
             "  double-count if allocators suballocate from shared chunks)\n", mib(sum));
      if (is_control) {
         printf("  control arm: reset() only, nothing released. 0%% return is the PASS — it shows\n");
         printf("  the instrument can tell reset from release, so the other modes' 100%% is real.\n");
      } else if (pct >= 80.0) {
         printf("  >= 80%%: release RETURNS the memory. The destroy policy is worth building.\n");
      } else if (pct <= 20.0) {
         printf("  <= 20%%: release does NOT return the memory. The destroy policy is DEAD.\n");
      } else {
         printf("  between 20%% and 80%%: attribute before deciding. No verdict from this run.\n");
      }
      printf("  (thresholds fixed in the header BEFORE the first run)\n");

      free(allocs);
   }
   return 0;
}
