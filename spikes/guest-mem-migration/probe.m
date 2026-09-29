// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * guest-mem-migration host driver.
 *
 * Question: at the 4 KiB IPA granule, can the host move live guest pages onto
 * host memory of its choosing — hv_vm_unmap the 4 KiB page, copy it, hv_vm_map
 * the new memory at the same guest-physical address — without the guest
 * noticing, and without losing a store made while the move is in flight, on
 * any of the VM's vCPUs? If so, scattered 4 KiB guest pages can be gathered
 * into one contiguous 16 KiB-aligned host buffer that Metal accepts as a
 * no-copy buffer, which no host-side remap can do (spikes/userptr-remap-metal/).
 *
 * Sequence (payload.S is the guest's command loop; every command goes to every
 * vCPU at once, each working on what it owns):
 *   1. guest FILL(1) over scattered 4 KiB pages; negative control.
 *   2. migrate every page into one contiguous buffer B, in list order.
 *      guest CHECK(1): the copy is what the guest sees. guest FILL(2): B holds
 *      salt 2 and the OLD backing does not — the guest really moved.
 *   3. Metal no-copy buffer on B: GPU CHECK(2); GPU WRITE(3) -> guest CHECK(3);
 *      guest FILL(4) -> GPU CHECK(4).
 *   4. migrate back to the original backing (host addresses 4 KiB- but not
 *      16 KiB-aligned); guest CHECK(4), FILL(5), host reads the original.
 *   5. race (with --sabotage, only in the race: the copy is taken before the
 *      unmap, so the checkers must catch lost stores): every vCPU runs --rounds rounds of FILL(s+i)/CHECK(s+i) without
 *      exiting while a host thread ping-pongs every page between the original
 *      backing and B as fast as it can. A vCPU that touches a page mid-move
 *      takes a stage-2 fault, waits for the move and retries.
 *
 * Ownership (--mode, with --vcpus N):
 *   disjoint     each vCPU owns whole pages: its own share of the page list.
 *   interleaved  every vCPU shares every page and owns every N-th 16-byte unit
 *                of it, so each page is live in every vCPU's TLB at once and a
 *                move must invalidate all of them — a vCPU HVF missed would
 *                store into the old page and its next check would catch it.
 *
 * Options: --vcpus N (default 1), --mode disjoint|interleaved (default
 * disjoint), --pages N (default 256, rounded up to a multiple of 4 * vcpus),
 * --rounds R (default 2000), --seed S, --sabotage (positive control).
 *
 * Build/run/sign: build.sh (needs com.apple.security.hypervisor). Sandbox off.
 */

#include <Hypervisor/Hypervisor.h>
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include <dlfcn.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#define RAM_BASE 0x80000000ULL
#define RAM_SIZE (256ULL << 20)
#define POOL_GPA (RAM_BASE + (1ULL << 20))
#define PG 0x1000ULL
#define WORDS (PG / 8)
#define UNITS (PG / 16)

/* Per-vCPU control block + page list, below the page pool. */
#define CTL_GPA(k) (RAM_BASE + 0x10000 + (uint64_t)(k) * 0x20000)
#define LIST_OFF 0x1000
#define MAX_LIST ((0x20000 - LIST_OFF) / 8)
#define MAX_VCPUS 4

#define MMIO_BASE 0x10000000ULL
#define M_READY 0x00
#define M_REPORT 0x08
#define M_ROUNDS 0x10
#define M_DONE 0x20

#define CMD_FILL 1
#define CMD_CHECK 2
#define CMD_RACE 4
#define CMD_DONE 3

#define BOOT_CPSR 0x3C5ULL /* EL1h, DAIF masked */
#define RWX (HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC)

#define CHECK(expr)                                                                   \
    do {                                                                              \
        hv_return_t _r = (expr);                                                      \
        if (_r != HV_SUCCESS) {                                                       \
            fprintf(stderr, "FATAL %s:%d %s -> 0x%x\n", __FILE__, __LINE__, #expr,    \
                    (uint32_t)_r);                                                    \
            exit(1);                                                                  \
        }                                                                             \
    } while (0)

static uint8_t *g_ram;
static void *gpa_to_hva(uint64_t gpa) { return g_ram + (gpa - RAM_BASE); }
static uint64_t now_ns(void) { return clock_gettime_nsec_np(CLOCK_UPTIME_RAW); }
static int g_failures;

static void expect(bool ok, const char *what) {
    printf("  %s %s\n", ok ? "PASS" : "FAIL", what);
    if (!ok) g_failures++;
}

static uint64_t g_npages;
static uint64_t *g_list;   /* every guest page, in B's order */
static uint8_t **g_where;  /* the host memory currently behind each page */
static uint8_t **g_orig;   /* the host memory behind each page at boot */
static uint8_t *g_buf;     /* the contiguous buffer pages migrate into */

/* ---- VM map accounting ------------------------------------------------------ */

static unsigned count_regions(void) {
    unsigned n = 0;
    mach_vm_address_t addr = 0;
    for (;;) {
        mach_vm_size_t size = 0;
        vm_region_basic_info_data_64_t info;
        mach_msg_type_number_t cnt = VM_REGION_BASIC_INFO_COUNT_64;
        mach_port_t obj = MACH_PORT_NULL;
        if (mach_vm_region(mach_task_self(), &addr, &size, VM_REGION_BASIC_INFO_64,
                           (vm_region_info_t)&info, &cnt, &obj) != KERN_SUCCESS)
            break;
        n++;
        addr += size;
    }
    return n;
}

/* ---- migration ---------------------------------------------------------------- */

static pthread_mutex_t g_mig = PTHREAD_MUTEX_INITIALIZER;
static _Atomic uint64_t g_moves, g_move_ns;
static bool g_sabotage; /* positive control: copy BEFORE the unmap, the bug the race must catch */

/* Move guest page i onto `dst`: nothing can store to it between the unmap and the map, so the
 * copy is the page's final content. */
static void migrate(uint64_t i, uint8_t *dst) {
    pthread_mutex_lock(&g_mig);
    uint64_t t0 = now_ns();
    if (g_sabotage) {
        memcpy(dst, g_where[i], PG); /* a store between here and the unmap is lost */
        CHECK(hv_vm_unmap(g_list[i], PG));
    } else {
        CHECK(hv_vm_unmap(g_list[i], PG));
        memcpy(dst, g_where[i], PG);
    }
    CHECK(hv_vm_map(dst, g_list[i], PG, RWX));
    g_where[i] = dst;
    atomic_fetch_add(&g_move_ns, now_ns() - t0);
    atomic_fetch_add(&g_moves, 1);
    pthread_mutex_unlock(&g_mig);
}

/* ---- vCPUs: one host thread each, driven by posted commands ------------------------ */

struct vcpu {
    int k;
    pthread_t th;
    uint64_t ctl_gpa;
    uint64_t npages, start, step; /* what this vCPU owns */
    pthread_mutex_t m;
    pthread_cond_t c;
    int posted;  /* 1: a command is waiting; 0: the last one finished */
    bool alive;
    uint64_t report, rounds, heals, max_spins;
};

static struct vcpu g_v[MAX_VCPUS];
static int g_nvcpus = 1;

static bool run_to_ready(struct vcpu *v, hv_vcpu_t vcpu, hv_vcpu_exit_t *ex) {
    uint64_t spins = 0, last_pa = 0;
    for (;;) {
        CHECK(hv_vcpu_run(vcpu));
        if (ex->reason != HV_EXIT_REASON_EXCEPTION) {
            fprintf(stderr, "vCPU %d: unexpected exit reason %u\n", v->k, ex->reason);
            exit(1);
        }
        uint64_t syn = ex->exception.syndrome, pa = ex->exception.physical_address;
        uint64_t pc = 0;
        CHECK(hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc));
        uint64_t ec = (syn >> 26) & 0x3f;
        if (ec == 0x24 && pa >= RAM_BASE && pa < RAM_BASE + RAM_SIZE) {
            /* A stage-2 fault on a page being moved: wait for the move, then retry the store. */
            pthread_mutex_lock(&g_mig);
            pthread_mutex_unlock(&g_mig);
            v->heals++;
            spins = (pa & ~(PG - 1)) == last_pa ? spins + 1 : 1;
            last_pa = pa & ~(PG - 1);
            if (spins > v->max_spins) v->max_spins = spins;
            if (spins > 10000000) {
                fprintf(stderr, "vCPU %d stuck faulting on pa=0x%llx pc=0x%llx\n", v->k, pa, pc);
                exit(1);
            }
            continue;
        }
        if (ec != 0x24 || pa < MMIO_BASE || pa >= MMIO_BASE + 0x1000) {
            fprintf(stderr, "vCPU %d: unhandled exception syndrome=0x%llx pc=0x%llx pa=0x%llx\n",
                    v->k, syn, pc, pa);
            exit(1);
        }
        spins = 0;
        CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4));
        uint32_t srt = (syn >> 16) & 0x1f;
        uint64_t val = 0;
        if (srt < 31) CHECK(hv_vcpu_get_reg(vcpu, HV_REG_X0 + srt, &val));
        switch (pa - MMIO_BASE) {
        case M_READY: return true;
        case M_REPORT: v->report = val; break;
        case M_ROUNDS: v->rounds = val; break;
        case M_DONE: return false;
        default: fprintf(stderr, "vCPU %d: unexpected MMIO write 0x%llx\n", v->k, pa); exit(1);
        }
    }
}

static void *vcpu_main(void *arg) {
    struct vcpu *v = arg;
    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *ex;
    CHECK(hv_vcpu_create(&vcpu, &ex, NULL));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, BOOT_CPSR));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_X0, v->ctl_gpa));
    bool alive = run_to_ready(v, vcpu, ex);
    for (;;) {
        pthread_mutex_lock(&v->m);
        v->alive = alive;
        v->posted = 0;
        pthread_cond_broadcast(&v->c);
        if (!alive) { pthread_mutex_unlock(&v->m); break; }
        while (!v->posted) pthread_cond_wait(&v->c, &v->m);
        pthread_mutex_unlock(&v->m);
        v->report = UINT64_MAX;
        alive = run_to_ready(v, vcpu, ex);
    }
    CHECK(hv_vcpu_destroy(vcpu));
    return NULL;
}

static void wait_idle(struct vcpu *v) {
    pthread_mutex_lock(&v->m);
    while (v->posted) pthread_cond_wait(&v->c, &v->m);
    pthread_mutex_unlock(&v->m);
}

/* Send one command to every vCPU at once and wait for all of them; returns the sum of reports. */
static uint64_t guest_cmd(uint64_t cmd, uint64_t salt, uint64_t iters) {
    for (int k = 0; k < g_nvcpus; k++) {
        struct vcpu *v = &g_v[k];
        wait_idle(v);
        volatile uint64_t *ctl = gpa_to_hva(v->ctl_gpa);
        ctl[0] = cmd;
        ctl[1] = salt;
        ctl[2] = v->npages;
        ctl[3] = PG;
        ctl[4] = iters;
        ctl[5] = v->start;
        ctl[6] = v->step;
        pthread_mutex_lock(&v->m);
        v->posted = 1;
        pthread_cond_broadcast(&v->c);
        pthread_mutex_unlock(&v->m);
    }
    uint64_t sum = 0;
    for (int k = 0; k < g_nvcpus; k++) {
        wait_idle(&g_v[k]);
        sum += g_v[k].report;
    }
    return sum;
}

/* ---- host checkers ------------------------------------------------------------- */

static uint64_t expected(uint64_t gpa, uint64_t salt) { return gpa ^ (salt << 48); }

static uint64_t count_page(const uint8_t *at, uint64_t i, uint64_t salt) {
    const volatile uint64_t *p = (const volatile uint64_t *)at;
    uint64_t m = 0;
    for (uint64_t w = 0; w < WORDS; w++)
        if (p[w] != expected(g_list[i] + w * 8, salt)) m++;
    return m;
}

static uint64_t count_where(uint8_t **backing, uint64_t salt) {
    uint64_t m = 0;
    for (uint64_t i = 0; i < g_npages; i++) m += count_page(backing[i], i, salt);
    return m;
}

static uint64_t count_buf(uint64_t salt) {
    uint64_t m = 0;
    for (uint64_t i = 0; i < g_npages; i++) m += count_page(g_buf + i * PG, i, salt);
    return m;
}

/* ---- Metal --------------------------------------------------------------------- */

static NSString *const kShader =
    @"#include <metal_stdlib>\n"
     "using namespace metal;\n"
     "kernel void pattern(device ulong *buf [[buffer(0)]],\n"
     "                    device const ulong *gpas [[buffer(1)]],\n"
     "                    constant ulong &salt [[buffer(2)]],\n"
     "                    device atomic_uint *mism [[buffer(3)]],\n"
     "                    constant uint &write [[buffer(4)]],\n"
     "                    uint gid [[thread_position_in_grid]]) {\n"
     "    ulong addr = gpas[gid / 512] + ulong(gid % 512) * 8;\n"
     "    ulong want = addr ^ (salt << 48);\n"
     "    if (write) buf[gid] = want;\n"
     "    else if (buf[gid] != want) atomic_fetch_add_explicit(mism, 1, memory_order_relaxed);\n"
     "}\n";

static id<MTLDevice> g_dev;
static id<MTLCommandQueue> g_queue;
static id<MTLComputePipelineState> g_pipe;
static id<MTLBuffer> g_gpas, g_mism;

static uint64_t gpu_pass(id<MTLBuffer> buf, uint64_t salt, uint32_t write) {
    *(uint32_t *)g_mism.contents = 0;
    id<MTLCommandBuffer> cb = [g_queue commandBuffer];
    id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
    [enc setComputePipelineState:g_pipe];
    [enc setBuffer:buf offset:0 atIndex:0];
    [enc setBuffer:g_gpas offset:0 atIndex:1];
    [enc setBytes:&salt length:sizeof(salt) atIndex:2];
    [enc setBuffer:g_mism offset:0 atIndex:3];
    [enc setBytes:&write length:sizeof(write) atIndex:4];
    [enc dispatchThreads:MTLSizeMake(g_npages * WORDS, 1, 1) threadsPerThreadgroup:MTLSizeMake(256, 1, 1)];
    [enc endEncoding];
    [cb commit];
    [cb waitUntilCompleted];
    if (cb.error) { fprintf(stderr, "command buffer: %s\n", cb.error.description.UTF8String); exit(1); }
    return *(uint32_t *)g_mism.contents;
}

static void metal_init(void) {
    g_dev = MTLCreateSystemDefaultDevice();
    g_queue = [g_dev newCommandQueue];
    NSError *err = nil;
    id<MTLLibrary> lib = [g_dev newLibraryWithSource:kShader options:nil error:&err];
    if (!lib) { fprintf(stderr, "shader: %s\n", err.description.UTF8String); exit(1); }
    g_pipe = [g_dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"pattern"] error:&err];
    if (!g_pipe) { fprintf(stderr, "pipeline: %s\n", err.description.UTF8String); exit(1); }
    g_gpas = [g_dev newBufferWithBytes:g_list length:g_npages * 8 options:MTLResourceStorageModeShared];
    g_mism = [g_dev newBufferWithLength:4 options:MTLResourceStorageModeShared];
}

/* ---- the race ----------------------------------------------------------------------- */

static _Atomic bool g_race_on;
static uint64_t rng_state = 0x9e3779b97f4a7c15ULL;
static uint64_t rng(void) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 7;
    rng_state ^= rng_state << 17;
    return rng_state;
}

/* Ping-pong every page between its original backing and B, in a random order each pass. */
static void *migrator(void *arg) {
    (void)arg;
    uint64_t *order = malloc(g_npages * 8);
    uint64_t r = 0x2545F4914F6CDD1DULL;
    while (atomic_load(&g_race_on)) {
        for (uint64_t i = 0; i < g_npages; i++) order[i] = i;
        for (uint64_t i = g_npages - 1; i > 0; i--) {
            r ^= r << 13; r ^= r >> 7; r ^= r << 17;
            uint64_t j = r % (i + 1), t = order[i];
            order[i] = order[j]; order[j] = t;
        }
        for (uint64_t k = 0; k < g_npages && atomic_load(&g_race_on); k++) {
            uint64_t i = order[k];
            migrate(i, g_where[i] == g_orig[i] ? g_buf + i * PG : g_orig[i]);
        }
    }
    free(order);
    return NULL;
}

/* ---- main ----------------------------------------------------------------------------- */

int main(int argc, char **argv) {
    @autoreleasepool {
        const char *payload_path = "payload.bin";
        uint64_t rounds = 2000;
        bool interleaved = false, sabotage = false;
        g_npages = 256;
        for (int i = 1; i < argc; i++) {
            if (!strcmp(argv[i], "--pages") && i + 1 < argc) g_npages = strtoull(argv[++i], 0, 0);
            else if (!strcmp(argv[i], "--rounds") && i + 1 < argc) rounds = strtoull(argv[++i], 0, 0);
            else if (!strcmp(argv[i], "--seed") && i + 1 < argc) rng_state = strtoull(argv[++i], 0, 0) | 1;
            else if (!strcmp(argv[i], "--vcpus") && i + 1 < argc) g_nvcpus = atoi(argv[++i]);
            else if (!strcmp(argv[i], "--mode") && i + 1 < argc) interleaved = !strcmp(argv[++i], "interleaved");
            else if (!strcmp(argv[i], "--sabotage")) sabotage = true;
            else payload_path = argv[i];
        }
        if (g_nvcpus < 1 || g_nvcpus > MAX_VCPUS) { fprintf(stderr, "bad --vcpus\n"); return 1; }
        uint64_t quantum = 4 * (uint64_t)g_nvcpus;
        g_npages = (g_npages + quantum - 1) / quantum * quantum;
        uint64_t per_list = interleaved ? g_npages : g_npages / g_nvcpus;
        if (per_list > MAX_LIST) { fprintf(stderr, "bad --pages\n"); return 1; }

        FILE *f = fopen(payload_path, "rb");
        if (!f) { perror(payload_path); return 1; }
        static uint8_t payload[0x1000];
        size_t payload_len = fread(payload, 1, sizeof(payload), f);
        fclose(f);

        typedef hv_vm_config_t (*create_fn)(void);
        typedef hv_return_t (*granule_fn)(hv_vm_config_t, uint32_t);
        create_fn create = (create_fn)dlsym(RTLD_DEFAULT, "hv_vm_config_create");
        granule_fn set = (granule_fn)dlsym(RTLD_DEFAULT, "hv_vm_config_set_ipa_granule");
        if (!create || !set) { fprintf(stderr, "no IPA granule API (needs macOS 26)\n"); return 1; }
        hv_vm_config_t cfg = create();
        CHECK(set(cfg, 0 /* HV_IPA_GRANULE_4KB */));
        CHECK(hv_vm_create(cfg));

        /* Guest RAM as vm-memory allocates it, mapped whole. */
        g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
        if (g_ram == MAP_FAILED) { perror("mmap"); return 1; }
        memcpy(g_ram, payload, payload_len);
        CHECK(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE, RWX));

        /* Scattered 4 KiB guest pages, never two in a row. */
        g_list = calloc(g_npages, sizeof(*g_list));
        g_where = calloc(g_npages, sizeof(*g_where));
        g_orig = calloc(g_npages, sizeof(*g_orig));
        uint64_t pool = (RAM_BASE + RAM_SIZE - POOL_GPA) / PG;
        uint8_t *used = calloc(pool + 1, 1);
        uint64_t unaligned = 0;
        for (uint64_t n = 0; n < g_npages;) {
            uint64_t p = rng() % pool;
            if (used[p] || (p && used[p - 1]) || used[p + 1]) continue;
            used[p] = 1;
            g_list[n] = POOL_GPA + p * PG;
            g_where[n] = g_orig[n] = gpa_to_hva(g_list[n]);
            unaligned += g_list[n] % 0x4000 != 0;
            n++;
        }
        free(used);

        /* Hand each vCPU what it owns. */
        for (int k = 0; k < g_nvcpus; k++) {
            struct vcpu *v = &g_v[k];
            v->k = k;
            v->ctl_gpa = CTL_GPA(k);
            pthread_mutex_init(&v->m, NULL);
            pthread_cond_init(&v->c, NULL);
            v->posted = 1;
            uint64_t *list = gpa_to_hva(v->ctl_gpa + LIST_OFF);
            if (interleaved) {
                v->npages = g_npages;
                v->start = 16 * (uint64_t)k;
                v->step = 16 * (uint64_t)g_nvcpus;
                memcpy(list, g_list, g_npages * 8);
            } else {
                v->npages = per_list;
                v->start = 0;
                v->step = 16;
                memcpy(list, g_list + k * per_list, per_list * 8);
            }
        }

        mach_vm_address_t b = 0;
        if (mach_vm_allocate(mach_task_self(), &b, g_npages * PG, VM_FLAGS_ANYWHERE)) return 1;
        g_buf = (uint8_t *)b;
        printf("config: 4 KiB IPA granule, %d vCPU(s) %s, %llu scattered 4 KiB guest pages (%llu not "
               "16 KiB-aligned), buffer B at 0x%llx (%llu KiB), rounds=%llu\n",
               g_nvcpus, interleaved ? "interleaved (every vCPU on every page)" : "on disjoint pages",
               g_npages, unaligned, b, g_npages * PG / 1024, rounds);

        for (int k = 0; k < g_nvcpus; k++) pthread_create(&g_v[k].th, NULL, vcpu_main, &g_v[k]);
        for (int k = 0; k < g_nvcpus; k++) {
            wait_idle(&g_v[k]);
            if (!g_v[k].alive) { fprintf(stderr, "vCPU %d never became ready\n", k); return 1; }
        }
        metal_init();
        const uint64_t all = g_npages * WORDS;

        printf("\n== 1. guest FILL(1) on the original backing ==\n");
        guest_cmd(CMD_FILL, 1, 0);
        expect(guest_cmd(CMD_CHECK, 1, 0) == 0, "guest CHECK(1)");
        expect(guest_cmd(CMD_CHECK, 9, 0) == all, "negative control: guest CHECK(9) misses every word");
        expect(count_where(g_orig, 1) == 0, "host reads salt 1 at the original backing");

        printf("\n== 2. migrate every page into B ==\n");
        unsigned regions_before = count_regions();
        uint64_t t0 = now_ns();
        for (uint64_t i = 0; i < g_npages; i++) migrate(i, g_buf + i * PG);
        uint64_t t_mig = now_ns() - t0;
        printf("  %llu pages in %.3f ms (%.2f us/page, cold, lock included); host VM regions %u -> %u\n",
               g_npages, t_mig / 1e6, t_mig / 1e3 / g_npages, regions_before, count_regions());
        expect(guest_cmd(CMD_CHECK, 1, 0) == 0, "guest CHECK(1): the guest sees the copy");
        guest_cmd(CMD_FILL, 2, 0);
        expect(count_buf(2) == 0, "guest FILL(2) lands in B");
        expect(count_where(g_orig, 1) == 0, "the original backing still holds salt 1: the guest really moved");

        printf("\n== 3. Metal no-copy buffer on B ==\n");
        id<MTLBuffer> buf = [g_dev newBufferWithBytesNoCopy:g_buf length:g_npages * PG
                                                    options:MTLResourceStorageModeShared
                                                deallocator:nil];
        expect(buf != nil, "newBufferWithBytesNoCopy(B) returns a buffer");
        if (buf) {
            expect(gpu_pass(buf, 2, 0) == 0, "GPU CHECK(2): guest -> GPU");
            expect(gpu_pass(buf, 9, 0) == all, "negative control: GPU CHECK(9) misses every word");
            gpu_pass(buf, 3, 1);
            expect(guest_cmd(CMD_CHECK, 3, 0) == 0, "guest CHECK(3): GPU -> guest");
            guest_cmd(CMD_FILL, 4, 0);
            expect(gpu_pass(buf, 4, 0) == 0, "GPU CHECK(4), same buffer: guest -> GPU after wiring");
            buf = nil;
        }

        printf("\n== 4. migrate back to the original backing (4 KiB-aligned host addresses) ==\n");
        for (uint64_t i = 0; i < g_npages; i++) migrate(i, g_orig[i]);
        expect(guest_cmd(CMD_CHECK, 4, 0) == 0, "guest CHECK(4) after moving back");
        guest_cmd(CMD_FILL, 5, 0);
        expect(count_where(g_orig, 5) == 0, "guest FILL(5) lands in the original backing");
        expect(count_buf(4) == 0, "B still holds salt 4: the guest left it");

        printf("\n== 5. race: every vCPU fills and checks while every page ping-pongs ==\n");
        atomic_store(&g_moves, 0);
        atomic_store(&g_move_ns, 0);
        for (int k = 0; k < g_nvcpus; k++) g_v[k].heals = g_v[k].max_spins = g_v[k].rounds = 0;
        g_sabotage = sabotage;
        if (sabotage) printf("  (sabotaged: every move copies before it unmaps)\n");
        atomic_store(&g_race_on, true);
        pthread_t th;
        pthread_create(&th, NULL, migrator, NULL);
        t0 = now_ns();
        uint64_t race_mism = guest_cmd(CMD_RACE, 100, rounds);
        uint64_t race_ns = now_ns() - t0;
        atomic_store(&g_race_on, false);
        pthread_join(th, NULL);
        uint64_t moves = atomic_load(&g_moves), heals = 0, max_spins = 0;
        bool all_rounds = true;
        for (int k = 0; k < g_nvcpus; k++) {
            struct vcpu *v = &g_v[k];
            printf("  vCPU %d: %llu rounds, %llu stage-2 faults healed (max %llu in a row on one page), "
                   "%llu stale words\n", k, v->rounds, v->heals, v->max_spins, v->report);
            heals += v->heals;
            if (v->max_spins > max_spins) max_spins = v->max_spins;
            all_rounds &= v->rounds == rounds;
        }
        printf("  race took %.1f ms; %llu page moves (%.2f us each, %.1f moves per page); "
               "%llu faults healed in all\n",
               race_ns / 1e6, moves, moves ? atomic_load(&g_move_ns) / 1e3 / moves : 0.0,
               (double)moves / g_npages, heals);
        printf("  host VM regions after the race: %u\n", count_regions());
        expect(all_rounds, "every vCPU completed every round");
        expect(moves >= 2 * g_npages, "every page moved at least twice on average during the race");
        bool every_vcpu_healed = true;
        for (int k = 0; k < g_nvcpus; k++) every_vcpu_healed &= g_v[k].heals > 0;
        expect(every_vcpu_healed, "every vCPU touched pages mid-move");
        expect(race_mism == 0, "no guest check on any vCPU saw a lost or stale word");
        expect(count_where(g_where, 100 + rounds - 1) == 0,
               "host reads the last round's salt at each page's final backing");
        expect(guest_cmd(CMD_CHECK, 100 + rounds - 1, 0) == 0, "the guest agrees after the race");

        guest_cmd(CMD_DONE, 0, 0);
        for (int k = 0; k < g_nvcpus; k++) pthread_join(g_v[k].th, NULL);
        CHECK(hv_vm_destroy());
        printf("\nRESULT: %s (%d failure(s))\n", g_failures ? "FAIL" : "PASS", g_failures);
        return g_failures ? 1 : 0;
    }
}
