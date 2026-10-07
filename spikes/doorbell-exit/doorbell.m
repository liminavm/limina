// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * doorbell-exit host driver: what a guest -> VMM -> guest round trip costs on HVF when the
 * guest's own trap carries the request, and the host work runs on the vCPU thread itself.
 *
 * The design under test (spikes/metal-heap-addressing/SKETCH.md §4) maps a device doorbell
 * page into the guest driver. A store or load there is a stage-2 data abort taken straight to
 * the VMM, whatever the guest EL, so a bare-metal EL1 guest measures the same trap a Linux
 * userspace driver would take. The guest times each round trip with CNTVCT_EL0
 * (payload.S).
 *
 * Exits:  store  str to the doorbell (a submit / "work is there" signal)
 *         load   ldr from it; the host returns a value in the guest's register (a query)
 *         hvc    hvc #0 (EL1 only; what a kernel-mediated call would use)
 *         none   no exit: the timer-read floor
 * Work on each exit, run before the guest resumes:
 *         nothing    the trap alone
 *         metal      a texture view created and its gpuResourceID read: a real Metal query
 *         handoff    passed to a parked worker thread (condvar) and waited for: the shape of
 *                    a dedicated device thread that is not spinning
 *         spin       passed to a worker thread that spins on an atomic: a dedicated device
 *                    thread burning a core
 * Each pair runs back to back (gap 0), with 200 us of guest busy-work between calls, and with
 * 5 ms between calls.
 *
 * Usage: ./build.sh   (signs with the hypervisor entitlement; run with the sandbox off)
 */

#include <Hypervisor/Hypervisor.h>
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include <mach/mach_time.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#define RAM_BASE 0x80000000ULL
#define RAM_SIZE (16ULL << 20)
#define CTL_GPA (RAM_BASE + 0x3000)
#define RES_GPA (RAM_BASE + 0x100000)
#define MAX_N ((RAM_SIZE - 0x100000) / 8)

#define MMIO_BASE 0x10000000ULL
#define M_READY 0x00
#define M_DONE 0x20
#define M_STORE 0x40
#define M_LOAD 0x48

#define EC_HVC 0x16
#define EC_DABT 0x24
#define BOOT_CPSR 0x3C5ULL

#define CHECK(expr)                                                                                \
   do {                                                                                            \
      hv_return_t _r = (expr);                                                                     \
      if (_r != HV_SUCCESS) {                                                                      \
         fprintf(stderr, "FATAL %s:%d %s -> 0x%x\n", __FILE__, __LINE__, #expr, (uint32_t)_r);   \
         exit(1);                                                                                  \
      }                                                                                            \
   } while (0)

enum work { W_NOTHING, W_METAL, W_HANDOFF, W_SPIN };
static const char *work_name[] = {"nothing", "metal", "handoff", "spin"};

static uint8_t *g_ram;
static hv_vcpu_t g_vcpu;
static hv_vcpu_exit_t *g_exit;
static uint64_t g_exits, g_load_value;
static enum work g_work;

/* metal work */
static id<MTLTexture> g_tex;
static uint64_t g_sink;

static void metal_work(void)
{
   @autoreleasepool {
      id<MTLTexture> v = [g_tex newTextureViewWithPixelFormat:MTLPixelFormatRGBA8Unorm_sRGB];
      g_sink += v.gpuResourceID._impl;
   }
}

/* handoff worker: parked on a condvar */
static pthread_mutex_t g_mx = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t g_req_cv = PTHREAD_COND_INITIALIZER, g_rep_cv = PTHREAD_COND_INITIALIZER;
static uint64_t g_req, g_rep;

static void *handoff_worker(void *arg)
{
   (void)arg;
   pthread_mutex_lock(&g_mx);
   for (;;) {
      while (g_req == g_rep)
         pthread_cond_wait(&g_req_cv, &g_mx);
      g_rep = g_req;
      pthread_cond_signal(&g_rep_cv);
   }
   return NULL;
}

static void handoff(void)
{
   pthread_mutex_lock(&g_mx);
   g_req++;
   pthread_cond_signal(&g_req_cv);
   while (g_rep != g_req)
      pthread_cond_wait(&g_rep_cv, &g_mx);
   pthread_mutex_unlock(&g_mx);
}

/* spin worker */
static _Atomic uint64_t g_sreq, g_srep;
static _Atomic bool g_spin_on;

static void *spin_worker(void *arg)
{
   (void)arg;
   uint64_t seen = 0;
   for (;;) {
      if (!atomic_load_explicit(&g_spin_on, memory_order_relaxed)) {
         usleep(1000);
         continue;
      }
      uint64_t r = atomic_load_explicit(&g_sreq, memory_order_acquire);
      if (r != seen) {
         seen = r;
         atomic_store_explicit(&g_srep, r, memory_order_release);
      }
   }
   return NULL;
}

static void spin_handoff(void)
{
   uint64_t r = atomic_fetch_add_explicit(&g_sreq, 1, memory_order_acq_rel) + 1;
   while (atomic_load_explicit(&g_srep, memory_order_acquire) != r)
      ;
}

static void do_work(void)
{
   switch (g_work) {
   case W_NOTHING: break;
   case W_METAL: metal_work(); break;
   case W_HANDOFF: handoff(); break;
   case W_SPIN: spin_handoff(); break;
   }
}

/* Run until the guest next reports READY (true) or DONE (false), serving doorbells. */
static bool run_to_ready(void)
{
   for (;;) {
      CHECK(hv_vcpu_run(g_vcpu));
      if (g_exit->reason != HV_EXIT_REASON_EXCEPTION) {
         if (g_exit->reason == HV_EXIT_REASON_VTIMER_ACTIVATED)
            continue;
         fprintf(stderr, "unexpected exit reason %u\n", g_exit->reason);
         exit(1);
      }
      uint64_t syn = g_exit->exception.syndrome, pa = g_exit->exception.physical_address;
      uint64_t ec = (syn >> 26) & 0x3f;
      if (ec == EC_HVC) {
         /* HVF reports an HVC with the PC already past the instruction. */
         g_exits++;
         do_work();
         continue;
      }
      uint64_t pc = 0;
      CHECK(hv_vcpu_get_reg(g_vcpu, HV_REG_PC, &pc));
      if (ec != EC_DABT || pa < MMIO_BASE || pa >= MMIO_BASE + 0x1000) {
         fprintf(stderr, "unhandled exception syndrome=0x%llx pc=0x%llx pa=0x%llx\n", syn, pc,
                 pa);
         exit(1);
      }
      CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_PC, pc + 4));
      bool write = (syn >> 6) & 1;
      uint32_t srt = (syn >> 16) & 0x1f;
      switch (pa - MMIO_BASE) {
      case M_READY: return true;
      case M_DONE: return false;
      case M_STORE:
         g_exits++;
         do_work();
         break;
      case M_LOAD:
         g_exits++;
         do_work();
         if (!write && srt < 31)
            CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_X0 + srt, ++g_load_value));
         break;
      default: fprintf(stderr, "unexpected MMIO 0x%llx\n", pa); exit(1);
      }
   }
}

static int cmp(const void *a, const void *b)
{
   uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
   return x < y ? -1 : x > y;
}

static double tick_ns;

static void scenario(int mode, const char *exit_name, enum work w, double gap_us, uint64_t n)
{
   volatile uint64_t *ctl = (volatile uint64_t *)(g_ram + (CTL_GPA - RAM_BASE));
   uint64_t *res = (uint64_t *)(g_ram + (RES_GPA - RAM_BASE));
   g_work = w;
   atomic_store(&g_spin_on, w == W_SPIN);
   g_exits = 0;
   g_load_value = 0;
   ctl[0] = mode;
   ctl[1] = n;
   ctl[2] = (uint64_t)(gap_us * 1000.0 / tick_ns);
   ctl[3] = 0;
   if (!run_to_ready()) {
      fprintf(stderr, "guest said DONE mid-scenario\n");
      exit(1);
   }
   atomic_store(&g_spin_on, false);
   uint64_t expect_exits = mode == 4 ? 0 : n;
   bool ok = g_exits == expect_exits && (mode != 2 || ctl[3] == n);
   uint64_t *t = malloc(n * sizeof(*t));
   memcpy(t, res, n * sizeof(*t));
   qsort(t, n, sizeof(*t), cmp);
   double sum = 0;
   for (uint64_t i = 0; i < n; i++)
      sum += t[i];
#define US(x) ((double)(x) * tick_ns / 1000.0)
   printf("%-5s %-8s gap=%5.0fus n=%-6llu p50=%7.2fus p90=%7.2fus p99=%7.2fus max=%8.1fus "
          "mean=%7.2fus %s\n",
          exit_name, work_name[w], gap_us, (unsigned long long)n, US(t[n / 2]),
          US(t[n * 9 / 10]), US(t[n * 99 / 100]), US(t[n - 1]), US(sum / n),
          ok ? "" : "EXIT-COUNT/VALUE MISMATCH");
   fflush(stdout);
   free(t);
}

int main(int argc, char **argv)
{
   @autoreleasepool {
      const char *payload_path = argc > 1 ? argv[1] : "payload.bin";
      mach_timebase_info_data_t tb;
      mach_timebase_info(&tb);
      tick_ns = (double)tb.numer / tb.denom;

      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      MTLTextureDescriptor *d =
         [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm
                                                            width:64
                                                           height:64
                                                        mipmapped:NO];
      d.usage = MTLTextureUsageShaderRead | MTLTextureUsagePixelFormatView;
      g_tex = [dev newTextureWithDescriptor:d];

      pthread_t th;
      pthread_create(&th, NULL, handoff_worker, NULL);
      pthread_create(&th, NULL, spin_worker, NULL);

      FILE *f = fopen(payload_path, "rb");
      if (!f) {
         perror(payload_path);
         return 1;
      }
      static uint8_t payload[0x1000];
      size_t len = fread(payload, 1, sizeof(payload), f);
      fclose(f);

      CHECK(hv_vm_create(NULL));
      g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
      memcpy(g_ram, payload, len);
      CHECK(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE,
                      HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
      CHECK(hv_vcpu_create(&g_vcpu, &g_exit, NULL));
      CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_CPSR, BOOT_CPSR));
      CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_PC, RAM_BASE));
      if (!run_to_ready()) {
         fprintf(stderr, "guest never became ready\n");
         return 1;
      }
      printf("timebase: %.3f ns/tick\n", tick_ns);

      struct {
         int mode;
         const char *name;
      } exits[] = {{4, "none"}, {1, "store"}, {2, "load"}, {3, "hvc"}};
      struct {
         double gap;
         uint64_t n;
      } gaps[] = {{0, 20000}, {200, 2000}, {5000, 200}};
      /* warm-up: first exits, first Metal view */
      scenario(2, "load", W_METAL, 0, 2000);
      for (unsigned g = 0; g < 3; g++) {
         scenario(4, "none", W_NOTHING, gaps[g].gap, gaps[g].n);
         for (unsigned e = 1; e < 4; e++)
            for (int w = 0; w < 4; w++) {
               if (e != 2 && w != W_NOTHING)
                  continue; /* work variants on the query (load) path only */
               scenario(exits[e].mode, exits[e].name, (enum work)w, gaps[g].gap, gaps[g].n);
            }
      }
      volatile uint64_t *ctl = (volatile uint64_t *)(g_ram + (CTL_GPA - RAM_BASE));
      ctl[0] = 0;
      run_to_ready();
      CHECK(hv_vcpu_destroy(g_vcpu));
      CHECK(hv_vm_destroy());
      (void)g_sink;
      (void)MAX_N;
   }
   return 0;
}
