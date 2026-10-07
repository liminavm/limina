// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * replayer-ipc: the two questions an out-of-process Metal replayer raises
 * (spikes/metal-heap-addressing/SKETCH.md §4).
 *
 * One binary, two roles. The parent is the REPLAYER: it owns Metal. It spawns itself as the
 * WORKER, which owns a VM (hv_vm_create) and a bare-metal guest (payload.S) that reads and
 * writes through stage 2. The replayer's port reaches the worker as an inherited registered
 * port (mach_ports_register), so the two can exchange Mach rights.
 *
 * A. Arena memory visible to both processes, and to the guest.
 *    A1  replayer-owned: a shared placement heap (KK's options) plus a full alias buffer,
 *        exported with mach_make_memory_entry_64. The worker maps it and hv_vm_maps it.
 *          - GPU fill -> worker CPU check -> guest read
 *          - worker CPU fill and guest write -> replayer GPU check
 *          - a texture placed in the heap: are its bytes in the worker's mapping?
 *          - lifetime: the replayer releases the heap and allocates and fills new ones;
 *            what does the worker's mapping hold now?
 *    A2  worker-owned: a named memory entry the worker creates and maps. The replayer maps it
 *        and wraps it with newBufferWithBytesNoCopy.
 *          - worker CPU fill -> GPU check
 *          - GPU fill -> worker CPU and guest check
 *          - lifetime: the replayer drops the buffer and unmaps; the worker re-checks
 * C. The replayer is SIGKILLed while the worker and its guest map a heap it owns; the
 *    worker re-checks the data afterwards.
 * B. Wake-up round trips, worker -> replayer -> worker: os_sync_wait_on_address (shared),
 *    a Mach semaphore pair, Mach message RPC, and spinning on shared memory. Each runs at
 *    gaps of 0 / 200 us / 5 ms, both across the two processes and between two threads of
 *    the worker.
 *
 * Usage: ./build.sh   (signs with the hypervisor entitlement; run with the sandbox off)
 */

#include <Hypervisor/Hypervisor.h>
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include <mach-o/dyld.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/mach_vm.h>
#include <os/os_sync_wait_on_address.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define MiB (1ULL << 20)
#define REGION (256 * MiB)
#define WORDS (REGION / 4)
/* A1 sub-ranges (in 32-bit words) */
#define A1_GPU_END (96 * MiB / 4)
#define A1_WORKER_END (192 * MiB / 4)
#define A1_TEX_OFF (192 * MiB)
#define TEX_TAG 0x7E000000u

#define RAM_BASE 0x80000000ULL
#define RAM_SIZE (16ULL << 20)
#define CTL_GPA (RAM_BASE + 0x3000)
#define GPA_A1 0xC0000000ULL
#define GPA_A2 0xD0000000ULL
#define MMIO_BASE 0x10000000ULL

#define S1 0x11111111u
#define S2 0x22222222u
#define S3 0x33333333u
#define S4 0x44444444u
#define S5 0x55555555u
#define S6 0x66666666u
#define GPA_C 0xE0000000ULL

#define CHECK_HV(expr)                                                                             \
   do {                                                                                            \
      hv_return_t _r = (expr);                                                                     \
      if (_r != HV_SUCCESS) {                                                                      \
         printf("[worker] FATAL %s -> 0x%x\n", #expr, (uint32_t)_r);                              \
         exit(1);                                                                                  \
      }                                                                                            \
   } while (0)
#define CHECK_KR(expr)                                                                             \
   do {                                                                                            \
      kern_return_t _k = (expr);                                                                   \
      if (_k != KERN_SUCCESS) {                                                                    \
         printf("FATAL [pid %d] %s -> %d (%s)\n", getpid(), #expr, _k, mach_error_string(_k));      \
         exit(1);                                                                                  \
      }                                                                                            \
   } while (0)

static double tick_ns;
static uint64_t now(void) { return mach_absolute_time(); }

/* ---------------------------------------------------------------- messages */

enum {
   M_HELLO = 100,     /* worker -> replayer: p0 = worker's port */
   M_A1_ENTRY,        /* replayer -> worker: p0 = heap memory entry (or null), d0 = kr */
   M_A1_DONE,         /* worker -> replayer: d0 = guest-written index, d1 = value */
   M_A1_RELEASED,     /* replayer -> worker */
   M_A1_CHECKED,      /* worker -> replayer */
   M_A2_ENTRY,        /* worker -> replayer: p0 = named entry */
   M_A2_DONE,         /* replayer -> worker */
   M_A2_RELEASED,     /* replayer -> worker */
   M_B_SETUP,         /* replayer -> worker: p0 = req semaphore, p1 = rep semaphore */
   M_B_SETUP2,        /* replayer -> worker: p0 = RPC port */
   M_B_DONE,          /* worker -> replayer */
   M_C_ENTRY,         /* replayer -> worker: p0 = a fresh heap's memory entry */
   M_C_MAPPED,        /* worker -> replayer: mapped; the replayer now kills itself */
};

typedef struct {
   mach_msg_header_t h;
   mach_msg_body_t body;
   mach_msg_port_descriptor_t p[2];
   uint64_t d[4];
} msg_t;

/* Generous: the kernel may deliver descriptors larger than the sender's layout. */
typedef struct {
   msg_t m;
   uint8_t slack[1024];
} rmsg_t;

static void send_msg(mach_port_t dest, int id, mach_port_t p0, mach_msg_type_name_t d0,
                     mach_port_t p1, mach_msg_type_name_t d1, uint64_t a, uint64_t b)
{
   msg_t m;
   memset(&m, 0, sizeof(m));
   m.h.msgh_bits = MACH_MSGH_BITS_SET(MACH_MSG_TYPE_COPY_SEND, 0, 0, MACH_MSGH_BITS_COMPLEX);
   m.h.msgh_size = sizeof(m);
   m.h.msgh_remote_port = dest;
   m.h.msgh_id = id;
   m.body.msgh_descriptor_count = 2;
   m.p[0].name = p0;
   m.p[0].disposition = d0;
   m.p[0].type = MACH_MSG_PORT_DESCRIPTOR;
   m.p[1].name = p1;
   m.p[1].disposition = d1;
   m.p[1].type = MACH_MSG_PORT_DESCRIPTOR;
   m.d[0] = a;
   m.d[1] = b;
   CHECK_KR(mach_msg(&m.h, MACH_SEND_MSG, sizeof(m), 0, MACH_PORT_NULL, MACH_MSG_TIMEOUT_NONE,
                     MACH_PORT_NULL));
}

static void recv_msg(mach_port_t port, rmsg_t *r, int expect)
{
   CHECK_KR(mach_msg(&r->m.h, MACH_RCV_MSG, 0, sizeof(*r), port, MACH_MSG_TIMEOUT_NONE,
                     MACH_PORT_NULL));
   if (r->m.h.msgh_id != expect) {
      printf("FATAL: expected message %d, got %d\n", expect, r->m.h.msgh_id);
      exit(1);
   }
}

static mach_port_t new_port(void)
{
   mach_port_t p;
   CHECK_KR(mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &p));
   CHECK_KR(mach_port_insert_right(mach_task_self(), p, p, MACH_MSG_TYPE_MAKE_SEND));
   return p;
}

/* ---------------------------------------------------------------- wake mechanisms */

/* Request/reply counters, each on its own cache line. */
typedef struct {
   _Alignas(128) _Atomic uint32_t os_req;
   _Alignas(128) _Atomic uint32_t os_rep;
   _Alignas(128) _Atomic uint64_t sp_req;
   _Alignas(128) _Atomic uint64_t sp_rep;
   _Alignas(128) _Atomic uint32_t sp_active;
} wake_t;

typedef struct {
   wake_t *w;
   bool shared;
   semaphore_t req, rep;
   mach_port_t rpc;
} server_t;

static void *os_server(void *arg)
{
   server_t *s = arg;
   os_sync_wait_on_address_flags_t wf =
      s->shared ? OS_SYNC_WAIT_ON_ADDRESS_SHARED : OS_SYNC_WAIT_ON_ADDRESS_NONE;
   os_sync_wake_by_address_flags_t kf =
      s->shared ? OS_SYNC_WAKE_BY_ADDRESS_SHARED : OS_SYNC_WAKE_BY_ADDRESS_NONE;
   uint32_t seen = 0; /* counters start at zero; reading them here races the first call */
   for (;;) {
      uint32_t v = atomic_load(&s->w->os_req);
      if (v == seen) {
         os_sync_wait_on_address(&s->w->os_req, v, 4, wf);
         continue;
      }
      seen = v;
      atomic_store(&s->w->os_rep, v);
      os_sync_wake_by_address_any(&s->w->os_rep, 4, kf);
   }
   return NULL;
}

static void *sem_server(void *arg)
{
   server_t *s = arg;
   for (;;) {
      semaphore_wait(s->req);
      semaphore_signal(s->rep);
   }
   return NULL;
}

static void *rpc_server(void *arg)
{
   server_t *s = arg;
   struct {
      mach_msg_header_t h;
      mach_msg_max_trailer_t t;
   } r;
   CHECK_KR(mach_msg(&r.h, MACH_RCV_MSG, 0, sizeof(r), s->rpc, MACH_MSG_TIMEOUT_NONE,
                     MACH_PORT_NULL));
   for (;;) {
      mach_msg_header_t rep = {
         .msgh_bits = MACH_MSGH_BITS_SET(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0, 0, 0),
         .msgh_size = sizeof(rep),
         .msgh_remote_port = r.h.msgh_remote_port,
         .msgh_id = 2,
      };
      /* reply and wait for the next request in one call */
      memcpy(&r.h, &rep, sizeof(rep));
      CHECK_KR(mach_msg(&r.h, MACH_SEND_MSG | MACH_RCV_MSG, sizeof(rep), sizeof(r), s->rpc,
                        MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL));
   }
   return NULL;
}

static void *spin_server(void *arg)
{
   server_t *s = arg;
   uint64_t seen = 0;
   for (;;) {
      if (!atomic_load_explicit(&s->w->sp_active, memory_order_relaxed)) {
         usleep(1000);
         continue;
      }
      uint64_t v = atomic_load_explicit(&s->w->sp_req, memory_order_acquire);
      if (v != seen) {
         seen = v;
         atomic_store_explicit(&s->w->sp_rep, v, memory_order_release);
      }
   }
   return NULL;
}

static void start_servers(server_t *s)
{
   pthread_t t;
   pthread_create(&t, NULL, os_server, s);
   pthread_create(&t, NULL, sem_server, s);
   pthread_create(&t, NULL, rpc_server, s);
   pthread_create(&t, NULL, spin_server, s);
}

/* client side, run by the worker */
static mach_port_t g_reply;

static void call_os(server_t *s)
{
   os_sync_wait_on_address_flags_t wf =
      s->shared ? OS_SYNC_WAIT_ON_ADDRESS_SHARED : OS_SYNC_WAIT_ON_ADDRESS_NONE;
   os_sync_wake_by_address_flags_t kf =
      s->shared ? OS_SYNC_WAKE_BY_ADDRESS_SHARED : OS_SYNC_WAKE_BY_ADDRESS_NONE;
   uint32_t r = atomic_fetch_add(&s->w->os_req, 1) + 1;
   os_sync_wake_by_address_any(&s->w->os_req, 4, kf);
   for (;;) {
      uint32_t v = atomic_load(&s->w->os_rep);
      if (v == r)
         return;
      os_sync_wait_on_address(&s->w->os_rep, v, 4, wf);
   }
}

static void call_sem(server_t *s)
{
   semaphore_signal(s->req);
   semaphore_wait(s->rep);
}

static void call_rpc(server_t *s)
{
   struct {
      mach_msg_header_t h;
      mach_msg_max_trailer_t t;
   } r;
   mach_msg_header_t *h = &r.h;
   h->msgh_bits = MACH_MSGH_BITS_SET(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE, 0, 0);
   h->msgh_size = sizeof(*h);
   h->msgh_remote_port = s->rpc;
   h->msgh_local_port = g_reply;
   h->msgh_id = 1;
   CHECK_KR(mach_msg(h, MACH_SEND_MSG | MACH_RCV_MSG, sizeof(*h), sizeof(r), g_reply,
                     MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL));
}

static void call_spin(server_t *s)
{
   uint64_t r = atomic_fetch_add_explicit(&s->w->sp_req, 1, memory_order_acq_rel) + 1;
   while (atomic_load_explicit(&s->w->sp_rep, memory_order_acquire) != r)
      ;
}

static int cmp64(const void *a, const void *b)
{
   uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
   return x < y ? -1 : x > y;
}

static void bench(const char *where, const char *name, void (*call)(server_t *), server_t *s)
{
   const struct {
      double gap_us;
      int n;
   } gaps[] = {{0, 20000}, {200, 2000}, {5000, 200}};
   atomic_store(&s->w->sp_active, call == call_spin);
   for (int i = 0; i < 200; i++)
      call(s); /* warm-up */
   for (int g = 0; g < 3; g++) {
      int n = gaps[g].n;
      uint64_t *t = malloc(n * sizeof(*t));
      uint64_t gap_ticks = (uint64_t)(gaps[g].gap_us * 1000.0 / tick_ns);
      for (int i = 0; i < n; i++) {
         if (gap_ticks) {
            uint64_t until = now() + gap_ticks;
            while (now() < until)
               ;
         }
         uint64_t a = now();
         call(s);
         t[i] = now() - a;
      }
      qsort(t, n, sizeof(*t), cmp64);
#define US(x) ((double)(x) * tick_ns / 1000.0)
      printf("[worker] %-13s %-9s gap=%5.0fus n=%-6d p50=%7.2fus p90=%7.2fus p99=%8.2fus "
             "max=%9.1fus\n",
             where, name, gaps[g].gap_us, n, US(t[n / 2]), US(t[n * 9 / 10]),
             US(t[n * 99 / 100]), US(t[n - 1]));
      free(t);
   }
   atomic_store(&s->w->sp_active, false);
}

/* ---------------------------------------------------------------- worker: guest */

static uint8_t *g_ram;
static hv_vcpu_t g_vcpu;
static hv_vcpu_exit_t *g_exit;
static uint64_t g_report;

static void run_to_ready(void)
{
   for (;;) {
      CHECK_HV(hv_vcpu_run(g_vcpu));
      if (g_exit->reason != HV_EXIT_REASON_EXCEPTION) {
         if (g_exit->reason == HV_EXIT_REASON_VTIMER_ACTIVATED)
            continue;
         printf("[worker] unexpected exit reason %u\n", g_exit->reason);
         exit(1);
      }
      uint64_t syn = g_exit->exception.syndrome, pa = g_exit->exception.physical_address;
      uint64_t pc;
      CHECK_HV(hv_vcpu_get_reg(g_vcpu, HV_REG_PC, &pc));
      if (((syn >> 26) & 0x3f) != 0x24 || pa < MMIO_BASE || pa >= MMIO_BASE + 0x1000) {
         printf("[worker] guest fault: syndrome=0x%llx pc=0x%llx pa=0x%llx\n", syn, pc, pa);
         exit(1);
      }
      CHECK_HV(hv_vcpu_set_reg(g_vcpu, HV_REG_PC, pc + 4));
      uint32_t srt = (syn >> 16) & 0x1f;
      if (pa - MMIO_BASE == 0x00)
         return;
      if (pa - MMIO_BASE == 0x08 && srt < 31)
         CHECK_HV(hv_vcpu_get_reg(g_vcpu, HV_REG_X0 + srt, &g_report));
   }
}

static uint32_t guest_read(uint64_t gpa)
{
   volatile uint64_t *ctl = (volatile uint64_t *)(g_ram + (CTL_GPA - RAM_BASE));
   ctl[0] = 1;
   ctl[1] = gpa;
   g_report = ~0ULL;
   run_to_ready();
   return (uint32_t)g_report;
}

static void guest_write(uint64_t gpa, uint32_t v)
{
   volatile uint64_t *ctl = (volatile uint64_t *)(g_ram + (CTL_GPA - RAM_BASE));
   ctl[0] = 2;
   ctl[1] = gpa;
   ctl[2] = v;
   run_to_ready();
}

static void guest_boot(const char *payload_path)
{
   FILE *f = fopen(payload_path, "rb");
   if (!f) {
      perror(payload_path);
      exit(1);
   }
   static uint8_t payload[0x1000];
   size_t len = fread(payload, 1, sizeof(payload), f);
   fclose(f);
   CHECK_HV(hv_vm_create(NULL));
   g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
   memcpy(g_ram, payload, len);
   CHECK_HV(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE,
                      HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
   CHECK_HV(hv_vcpu_create(&g_vcpu, &g_exit, NULL));
   CHECK_HV(hv_vcpu_set_reg(g_vcpu, HV_REG_CPSR, 0x3C5ULL));
   CHECK_HV(hv_vcpu_set_reg(g_vcpu, HV_REG_PC, RAM_BASE));
   run_to_ready();
}

/* CPU scans that survive a SIGBUS (memory yanked from under a mapping) */
static sigjmp_buf g_jb;
static void on_bus(int sig)
{
   (void)sig;
   siglongjmp(g_jb, 1);
}

typedef struct {
   uint64_t match, other, zero, faulted;
   uint64_t oi[4];
   uint32_t ov[4];
} scan_t;

/* count words in [lo,hi) equal to i ^ salt */
static scan_t scan(volatile uint32_t *p, uint64_t lo, uint64_t hi, uint32_t salt)
{
   scan_t s = {0};
   volatile uint64_t i = lo;
   if (sigsetjmp(g_jb, 1)) {
      s.faulted = 1;
      return s;
   }
   for (; i < hi; i++) {
      uint32_t v = p[i];
      if (v == ((uint32_t)i ^ salt))
         s.match++;
      else if (!v)
         s.zero++;
      else {
         if (s.other < 4) {
            s.oi[s.other] = i;
            s.ov[s.other] = v;
         }
         s.other++;
      }
   }
   return s;
}

static void print_scan(const char *what, scan_t s, uint64_t n)
{
   printf("[worker] %-44s %s match=%llu/%llu zero=%llu other=%llu", what,
          s.faulted ? "FAULTED (SIGBUS/SEGV)" : "", s.match, n, s.zero, s.other);
   for (uint64_t k = 0; k < s.other && k < 4; k++)
      printf(" [word %llu = 0x%08x]", s.oi[k], s.ov[k]);
   printf("\n");
}

static int worker_main(const char *payload_path)
{
   setvbuf(stdout, NULL, _IOLBF, 0);
   signal(SIGBUS, on_bus);
   signal(SIGSEGV, on_bus);
   mach_port_array_t ports;
   mach_msg_type_number_t nports = 0;
   CHECK_KR(mach_ports_lookup(mach_task_self(), &ports, &nports));
   mach_port_t rep = nports ? ports[0] : MACH_PORT_NULL;
   if (rep == MACH_PORT_NULL) {
      printf("[worker] no registered port inherited from the replayer\n");
      return 1;
   }
   mach_port_t me = new_port();
   g_reply = new_port();
   send_msg(rep, M_HELLO, me, MACH_MSG_TYPE_MAKE_SEND, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, 0,
            0);
   guest_boot(payload_path);
   rmsg_t r;

   /* ---- A1: replayer-owned heap */
   recv_msg(me, &r, M_A1_ENTRY);
   mach_port_t entry = r.m.p[0].name;
   if (entry == MACH_PORT_NULL) {
      printf("[worker] A1: the replayer could not export its heap (kr %llu); skipping A1\n",
             r.m.d[0]);
      send_msg(rep, M_A1_DONE, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
               MACH_MSG_TYPE_COPY_SEND, ~0ULL, 0);
      recv_msg(me, &r, M_A1_RELEASED);
      send_msg(rep, M_A1_CHECKED, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
               MACH_MSG_TYPE_COPY_SEND, 0, 0);
   } else {
      mach_vm_address_t a = 0;
      kern_return_t kr = mach_vm_map(mach_task_self(), &a, REGION, 0, VM_FLAGS_ANYWHERE, entry, 0,
                                     FALSE, VM_PROT_READ | VM_PROT_WRITE,
                                     VM_PROT_READ | VM_PROT_WRITE, VM_INHERIT_NONE);
      printf("[worker] A1: mach_vm_map of the heap entry -> %d (%s) at 0x%llx\n", kr,
             mach_error_string(kr), (unsigned long long)a);
      if (kr != KERN_SUCCESS)
         exit(1);
      volatile uint32_t *p = (volatile uint32_t *)a;
      print_scan("A1 GPU-filled range, read by worker CPU", scan(p, 0, A1_GPU_END, S1),
                 A1_GPU_END);
      hv_return_t hr = hv_vm_map((void *)a, GPA_A1, REGION, HV_MEMORY_READ | HV_MEMORY_WRITE);
      printf("[worker] A1: hv_vm_map of the shared heap -> 0x%x\n", hr);
      int gok = 0;
      for (int k = 0; k < 64; k++) {
         uint64_t idx = (uint64_t)k * (A1_GPU_END / 64) + 7;
         gok += guest_read(GPA_A1 + idx * 4) == ((uint32_t)idx ^ S1);
      }
      printf("[worker] A1: guest reads of GPU-written words through stage 2: %d/64 correct\n",
             gok);
      for (uint64_t i = A1_GPU_END; i < A1_WORKER_END; i++)
         p[i] = (uint32_t)i ^ S2;
      uint64_t gidx = 12345;
      guest_write(GPA_A1 + gidx * 4, 0xFEEDF00Du);
      uint64_t tags = 0;
      volatile uint32_t *tex = (volatile uint32_t *)(a + A1_TEX_OFF);
      for (uint64_t i = 0; i < 65536 * 4; i++)
         tags += (tex[i] & 0xFF000000u) == TEX_TAG;
      printf("[worker] A1: tagged texels of the heap-placed texture seen in the worker "
             "mapping: %llu/65536\n",
             tags);
      send_msg(rep, M_A1_DONE, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
               MACH_MSG_TYPE_COPY_SEND, gidx, 0xFEEDF00Du);
      recv_msg(me, &r, M_A1_RELEASED);
      print_scan("A1 after the replayer released the heap: [0,96M) vs S1",
                 scan(p, 0, A1_GPU_END, S1), A1_GPU_END);
      print_scan("A1 after release: [96M,192M) vs S2", scan(p, A1_GPU_END, A1_WORKER_END, S2),
                 A1_WORKER_END - A1_GPU_END);
      print_scan("A1 after release: [0,96M) vs the replayer's NEW heaps (S3)",
                 scan(p, 0, A1_GPU_END, S3), A1_GPU_END);
      uint64_t idx = 99;
      printf("[worker] A1 after release: guest read of word %llu = 0x%08x (S1 pattern 0x%08x)\n",
             idx, guest_read(GPA_A1 + idx * 4), (uint32_t)idx ^ S1);
      send_msg(rep, M_A1_CHECKED, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
               MACH_MSG_TYPE_COPY_SEND, 0, 0);
   }

   /* ---- A2: worker-owned memory */
   memory_object_size_t sz = REGION;
   mach_port_t ent2;
   CHECK_KR(mach_make_memory_entry_64(mach_task_self(), &sz, 0,
                                      MAP_MEM_NAMED_CREATE | VM_PROT_READ | VM_PROT_WRITE, &ent2,
                                      MACH_PORT_NULL));
   mach_vm_address_t a2 = 0;
   CHECK_KR(mach_vm_map(mach_task_self(), &a2, REGION, 0, VM_FLAGS_ANYWHERE, ent2, 0, FALSE,
                        VM_PROT_READ | VM_PROT_WRITE, VM_PROT_READ | VM_PROT_WRITE,
                        VM_INHERIT_NONE));
   volatile uint32_t *q = (volatile uint32_t *)a2;
   for (uint64_t i = 0; i < WORDS; i++)
      q[i] = (uint32_t)i ^ S4;
   CHECK_HV(hv_vm_map((void *)a2, GPA_A2, REGION, HV_MEMORY_READ | HV_MEMORY_WRITE));
   send_msg(rep, M_A2_ENTRY, ent2, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
            MACH_MSG_TYPE_COPY_SEND, 0, 0);
   recv_msg(me, &r, M_A2_DONE);
   /* the last page holds the wake counters of part B; scans stop short of it */
   const uint64_t A2_END = WORDS - 16384 / 4;
   print_scan("A2 GPU-written half, read by worker CPU (S5)", scan(q, WORDS / 2, A2_END, S5),
              A2_END - WORDS / 2);
   int g2 = 0;
   for (int k = 0; k < 64; k++) {
      uint64_t idx = WORDS / 2 + (uint64_t)k * (WORDS / 128) + 3;
      g2 += guest_read(GPA_A2 + idx * 4) == ((uint32_t)idx ^ S5);
   }
   printf("[worker] A2: guest reads of GPU-written words: %d/64 correct\n", g2);
   recv_msg(me, &r, M_A2_RELEASED);
   print_scan("A2 after the replayer dropped buffer+mapping: [0,128M) S4",
              scan(q, 0, WORDS / 2, S4), WORDS / 2);
   print_scan("A2 after drop: [128M,256M-16K) S5", scan(q, WORDS / 2, A2_END, S5),
              A2_END - WORDS / 2);

   /* ---- B: wake-ups. Shared counters in the last page of the A2 region. */
   recv_msg(me, &r, M_B_SETUP);
   server_t xs = {.w = (wake_t *)(a2 + REGION - 16384), .shared = true};
   xs.req = r.m.p[0].name;
   xs.rep = r.m.p[1].name;
   recv_msg(me, &r, M_B_SETUP2);
   xs.rpc = r.m.p[0].name;
   bench("cross-process", "os_sync", call_os, &xs);
   bench("cross-process", "semaphore", call_sem, &xs);
   bench("cross-process", "mach-rpc", call_rpc, &xs);
   bench("cross-process", "spin", call_spin, &xs);

   /* the same, between two threads of this process */
   static wake_t local;
   server_t ls = {.w = &local, .shared = false};
   CHECK_KR(semaphore_create(mach_task_self(), &ls.req, SYNC_POLICY_FIFO, 0));
   CHECK_KR(semaphore_create(mach_task_self(), &ls.rep, SYNC_POLICY_FIFO, 0));
   ls.rpc = new_port();
   start_servers(&ls);
   bench("in-process", "os_sync", call_os, &ls);
   bench("in-process", "semaphore", call_sem, &ls);
   bench("in-process", "mach-rpc", call_rpc, &ls);
   bench("in-process", "spin", call_spin, &ls);
   send_msg(rep, M_B_DONE, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
            MACH_MSG_TYPE_COPY_SEND, 0, 0);

   /* ---- C: the replayer dies (SIGKILL) while the worker and guest map its heap */
   recv_msg(me, &r, M_C_ENTRY);
   mach_vm_address_t c = 0;
   CHECK_KR(mach_vm_map(mach_task_self(), &c, REGION, 0, VM_FLAGS_ANYWHERE, r.m.p[0].name, 0,
                        FALSE, VM_PROT_READ | VM_PROT_WRITE, VM_PROT_READ | VM_PROT_WRITE,
                        VM_INHERIT_NONE));
   CHECK_HV(hv_vm_map((void *)c, GPA_C, REGION, HV_MEMORY_READ | HV_MEMORY_WRITE));
   volatile uint32_t *cp = (volatile uint32_t *)c;
   print_scan("C before the replayer dies (S6)", scan(cp, 0, WORDS, S6), WORDS);
   pid_t parent = getppid();
   send_msg(rep, M_C_MAPPED, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
            MACH_MSG_TYPE_COPY_SEND, 0, 0);
   while (getppid() == parent)
      usleep(1000);
   printf("[worker] C: the replayer (pid %d) is gone\n", parent);
   for (int round = 0; round < 2; round++) {
      sleep(round ? 3 : 1);
      char what[64];
      snprintf(what, sizeof(what), "C %d s after the replayer died (S6)", round ? 4 : 1);
      print_scan(what, scan(cp, 0, WORDS, S6), WORDS);
      int gc = 0;
      for (int k = 0; k < 64; k++) {
         uint64_t idx = (uint64_t)k * (WORDS / 64) + 5;
         gc += guest_read(GPA_C + idx * 4) == ((uint32_t)idx ^ S6);
      }
      printf("[worker] C: guest reads through stage 2: %d/64 correct\n", gc);
   }
   return 0;
}

/* ---------------------------------------------------------------- replayer */

static NSString *const MSL =
   @"#include <metal_stdlib>\n"
    "using namespace metal;\n"
    "kernel void fill(device uint *p [[buffer(0)]], constant uint2 &a [[buffer(1)]],\n"
    "                 uint i [[thread_position_in_grid]]) { uint k = a.x + i; p[k] = k ^ a.y; }\n"
    "kernel void check(device const uint *p [[buffer(0)]], constant uint2 &a [[buffer(1)]],\n"
    "                  device atomic_uint *bad [[buffer(2)]], uint i [[thread_position_in_grid]]) {\n"
    "   uint k = a.x + i; if (p[k] != (k ^ a.y)) atomic_fetch_add_explicit(bad, 1, memory_order_relaxed); }\n";

static id<MTLDevice> dev;
static id<MTLCommandQueue> queue;
static id<MTLComputePipelineState> ps_fill, ps_check;

/* Fill or check words [lo,hi) of buf with i ^ salt. Returns mismatches for a check. */
static uint32_t gpu(bool check, id<MTLBuffer> buf, id<MTLHeap> heap, uint64_t lo, uint64_t hi,
                    uint32_t salt)
{
   id<MTLBuffer> bad = [dev newBufferWithLength:4 options:MTLResourceStorageModeShared];
   *(uint32_t *)bad.contents = 0;
   uint32_t args[2] = {(uint32_t)lo, salt};
   id<MTLCommandBuffer> cb = [queue commandBuffer];
   id<MTLComputeCommandEncoder> e = [cb computeCommandEncoder];
   [e setComputePipelineState:check ? ps_check : ps_fill];
   [e setBuffer:buf offset:0 atIndex:0];
   [e setBytes:args length:sizeof(args) atIndex:1];
   [e setBuffer:bad offset:0 atIndex:2];
   if (heap)
      [e useHeap:heap];
   [e dispatchThreads:MTLSizeMake(hi - lo, 1, 1) threadsPerThreadgroup:MTLSizeMake(256, 1, 1)];
   [e endEncoding];
   [cb commit];
   [cb waitUntilCompleted];
   if (cb.error)
      printf("[replayer] GPU error: %s\n", cb.error.localizedDescription.UTF8String);
   return *(uint32_t *)bad.contents;
}

static id<MTLHeap> new_heap(void)
{
   MTLHeapDescriptor *d = [MTLHeapDescriptor new];
   d.type = MTLHeapTypePlacement;
   d.resourceOptions = MTLResourceStorageModeShared | MTLResourceHazardTrackingModeUntracked;
   d.size = REGION;
   return [dev newHeapWithDescriptor:d];
}

static int replayer_main(const char *self, const char *payload_path)
{
   setvbuf(stdout, NULL, _IOLBF, 0);
   dev = MTLCreateSystemDefaultDevice();
   queue = [dev newCommandQueue];
   NSError *err = nil;
   id<MTLLibrary> lib = [dev newLibraryWithSource:MSL options:nil error:&err];
   ps_fill = [dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"fill"]
                                                error:&err];
   ps_check = [dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"check"]
                                                 error:&err];

   mach_port_t me = new_port();
   /* Registered ports are inherited by child tasks; the worker's bootstrap port stays
    * launchd's, which its frameworks need. */
   mach_port_t reg[1] = {me};
   CHECK_KR(mach_ports_register(mach_task_self(), reg, 1));
   posix_spawnattr_t attr;
   posix_spawnattr_init(&attr);
   char *argv[] = {(char *)self, "worker", (char *)payload_path, NULL};
   pid_t pid;
   extern char **environ;
   if (posix_spawn(&pid, self, NULL, &attr, argv, environ) != 0) {
      perror("posix_spawn");
      return 1;
   }
   rmsg_t r;
   recv_msg(me, &r, M_HELLO);
   mach_port_t worker = r.m.p[0].name;
   printf("[replayer] worker pid %d connected\n", pid);

   /* ---- A1 */
   uint64_t a1_bad_s2 = 0, a1_guest_ok = 0;
   @autoreleasepool {
      id<MTLHeap> heap = new_heap();
      id<MTLBuffer> alias = [heap newBufferWithLength:REGION
                                              options:MTLResourceStorageModeShared |
                                                      MTLResourceHazardTrackingModeUntracked
                                               offset:0];
      gpu(false, alias, heap, 0, A1_GPU_END, S1);
      MTLTextureDescriptor *td =
         [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatR32Uint
                                                            width:256
                                                           height:256
                                                        mipmapped:NO];
      td.resourceOptions = MTLResourceStorageModeShared | MTLResourceHazardTrackingModeUntracked;
      id<MTLTexture> tex = [heap newTextureWithDescriptor:td offset:A1_TEX_OFF];
      uint32_t *px = malloc(65536 * 4);
      for (int i = 0; i < 65536; i++)
         px[i] = TEX_TAG | (uint32_t)i;
      [tex replaceRegion:MTLRegionMake2D(0, 0, 256, 256) mipmapLevel:0 withBytes:px
             bytesPerRow:1024];
      free(px);

      mach_port_t entry = MACH_PORT_NULL;
      struct {
         const char *n;
         vm_prot_t f;
      } tries[] = {{"plain", 0}, {"MAP_MEM_VM_SHARE", MAP_MEM_VM_SHARE}};
      kern_return_t kr = KERN_FAILURE;
      for (int t = 0; t < 2 && entry == MACH_PORT_NULL; t++) {
         memory_object_size_t sz = REGION;
         kr = mach_make_memory_entry_64(mach_task_self(), &sz,
                                        (memory_object_offset_t)(uintptr_t)alias.contents,
                                        tries[t].f | VM_PROT_READ | VM_PROT_WRITE, &entry,
                                        MACH_PORT_NULL);
         printf("[replayer] A1: mach_make_memory_entry_64(heap alias, %s) -> %d (%s), size "
                "%llu MiB\n",
                tries[t].n, kr, mach_error_string(kr), (unsigned long long)(sz / MiB));
         if (kr != KERN_SUCCESS)
            entry = MACH_PORT_NULL;
         else if (sz < REGION) {
            printf("[replayer] A1: entry covers less than the heap; not using it\n");
            entry = MACH_PORT_NULL;
         }
      }
      send_msg(worker, M_A1_ENTRY, entry, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
               MACH_MSG_TYPE_COPY_SEND, (uint64_t)kr, 0);
      recv_msg(me, &r, M_A1_DONE);
      if (r.m.d[0] != ~0ULL) {
         a1_bad_s2 = gpu(true, alias, heap, A1_GPU_END, A1_WORKER_END, S2);
         uint32_t gv = ((volatile uint32_t *)alias.contents)[r.m.d[0]];
         a1_guest_ok = gv == (uint32_t)r.m.d[1];
         printf("[replayer] A1: GPU check of the worker-filled range: %llu bad; guest-written "
                "word read by the replayer: 0x%08x (%s)\n",
                a1_bad_s2, gv, a1_guest_ok ? "ok" : "WRONG");
      }
      if (entry != MACH_PORT_NULL)
         mach_port_deallocate(mach_task_self(), entry);
      tex = nil;
      alias = nil;
      heap = nil;
   }
   /* Encourage reuse of the released pages: new heaps, GPU-filled with S3. */
   NSMutableArray *keep = [NSMutableArray array];
   @autoreleasepool {
      for (int i = 0; i < 4; i++) {
         id<MTLHeap> h = new_heap();
         id<MTLBuffer> b = [h newBufferWithLength:REGION
                                          options:MTLResourceStorageModeShared |
                                                  MTLResourceHazardTrackingModeUntracked
                                           offset:0];
         gpu(false, b, h, 0, A1_GPU_END, S3);
         [keep addObject:b];
         [keep addObject:h];
      }
   }
   send_msg(worker, M_A1_RELEASED, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
            MACH_MSG_TYPE_COPY_SEND, 0, 0);
   recv_msg(me, &r, M_A1_CHECKED);
   [keep removeAllObjects];

   /* ---- A2 */
   recv_msg(me, &r, M_A2_ENTRY);
   mach_port_t ent2 = r.m.p[0].name;
   mach_vm_address_t a2 = 0;
   CHECK_KR(mach_vm_map(mach_task_self(), &a2, REGION, 0, VM_FLAGS_ANYWHERE, ent2, 0, FALSE,
                        VM_PROT_READ | VM_PROT_WRITE, VM_PROT_READ | VM_PROT_WRITE,
                        VM_INHERIT_NONE));
   @autoreleasepool {
      id<MTLBuffer> b2 = [dev newBufferWithBytesNoCopy:(void *)a2
                                                length:REGION
                                               options:MTLResourceStorageModeShared
                                           deallocator:nil];
      printf("[replayer] A2: newBufferWithBytesNoCopy over the worker's memory -> %s\n",
             b2 ? "a buffer" : "nil");
      if (b2) {
         printf("[replayer] A2: GPU check of the worker-filled half: %u bad\n",
                gpu(true, b2, nil, 0, WORDS / 2, S4));
         gpu(false, b2, nil, WORDS / 2, WORDS, S5);
      }
      b2 = nil;
   }
   send_msg(worker, M_A2_DONE, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
            MACH_MSG_TYPE_COPY_SEND, 0, 0);
   /* the bench needs the region mapped here; drop-and-check used a separate mapping */
   mach_vm_address_t a2b = 0;
   CHECK_KR(mach_vm_map(mach_task_self(), &a2b, REGION, 0, VM_FLAGS_ANYWHERE, ent2, 0, FALSE,
                        VM_PROT_READ | VM_PROT_WRITE, VM_PROT_READ | VM_PROT_WRITE,
                        VM_INHERIT_NONE));
   CHECK_KR(mach_vm_deallocate(mach_task_self(), a2, REGION));
   send_msg(worker, M_A2_RELEASED, MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
            MACH_MSG_TYPE_COPY_SEND, 0, 0);

   /* ---- B */
   static server_t xs;
   xs.w = (wake_t *)(a2b + REGION - 16384);
   memset((void *)xs.w, 0, sizeof(wake_t));
   xs.shared = true;
   CHECK_KR(semaphore_create(mach_task_self(), &xs.req, SYNC_POLICY_FIFO, 0));
   CHECK_KR(semaphore_create(mach_task_self(), &xs.rep, SYNC_POLICY_FIFO, 0));
   xs.rpc = new_port();
   start_servers(&xs);
   send_msg(worker, M_B_SETUP, xs.req, MACH_MSG_TYPE_COPY_SEND, xs.rep, MACH_MSG_TYPE_COPY_SEND,
            0, 0);
   send_msg(worker, M_B_SETUP2, xs.rpc, MACH_MSG_TYPE_MAKE_SEND, MACH_PORT_NULL,
            MACH_MSG_TYPE_COPY_SEND, 0, 0);
   recv_msg(me, &r, M_B_DONE);

   /* ---- C */
   @autoreleasepool {
      id<MTLHeap> heap = new_heap();
      id<MTLBuffer> alias = [heap newBufferWithLength:REGION
                                              options:MTLResourceStorageModeShared |
                                                      MTLResourceHazardTrackingModeUntracked
                                               offset:0];
      gpu(false, alias, heap, 0, WORDS, S6);
      memory_object_size_t sz = REGION;
      mach_port_t entry;
      CHECK_KR(mach_make_memory_entry_64(mach_task_self(), &sz,
                                         (memory_object_offset_t)(uintptr_t)alias.contents,
                                         VM_PROT_READ | VM_PROT_WRITE, &entry, MACH_PORT_NULL));
      send_msg(worker, M_C_ENTRY, entry, MACH_MSG_TYPE_COPY_SEND, MACH_PORT_NULL,
               MACH_MSG_TYPE_COPY_SEND, 0, 0);
      recv_msg(me, &r, M_C_MAPPED);
      printf("[replayer] C: killing myself with SIGKILL while the worker maps my heap\n");
      kill(getpid(), SIGKILL);
   }
   return 0;
}

int main(int argc, char **argv)
{
   @autoreleasepool {
      mach_timebase_info_data_t tb;
      mach_timebase_info(&tb);
      tick_ns = (double)tb.numer / tb.denom;
      if (argc > 2 && !strcmp(argv[1], "worker"))
         return worker_main(argv[2]);
      char self[4096];
      uint32_t n = sizeof(self);
      _NSGetExecutablePath(self, &n);
      char real[4096];
      realpath(self, real);
      return replayer_main(real, argc > 1 ? argv[1] : "payload.bin");
   }
}
