// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// DYLD interposer that names WHO mints the leaking IOAccelerator regions.
//
// WHY. ioclasscount established that each leaked `IOAccelerator (graphics)` VM region is one AGX
// kernel resource, and the standalone MTL4 repro (../mtl4-repro/) proved that plain Metal —
// heaps, textures, compute, render passes, allocators, commits — returns every one of them. So
// the allocation is being triggered by something specific in the real stack, and no amount of
// further hypothesising about WHICH object class narrows it. A backtrace at the mapping call
// names the caller outright.
//
// WHICH CALL — measured, not assumed. The obvious guess was `IOConnectMapMemory64`, since every
// IOAccelerator region is a mapping established through IOKit. It counts **zero**, in the repro
// and in the worker alike. Measured during a real aquarium run: `iomap=0`, `vm_map=164`,
// **`callm=32339`** — the last is the right order of magnitude for the ~25k resources leaked per
// workload cycle, while both user-space mapping APIs sit nearly idle. The kernel allocates AND
// maps the resource inside the `IOConnectCallMethod` user-client call, which is exactly why
// interposing the mapping API sees nothing.
//
// `leaks`, `heap` and `malloc_history` are useless here for the same reason — these are
// kernel-established mappings that never pass through the malloc/VM layer those tools rely on.
//
// ⚠ BOTH the supervisor and the worker load this, and they share a log. Every line carries the
// pid: the supervisor's busiest stack is its own CATransaction present path, which is NOT the
// leak and will otherwise sit at the top of a merged reading.
//
// It also counts UNMAPS, which is not decoration: the vmmap adjacency pass showed 59 365 of
// 59 465 region boundaries exactly contiguous, i.e. a pure ratchet with no unmap churn at all.
// If that is right, the unmap counter stays near zero and the map counter climbs. If unmaps DO
// fire in bulk, the contiguity reading is wrong and this says so immediately.
//
// The worker carries `com.apple.security.cs.allow-dyld-environment-variables` AND
// `com.apple.security.cs.disable-library-validation`, so an ad-hoc-signed dylib inserts fine.
//
// Usage (see run-traced.sh):
//   DYLD_INSERT_LIBRARIES=/path/to/libiokittrace.dylib <the worker>
//   LIMINA_IOTRACE_DUMP=<secs>   dump the top buckets on a timer (default 20)
//   LIMINA_IOTRACE_DEPTH=<n>     backtrace frames to key on (default 14)
//
// ⚠ Interposing only works for calls that go through the dynamic linker's stub for THIS symbol.
// If AGXMetal reaches the kernel by another route the counters stay at zero — which is a
// RESULT, not a broken build: it says the mapping is established somewhere else, and the next
// candidates are mach_vm_map / mach_makememoryentry. Do not read a zero as "no allocations".

#include <dlfcn.h>
#include <execinfo.h>
#include <mach/mach.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <IOKit/IOKitLib.h>
#include <mach/mach_vm.h>
#include <sys/mman.h>

// Which symbol actually establishes these mappings is NOT known a priori: the first version of
// this interposed only IOConnectMapMemory64 and counted ZERO on a workload that demonstrably
// creates regions. That is a result, not a bug — it says AGXMetal reaches the kernel another
// way — but it is only visible because the instrument was proven against a known-allocating
// workload first. So cast a wide net across every plausible entry point and let the counters
// say which one carries the traffic. A symbol that stays at zero is eliminated; the one that
// tracks the region count is the hook.
//
// ⚠ AGXMetal lives in the dyld shared cache. Calls made *within* the cache may be bound
// directly rather than through a stub, in which case DYLD interposition cannot see them at all
// and every counter here stays zero. That outcome is informative and must not be read as
// "nothing allocated" — it means this technique is the wrong one and the next move is
// correlation from inside our own code (KK encoder-rate stats against region growth).

#define MAX_DEPTH   32
#define MAX_BUCKETS 512

struct bucket {
   uint64_t hash;
   void    *frames[MAX_DEPTH];
   int      nframes;
   uint64_t maps;
   uint64_t bytes;
   uint32_t selector;   /* IOKit user-client selector, when the bucket came from a call method */
};

static struct bucket   g_buckets[MAX_BUCKETS];
static int             g_nbuckets;
static pthread_mutex_t g_lock = PTHREAD_MUTEX_INITIALIZER;
static _Atomic uint64_t g_maps, g_unmaps, g_map_bytes;
static int             g_depth = 14;

/* Per-symbol hit counters: whichever tracks the region count is the hook we want. */
static _Atomic uint64_t g_hits_mach_vm_map, g_hits_mach_vm_allocate, g_hits_mmap,
                        g_hits_call_method, g_hits_call_struct;

/* Not armed until the constructor finishes, so nothing takes a backtrace during dyld's own
 * library loading. */
static _Atomic int g_armed;
static _Atomic uint64_t g_recorded;
static int g_every = 2000; /* dump one report per N recorded allocations (LIMINA_IOTRACE_EVERY) */
/* IOConnectCallMethod runs ~540/s in the worker; sample 1-in-N if the stack capture ever costs
 * enough to distort the thing being measured. 1 = capture every call. */
static int g_callm_sample = 1;

/* The leaked-region size histogram from vmmap -v at 59 196 regions (see
 * ../data/c2-closed.size-histogram.txt): 41 189 x 32K carry the COUNT, 2 394 x 1024K +
 * 2 393 x 768K carry the BYTES, 6 042 x 16K track IOGPUDeviceShmem, plus the 48K/160K pair.
 * Filtering to exactly these sizes keeps the stack capture off the early-load path and off the
 * unrelated traffic, so what survives is the allocations under investigation and nothing else.
 * LIMINA_IOTRACE_ALL=1 disables the filter when a size turns out not to be on this list. */
static int g_filter = 1;
static int
size_is_interesting(uint64_t size)
{
   if (!g_filter) return 1;
   switch (size) {
   case 16384: case 32768: case 49152: case 163840:
   case 786432: case 1048576:
      return 1;
   default:
      return 0;
   }
}

typedef struct { const void *replacement; const void *replacee; } interpose_t;

static void
record_sel(void **frames, int n, uint64_t bytes, uint32_t selector)
{
   uint64_t h = 1469598103934665603ull;
   for (int i = 0; i < n; i++) {
      h ^= (uint64_t)(uintptr_t)frames[i];
      h *= 1099511628211ull;
   }
   h ^= selector; h *= 1099511628211ull;
   pthread_mutex_lock(&g_lock);
   for (int i = 0; i < g_nbuckets; i++) {
      if (g_buckets[i].hash == h) {
         g_buckets[i].maps++;
         g_buckets[i].bytes += bytes;
         pthread_mutex_unlock(&g_lock);
         return;
      }
   }
   if (g_nbuckets < MAX_BUCKETS) {
      struct bucket *b = &g_buckets[g_nbuckets++];
      b->hash = h;
      b->nframes = n;
      memcpy(b->frames, frames, (size_t)n * sizeof(void *));
      b->maps = 1;
      b->bytes = bytes;
      b->selector = selector;
   }
   /* Table full: further distinct stacks are dropped rather than mis-attributed. The dump
    * prints the bucket count so a full table is visible instead of silently truncating. */
   pthread_mutex_unlock(&g_lock);
}

static void
dump(void)
{
   pthread_mutex_lock(&g_lock);
   /* Sort by map count: the leaking caller is meant to be the first stack printed. */
   int order[MAX_BUCKETS];
   for (int i = 0; i < g_nbuckets; i++) order[i] = i;
   for (int a = 0; a < g_nbuckets; a++)
      for (int b = a + 1; b < g_nbuckets; b++)
         if (g_buckets[order[b]].maps > g_buckets[order[a]].maps) {
            int t = order[a]; order[a] = order[b]; order[b] = t;
         }

   fprintf(stderr,
           "[IOTRACE %d] iomap=%llu iounmap=%llu map_bytes=%.1f MiB | vm_map=%llu vm_alloc=%llu "
           "mmap=%llu callm=%llu callstruct=%llu | buckets=%d%s\n",
           getpid(),
           (unsigned long long)atomic_load(&g_maps),
           (unsigned long long)atomic_load(&g_unmaps),
           (double)atomic_load(&g_map_bytes) / (1024.0 * 1024.0),
           (unsigned long long)atomic_load(&g_hits_mach_vm_map),
           (unsigned long long)atomic_load(&g_hits_mach_vm_allocate),
           (unsigned long long)atomic_load(&g_hits_mmap),
           (unsigned long long)atomic_load(&g_hits_call_method),
           (unsigned long long)atomic_load(&g_hits_call_struct),
           g_nbuckets, g_nbuckets >= MAX_BUCKETS ? " (TABLE FULL — stacks dropped)" : "");

   /* PER-SELECTOR TOTALS — the number that decides causation. A single stack's count is only a
    * lower bound: the same selector is reached from dozens of distinct call sites, so the
    * per-stack table (truncated below) undercounts it. Totalling by selector makes the
    * comparison direct: if selector 9's total matches the AGXResource delta over the same
    * window, the identification is arithmetic rather than pattern-matching — which is the bar
    * this investigation has twice failed to clear. */
   {
      uint32_t sels[64]; uint64_t tot[64]; int ns = 0;
      for (int i = 0; i < g_nbuckets; i++) {
         int k = -1;
         for (int j = 0; j < ns; j++) if (sels[j] == g_buckets[i].selector) { k = j; break; }
         if (k < 0 && ns < 64) { k = ns++; sels[k] = g_buckets[i].selector; tot[k] = 0; }
         if (k >= 0) tot[k] += g_buckets[i].maps;
      }
      for (int a = 0; a < ns; a++)
         for (int b = a + 1; b < ns; b++)
            if (tot[b] > tot[a]) {
               uint64_t tt = tot[a]; tot[a] = tot[b]; tot[b] = tt;
               uint32_t ts = sels[a]; sels[a] = sels[b]; sels[b] = ts;
            }
      fprintf(stderr, "[IOTRACE %d] PER-SELECTOR TOTALS:", getpid());
      for (int i = 0; i < ns && i < 8; i++)
         fprintf(stderr, " sel%u=%llu", sels[i], (unsigned long long)tot[i]);
      fprintf(stderr, "\n");
   }

   /* Compact per-stack table. The busiest stack is NOT automatically the leaking one — measured,
    * the top bucket is IOGPUCommandQueueSubmitCommandBuffers, i.e. plain submission, which fires
    * per commit and allocates nothing that persists. */
   fprintf(stderr, "[IOTRACE %d] bucket table (count selector):\n", getpid());
   for (int i = 0; i < g_nbuckets && i < 24; i++) {
      struct bucket *b = &g_buckets[order[i]];
      fprintf(stderr, "[IOTRACE %d]   %8llu  sel=%-6u  top=%p\n", getpid(),
              (unsigned long long)b->maps, b->selector, b->nframes ? b->frames[0] : NULL);
   }

   int show = g_nbuckets < 3 ? g_nbuckets : 3;
   for (int i = 0; i < show; i++) {
      struct bucket *b = &g_buckets[order[i]];
      fprintf(stderr, "[IOTRACE %d] --- #%d: %llu calls, %.1f MiB, selector %u ---\n", getpid(), i + 1,
              (unsigned long long)b->maps, (double)b->bytes / (1024.0 * 1024.0), b->selector);
      char **syms = backtrace_symbols(b->frames, b->nframes);
      if (syms) {
         for (int f = 0; f < b->nframes; f++)
            fprintf(stderr, "[IOTRACE %d]     %s\n", getpid(), syms[f]);
         free(syms);
      }
   }
   fflush(stderr);
   pthread_mutex_unlock(&g_lock);
}

static void *
dumper(void *arg)
{
   long secs = (long)(intptr_t)arg;
   for (;;) { sleep((unsigned)secs); dump(); }
   return NULL;
}

static kern_return_t
limina_IOConnectMapMemory64(io_connect_t connect, uint32_t memoryType, task_port_t intoTask,
                            mach_vm_address_t *atAddress, mach_vm_size_t *ofSize,
                            IOOptionBits options)
{
   kern_return_t kr = IOConnectMapMemory64(connect, memoryType, intoTask, atAddress, ofSize, options);
   if (kr == KERN_SUCCESS) {
      atomic_fetch_add(&g_maps, 1ull);
      atomic_fetch_add(&g_map_bytes, ofSize ? (uint64_t)*ofSize : 0ull);
      void *frames[MAX_DEPTH];
      int n = backtrace(frames, g_depth < MAX_DEPTH ? g_depth : MAX_DEPTH);
      /* Drop frame 0 (this function) so identical callers hash together. */
      if (n > 1) record_sel(frames + 1, n - 1, ofSize ? (uint64_t)*ofSize : 0ull, 0);
   }
   return kr;
}

static kern_return_t
limina_IOConnectUnmapMemory64(io_connect_t connect, uint32_t memoryType, task_port_t fromTask,
                              mach_vm_address_t atAddress)
{
   atomic_fetch_add(&g_unmaps, 1ull);
   return IOConnectUnmapMemory64(connect, memoryType, fromTask, atAddress);
}

static kern_return_t
limina_mach_vm_map(vm_map_t target, mach_vm_address_t *address, mach_vm_size_t size,
                   mach_vm_offset_t mask, int flags, mem_entry_name_port_t object,
                   memory_object_offset_t offset, boolean_t copy, vm_prot_t cur,
                   vm_prot_t max, vm_inherit_t inheritance)
{
   kern_return_t kr = mach_vm_map(target, address, size, mask, flags, object, offset, copy,
                                  cur, max, inheritance);
   /* PHASE 1 (discovery) was count-only across several candidate symbols, because taking a
    * backtrace inside a VM primitive is not safe on the early-load path — backtrace() itself
    * allocates and can re-enter. (mmap is deliberately NOT interposed at all: dyld uses it
    * while loading this very library, and interposing it silently killed the process before it
    * printed a single line.) Phase 1's answer, measured on the standalone repro where regions
    * demonstrably go 4 -> 25: iomap=0, callm=45, callstruct=6, **vm_map=27**. So mach_vm_map
    * is the carrier and IOConnectMapMemory64 — the obvious guess, and this file's first
    * version — is not involved at all.
    *
    * PHASE 2 captures stacks HERE, and only for allocation sizes that match the leaked region
    * histogram. That filter is what makes it safe and readable: it skips the early-load traffic
    * entirely and cuts the volume to the allocations actually under investigation. */
   if (kr == KERN_SUCCESS) {
      atomic_fetch_add(&g_hits_mach_vm_map, 1ull);
      if (atomic_load(&g_armed) && size_is_interesting((uint64_t)size)) {
         void *frames[MAX_DEPTH];
         int n = backtrace(frames, g_depth < MAX_DEPTH ? g_depth : MAX_DEPTH);
         if (n > 1) record_sel(frames + 1, n - 1, (uint64_t)size, 0);
         /* Dump from HERE, on a count, rather than from a timer thread. A thread created in a
          * library constructor does not survive the fork the worker does on the way up: the
          * first version printed its "armed" banner four times and then never dumped again,
          * which looks identical to "nothing was ever allocated". Counting is fork-proof. */
         uint64_t seen = atomic_fetch_add(&g_recorded, 1ull) + 1;
         if (g_every && (seen % (uint64_t)g_every) == 0) dump();
      }
   }
   return kr;
}

static kern_return_t
limina_IOConnectCallMethod(mach_port_t connection, uint32_t selector, const uint64_t *input,
                           uint32_t inputCnt, const void *inputStruct, size_t inputStructCnt,
                           uint64_t *output, uint32_t *outputCnt, void *outputStruct,
                           size_t *outputStructCnt)
{
   /* THIS is the one that carries the traffic in the real worker. Measured during an aquarium
    * run: iomap=0, vm_map=164, **callm=32339** — the right order of magnitude for the ~25k
    * resources leaked per workload cycle, while both user-space mapping APIs are nearly idle.
    * The kernel evidently allocates AND maps the resource inside the user-client call, which is
    * exactly why interposing IOConnectMapMemory64 (the obvious guess) counted zero.
    *
    * Bucketed by (stack, selector) so the dump names both the calling code and which user-client
    * method it invoked. */
   uint64_t n = atomic_fetch_add(&g_hits_call_method, 1ull) + 1;
   if (atomic_load(&g_armed) && (g_callm_sample <= 1 || (n % (uint64_t)g_callm_sample) == 0)) {
      void *frames[MAX_DEPTH];
      int nf = backtrace(frames, g_depth < MAX_DEPTH ? g_depth : MAX_DEPTH);
      if (nf > 1) record_sel(frames + 1, nf - 1, 0, selector);
      uint64_t seen = atomic_fetch_add(&g_recorded, 1ull) + 1;
      if (g_every && (seen % (uint64_t)g_every) == 0) dump();
   }
   return IOConnectCallMethod(connection, selector, input, inputCnt, inputStruct, inputStructCnt,
                              output, outputCnt, outputStruct, outputStructCnt);
}

static kern_return_t
limina_IOConnectCallStructMethod(mach_port_t connection, uint32_t selector,
                                 const void *inputStruct, size_t inputStructCnt,
                                 void *outputStruct, size_t *outputStructCnt)
{
   atomic_fetch_add(&g_hits_call_struct, 1ull);
   return IOConnectCallStructMethod(connection, selector, inputStruct, inputStructCnt,
                                    outputStruct, outputStructCnt);
}

__attribute__((used)) static const interpose_t g_interposers[]
   __attribute__((section("__DATA,__interpose"))) = {
      { (const void *)limina_IOConnectMapMemory64,     (const void *)IOConnectMapMemory64 },
      { (const void *)limina_IOConnectUnmapMemory64,   (const void *)IOConnectUnmapMemory64 },
      { (const void *)limina_mach_vm_map,              (const void *)mach_vm_map },
      { (const void *)limina_IOConnectCallMethod,      (const void *)IOConnectCallMethod },
      { (const void *)limina_IOConnectCallStructMethod,(const void *)IOConnectCallStructMethod },
   };

__attribute__((constructor)) static void
limina_iotrace_init(void)
{
   const char *d = getenv("LIMINA_IOTRACE_DEPTH");
   if (d && *d) g_depth = (int)strtol(d, NULL, 10);
   const char *a = getenv("LIMINA_IOTRACE_ALL");
   if (a && *a && strcmp(a, "0")) g_filter = 0;
   const char *cs = getenv("LIMINA_IOTRACE_CALLM_SAMPLE");
   if (cs && *cs) g_callm_sample = (int)strtol(cs, NULL, 10);
   const char *e = getenv("LIMINA_IOTRACE_EVERY");
   if (e && *e) g_every = (int)strtol(e, NULL, 10);
   const char *s = getenv("LIMINA_IOTRACE_DUMP");
   long secs = (s && *s) ? strtol(s, NULL, 10) : 20;
   fprintf(stderr, "[IOTRACE %d] armed: dump every %d recorded allocs (and %lds), depth %d, "
                   "size filter %s\n", getpid(), g_every, secs, g_depth,
           g_filter ? "ON (leaked sizes only)" : "OFF (every size)");
   /* The timer thread is kept only as a belt-and-braces heartbeat for the process that does not
    * fork; the count-based dump above is the one that is actually relied on. */
   pthread_t t;
   pthread_create(&t, NULL, dumper, (void *)(intptr_t)secs);
   pthread_detach(t);
   atomic_store(&g_armed, 1);
}
