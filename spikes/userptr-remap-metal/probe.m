// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * userptr-remap-metal host driver.
 *
 * Question: can the worker stitch scattered 16 KiB pages of guest RAM into one
 * contiguous host range (mach_vm_remap, copy=FALSE) and hand that range to
 * Metal as newBufferWithBytesNoCopy — and does the result stay ONE set of
 * pages, coherent in both directions, while the guest keeps running on them?
 * That is the host half a venus VK_EXT_external_memory_host (guest userptr
 * blob -> scattered guest pages -> one host pointer -> KosmicKrisp import)
 * would stand on.
 *
 * Sequence (see payload.S for the guest's command loop):
 *   guest FILL(1) -> remap -> CPU reads alias      (guest -> alias)
 *   Metal buffer on the alias, GPU CHECK(1)        (guest -> GPU at creation)
 *   GPU WRITE(2) -> guest CHECK(2)                 (GPU -> guest through stage-2)
 *   guest FILL(3) -> GPU CHECK(3), same MTLBuffer  (guest -> GPU after wiring)
 *   ... repeated --rounds times, then teardown and a last guest FILL/CHECK.
 * Negative controls: CHECK with a salt nobody wrote must report every word, on
 * the guest and on the GPU, or a broken checker reads as a pass.
 *
 * Options:
 *   --pages N         pages in the userptr (default 256 = 4 MiB)
 *   --run K           pick the pages in guest-contiguous runs of K (default 1: all scattered)
 *   --untouched       remap before the guest has ever touched the pages
 *   --shared          back guest RAM with MAP_SHARED instead of MAP_PRIVATE (vm-memory's default)
 *   --rounds R        GPU-write / guest-fill rounds (default 4)
 *   --seed S          page-shuffle seed
 *
 * Build/run/sign: build.sh (needs com.apple.security.hypervisor). Sandbox off.
 */

#include <Hypervisor/Hypervisor.h>
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>

#define RAM_BASE 0x80000000ULL
#define RAM_SIZE (256ULL << 20)
#define POOL_GPA (RAM_BASE + (1ULL << 20))
#define PAGE 0x4000ULL
#define WORDS_PER_PAGE (PAGE / 8)

#define CTL_GPA (RAM_BASE + 0x3000)
#define LIST_GPA (RAM_BASE + 0x4000)
#define MAX_PAGES ((0x100000 - 0x4000) / 8)

#define MMIO_BASE 0x10000000ULL
#define M_READY 0x00
#define M_REPORT 0x08
#define M_DONE 0x20

#define CMD_FILL 1
#define CMD_CHECK 2
#define CMD_DONE 3

#define BOOT_CPSR 0x3C5ULL /* EL1h, DAIF masked */

#define CHECK(expr)                                                                   \
    do {                                                                              \
        hv_return_t _r = (expr);                                                      \
        if (_r != HV_SUCCESS) {                                                       \
            fprintf(stderr, "FATAL %s:%d %s -> 0x%x\n", __FILE__, __LINE__, #expr,    \
                    (uint32_t)_r);                                                    \
            exit(1);                                                                  \
        }                                                                             \
    } while (0)

#define KCHECK(expr)                                                                  \
    do {                                                                              \
        kern_return_t _k = (expr);                                                    \
        if (_k != KERN_SUCCESS) {                                                     \
            fprintf(stderr, "FATAL %s:%d %s -> %d (%s)\n", __FILE__, __LINE__, #expr, \
                    _k, mach_error_string(_k));                                       \
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

/* ---- guest ---------------------------------------------------------------- */

static hv_vcpu_t g_vcpu;
static hv_vcpu_exit_t *g_exit;
static uint64_t g_report;

/* Run the vCPU until it next waits for a command (READY) or says DONE. */
static bool run_to_ready(void) {
    for (;;) {
        CHECK(hv_vcpu_run(g_vcpu));
        if (g_exit->reason != HV_EXIT_REASON_EXCEPTION) {
            fprintf(stderr, "unexpected exit reason %u\n", g_exit->reason);
            exit(1);
        }
        uint64_t syn = g_exit->exception.syndrome, pa = g_exit->exception.physical_address;
        uint64_t pc = 0;
        CHECK(hv_vcpu_get_reg(g_vcpu, HV_REG_PC, &pc));
        if (((syn >> 26) & 0x3f) != 0x24 || pa < MMIO_BASE || pa >= MMIO_BASE + 0x1000) {
            fprintf(stderr, "unhandled exception syndrome=0x%llx pc=0x%llx pa=0x%llx\n", syn, pc, pa);
            exit(1);
        }
        CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_PC, pc + 4));
        uint32_t srt = (syn >> 16) & 0x1f;
        uint64_t val = 0;
        if (srt < 31) CHECK(hv_vcpu_get_reg(g_vcpu, HV_REG_X0 + srt, &val));
        switch (pa - MMIO_BASE) {
        case M_READY: return true;
        case M_REPORT: g_report = val; break;
        case M_DONE: return false;
        default: fprintf(stderr, "unexpected MMIO write 0x%llx\n", pa); exit(1);
        }
    }
}

static uint64_t g_npages;
static uint64_t *g_list; /* guest-physical page addresses, in alias order */

static uint64_t guest_cmd(uint64_t cmd, uint64_t salt) {
    volatile uint64_t *ctl = gpa_to_hva(CTL_GPA);
    ctl[0] = cmd;
    ctl[1] = salt;
    ctl[2] = g_npages;
    g_report = UINT64_MAX;
    run_to_ready();
    return g_report;
}

/* ---- host CPU checkers ------------------------------------------------------ */

static uint64_t expected(uint64_t gpa, uint64_t salt) { return gpa ^ (salt << 48); }

/* Words of the alias that do not hold `salt`'s pattern for the page the list says is there. */
static uint64_t cpu_check_alias(const volatile uint64_t *alias, uint64_t salt) {
    uint64_t mism = 0;
    for (uint64_t i = 0; i < g_npages; i++)
        for (uint64_t w = 0; w < WORDS_PER_PAGE; w++)
            if (alias[i * WORDS_PER_PAGE + w] != expected(g_list[i] + w * 8, salt)) mism++;
    return mism;
}

/* The same, read through the VMM's own mapping of guest RAM. */
static uint64_t cpu_check_ram(uint64_t salt) {
    uint64_t mism = 0;
    for (uint64_t i = 0; i < g_npages; i++) {
        const volatile uint64_t *p = gpa_to_hva(g_list[i]);
        for (uint64_t w = 0; w < WORDS_PER_PAGE; w++)
            if (p[w] != expected(g_list[i] + w * 8, salt)) mism++;
    }
    return mism;
}

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

/* ---- the remap -------------------------------------------------------------- */

/* One contiguous host range whose i-th 16 KiB page IS guest page g_list[i]. */
static mach_vm_address_t make_alias(void) {
    mach_vm_address_t alias = 0;
    KCHECK(mach_vm_allocate(mach_task_self(), &alias, g_npages * PAGE, VM_FLAGS_ANYWHERE));
    for (uint64_t i = 0; i < g_npages;) {
        /* Coalesce pages that are adjacent in both guest and alias order into one remap. */
        uint64_t run = 1;
        while (i + run < g_npages && g_list[i + run] == g_list[i] + run * PAGE) run++;
        mach_vm_address_t dst = alias + i * PAGE;
        vm_prot_t cur, max;
        KCHECK(mach_vm_remap(mach_task_self(), &dst, run * PAGE, 0,
                             VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE, mach_task_self(),
                             (mach_vm_address_t)gpa_to_hva(g_list[i]), FALSE, &cur, &max,
                             VM_INHERIT_NONE));
        if (dst != alias + i * PAGE) {
            fprintf(stderr, "remap landed at 0x%llx, wanted 0x%llx\n", dst, alias + i * PAGE);
            exit(1);
        }
        i += run;
    }
    return alias;
}

/* ---- Metal ------------------------------------------------------------------ */

static NSString *const kShader =
    @"#include <metal_stdlib>\n"
     "using namespace metal;\n"
     "kernel void pattern(device ulong *buf [[buffer(0)]],\n"
     "                    device const ulong *gpas [[buffer(1)]],\n"
     "                    constant ulong &salt [[buffer(2)]],\n"
     "                    device atomic_uint *mism [[buffer(3)]],\n"
     "                    constant uint &write [[buffer(4)]],\n"
     "                    uint gid [[thread_position_in_grid]]) {\n"
     "    ulong addr = gpas[gid / 2048] + ulong(gid % 2048) * 8;\n"
     "    ulong want = addr ^ (salt << 48);\n"
     "    if (write) buf[gid] = want;\n"
     "    else if (buf[gid] != want) atomic_fetch_add_explicit(mism, 1, memory_order_relaxed);\n"
     "}\n"
     "kernel void copy(device const ulong *src [[buffer(0)]], device ulong *dst [[buffer(1)]],\n"
     "                 uint gid [[thread_position_in_grid]]) { dst[gid] = src[gid]; }\n";

static id<MTLDevice> g_dev;
static id<MTLCommandQueue> g_queue;
static id<MTLComputePipelineState> g_pipe, g_copy;
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
    NSUInteger tg = MIN((NSUInteger)256, g_pipe.maxTotalThreadsPerThreadgroup);
    [enc dispatchThreads:MTLSizeMake(g_npages * WORDS_PER_PAGE, 1, 1)
        threadsPerThreadgroup:MTLSizeMake(tg, 1, 1)];
    [enc endEncoding];
    [cb commit];
    [cb waitUntilCompleted];
    if (cb.error) {
        fprintf(stderr, "command buffer error: %s\n", cb.error.description.UTF8String);
        exit(1);
    }
    return *(uint32_t *)g_mism.contents;
}

static void metal_init(void) {
    g_dev = MTLCreateSystemDefaultDevice();
    g_queue = [g_dev newCommandQueue];
    NSError *err = nil;
    id<MTLLibrary> lib = [g_dev newLibraryWithSource:kShader options:nil error:&err];
    if (!lib) { fprintf(stderr, "shader: %s\n", err.description.UTF8String); exit(1); }
    g_pipe = [g_dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"pattern"]
                                                  error:&err];
    if (!g_pipe) { fprintf(stderr, "pipeline: %s\n", err.description.UTF8String); exit(1); }
    g_copy = [g_dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"copy"] error:&err];
    if (!g_copy) { fprintf(stderr, "pipeline: %s\n", err.description.UTF8String); exit(1); }
    g_gpas = [g_dev newBufferWithBytes:g_list length:g_npages * 8
                               options:MTLResourceStorageModeShared];
    g_mism = [g_dev newBufferWithLength:4 options:MTLResourceStorageModeShared];
    printf("metal: %s\n", g_dev.name.UTF8String);
}

/* ---- the 4 KiB questions ------------------------------------------------------ */

/* Copy `words` words out of `buf` on the GPU into a fresh buffer. */
static id<MTLBuffer> gpu_copy(id<MTLBuffer> buf, uint64_t words) {
    id<MTLBuffer> out = [g_dev newBufferWithLength:words * 8 options:MTLResourceStorageModeShared];
    id<MTLCommandBuffer> cb = [g_queue commandBuffer];
    id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
    [enc setComputePipelineState:g_copy];
    [enc setBuffer:buf offset:0 atIndex:0];
    [enc setBuffer:out offset:0 atIndex:1];
    [enc dispatchThreads:MTLSizeMake(words, 1, 1) threadsPerThreadgroup:MTLSizeMake(64, 1, 1)];
    [enc endEncoding];
    [cb commit];
    [cb waitUntilCompleted];
    if (cb.error) { printf("    command buffer error: %s\n", cb.error.description.UTF8String); return nil; }
    return out;
}

/* Words of a GPU copy of `buf` that do not match what guest-physical address `gpa0 + 8*w` should
 * hold (crossing into g_list[1] past the first page's end). */
static uint64_t gpu_readback(id<MTLBuffer> buf, uint64_t words, uint64_t gpa0, uint64_t salt) {
    id<MTLBuffer> out = gpu_copy(buf, words);
    if (!out) return words;
    const uint64_t *o = out.contents;
    uint64_t page_end = (gpa0 & ~(PAGE - 1)) + PAGE, mism = 0;
    for (uint64_t w = 0; w < words; w++) {
        uint64_t gpa = gpa0 + w * 8;
        if (gpa >= page_end) gpa = g_list[1] + (gpa - page_end);
        if (o[w] != expected(gpa, salt)) mism++;
    }
    return mism;
}

/* The same two sub-page shapes on freshly allocated memory no MTLBuffer has ever covered, so an
 * acceptance cannot come from a page some other buffer already registered. */
static void four_k_fresh(void) {
    struct { uint64_t off, len; const char *name; } shapes[] = {
        {4096, PAGE, "fresh + 4096, 16 KiB"},
        {0, 4096, "fresh, 4 KiB"},
    };
    for (int i = 0; i < 2; i++) {
        @autoreleasepool {
            mach_vm_address_t fresh = 0;
            KCHECK(mach_vm_allocate(mach_task_self(), &fresh, 2 * PAGE, VM_FLAGS_ANYWHERE));
            uint64_t *p = (uint64_t *)fresh;
            for (uint64_t w = 0; w < 2 * PAGE / 8; w++) p[w] = (0xF00DULL << 48) | (w * 8);
            id<MTLBuffer> b = [g_dev newBufferWithBytesNoCopy:(void *)(fresh + shapes[i].off)
                                                       length:shapes[i].len
                                                      options:MTLResourceStorageModeShared
                                                  deallocator:nil];
            printf("  newBufferWithBytesNoCopy(%s) -> %s", shapes[i].name, b ? "a buffer" : "nil");
            if (b) {
                uint64_t words = shapes[i].len / 8, mism = 0;
                id<MTLBuffer> out = gpu_copy(b, words);
                const uint64_t *o = out ? out.contents : NULL;
                for (uint64_t w = 0; w < words; w++)
                    if (!o || o[w] != ((0xF00DULL << 48) | (shapes[i].off + w * 8))) mism++;
                printf(", length %lu, GPU read-back %llu of %llu words wrong", (unsigned long)b.length,
                       mism, words);
            }
            printf("\n");
            b = nil;
            KCHECK(mach_vm_deallocate(mach_task_self(), fresh, 2 * PAGE));
        }
    }
}

/* What a stock 4 KiB guest would need: a pointer or a length that is not a whole host page. */
static void four_k_questions(mach_vm_address_t alias, uint64_t salt) {
    printf("\n== 4 KiB questions (host page %lu), before any other buffer covers the alias ==\n",
           (unsigned long)vm_page_size);
    four_k_fresh();
    id<MTLBuffer> b = [g_dev newBufferWithBytesNoCopy:(void *)(alias + 4096) length:PAGE
                                              options:MTLResourceStorageModeShared
                                          deallocator:nil];
    printf("  newBufferWithBytesNoCopy(alias + 4096, 16 KiB) -> %s\n", b ? "a buffer" : "nil");
    if (b)
        printf("    GPU read-back vs guest pages: %llu of %llu words wrong\n",
               gpu_readback(b, PAGE / 8, g_list[0] + 4096, salt), PAGE / 8);
    b = [g_dev newBufferWithBytesNoCopy:(void *)alias length:4096
                                options:MTLResourceStorageModeShared
                            deallocator:nil];
    printf("  newBufferWithBytesNoCopy(alias, 4 KiB)          -> %s\n", b ? "a buffer" : "nil");
    if (b)
        printf("    GPU read-back vs guest page: %llu of %u words wrong; buffer length %lu\n",
               gpu_readback(b, 4096 / 8, g_list[0], salt), 4096 / 8, (unsigned long)b.length);
    b = nil;

    /* Remap 4 KiB that starts 4 KiB into a guest page: what does the new mapping expose? */
    uint64_t src_gpa = g_list[0] + 4096;
    mach_vm_address_t dst = 0;
    vm_prot_t cur, max;
    kern_return_t k = mach_vm_remap(mach_task_self(), &dst, 4096, 0, VM_FLAGS_ANYWHERE,
                                    mach_task_self(), (mach_vm_address_t)gpa_to_hva(src_gpa),
                                    FALSE, &cur, &max, VM_INHERIT_NONE);
    if (k != KERN_SUCCESS) {
        printf("  mach_vm_remap(4 KiB at page+4096) -> %d (%s)\n", k, mach_error_string(k));
        return;
    }
    mach_vm_address_t base = dst & ~(PAGE - 1);
    const volatile uint64_t *pb = (const volatile uint64_t *)base;
    uint64_t exposed = 0;
    for (uint64_t w = 0; w < WORDS_PER_PAGE; w++)
        if (pb[w] == ((const volatile uint64_t *)gpa_to_hva(g_list[0]))[w]) exposed++;
    printf("  mach_vm_remap(4 KiB at page+4096) -> 0x%llx (offset in host page 0x%llx); the host "
           "page under it matches the whole 16 KiB guest page in %llu/%llu words\n",
           dst, dst - base, exposed, WORDS_PER_PAGE);
    mach_vm_deallocate(mach_task_self(), base, PAGE);
}

/* ---- main ------------------------------------------------------------------- */

static uint64_t rng_state = 0x9e3779b97f4a7c15ULL;
static uint64_t rng(void) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 7;
    rng_state ^= rng_state << 17;
    return rng_state;
}

static void pick_pages(uint64_t run) {
    uint64_t pool = (RAM_BASE + RAM_SIZE - POOL_GPA) / PAGE;
    uint8_t *used = calloc(pool, 1);
    uint64_t n = 0;
    /* Fixed shapes first, when scattered: a guest-adjacent pair split in the alias (list[0],
     * list[2]) and a guest-adjacent pair reversed in the alias (list[4], list[5]). */
    if (run == 1 && g_npages >= 8) {
        uint64_t a = 10, b = pool / 2;
        g_list[0] = a; g_list[1] = pool - 3; g_list[2] = a + 1; g_list[3] = pool / 3;
        g_list[4] = b + 1; g_list[5] = b;
        for (int i = 0; i < 6; i++) used[g_list[i]] = 1;
        n = 6;
    }
    while (n < g_npages) {
        uint64_t k = MIN(run, g_npages - n);
        uint64_t start = rng() % (pool - k);
        bool ok = true;
        for (uint64_t j = 0; j < k; j++) ok &= !used[start + j];
        if (!ok) continue;
        for (uint64_t j = 0; j < k; j++) { used[start + j] = 1; g_list[n++] = start + j; }
    }
    for (uint64_t i = 0; i < g_npages; i++) g_list[i] = POOL_GPA + g_list[i] * PAGE;
    free(used);
}

int main(int argc, char **argv) {
    @autoreleasepool {
        const char *payload_path = "payload.bin";
        uint64_t run = 1, rounds = 4, cycles = 0;
        bool untouched = false, shared = false;
        g_npages = 256;
        for (int i = 1; i < argc; i++) {
            if (!strcmp(argv[i], "--pages") && i + 1 < argc) g_npages = strtoull(argv[++i], 0, 0);
            else if (!strcmp(argv[i], "--run") && i + 1 < argc) run = strtoull(argv[++i], 0, 0);
            else if (!strcmp(argv[i], "--rounds") && i + 1 < argc) rounds = strtoull(argv[++i], 0, 0);
            else if (!strcmp(argv[i], "--seed") && i + 1 < argc) rng_state = strtoull(argv[++i], 0, 0) | 1;
            else if (!strcmp(argv[i], "--cycles") && i + 1 < argc) cycles = strtoull(argv[++i], 0, 0);
            else if (!strcmp(argv[i], "--untouched")) untouched = true;
            else if (!strcmp(argv[i], "--shared")) shared = true;
            else payload_path = argv[i];
        }
        if (g_npages < 1 || g_npages > MAX_PAGES || run < 1) { fprintf(stderr, "bad --pages/--run\n"); return 1; }

        FILE *f = fopen(payload_path, "rb");
        if (!f) { perror(payload_path); return 1; }
        static uint8_t payload[0x1000];
        size_t payload_len = fread(payload, 1, sizeof(payload), f);
        fclose(f);

        CHECK(hv_vm_create(NULL));
        g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE,
                     MAP_ANON | (shared ? MAP_SHARED : MAP_PRIVATE), -1, 0);
        if (g_ram == MAP_FAILED) { perror("mmap"); return 1; }
        memcpy(g_ram, payload, payload_len);
        g_list = gpa_to_hva(LIST_GPA);
        pick_pages(run);
        CHECK(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));

        uint64_t adjacent = 0;
        for (uint64_t i = 1; i < g_npages; i++) adjacent += g_list[i] == g_list[i - 1] + PAGE;
        printf("config: pages=%llu (%llu KiB) run=%llu untouched=%d ram=%s rounds=%llu; "
               "%llu of %llu alias neighbours are guest neighbours\n",
               g_npages, g_npages * PAGE / 1024, run, untouched, shared ? "MAP_SHARED" : "MAP_PRIVATE",
               rounds, adjacent, g_npages - 1);

        CHECK(hv_vcpu_create(&g_vcpu, &g_exit, NULL));
        CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_CPSR, BOOT_CPSR));
        CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_PC, RAM_BASE));
        if (!run_to_ready()) { fprintf(stderr, "guest never became ready\n"); return 1; }
        metal_init();

        const uint64_t all = g_npages * WORDS_PER_PAGE;
        mach_vm_address_t alias = 0;
        unsigned regions_before = count_regions();
        uint64_t t_remap = 0;

        if (untouched) {
            uint64_t t0 = now_ns();
            alias = make_alias();
            t_remap = now_ns() - t0;
        }

        printf("\n== guest FILL(1) ==\n");
        guest_cmd(CMD_FILL, 1);
        expect(guest_cmd(CMD_CHECK, 1) == 0, "guest CHECK(1) reads its own fill");
        expect(guest_cmd(CMD_CHECK, 9) == all, "negative control: guest CHECK(9) misses every word");
        expect(cpu_check_ram(1) == 0, "host reads salt 1 through the VMM's mapping");

        if (!untouched) {
            uint64_t t0 = now_ns();
            alias = make_alias();
            t_remap = now_ns() - t0;
        }
        unsigned regions_after = count_regions();
        printf("\n== alias at 0x%llx: remap %.3f ms, VM regions %u -> %u (+%d) ==\n", alias,
               t_remap / 1e6, regions_before, regions_after, (int)regions_after - (int)regions_before);
        expect(cpu_check_alias((const volatile uint64_t *)alias, 1) == 0, "host reads salt 1 through the alias");

        four_k_questions(alias, 1);

        uint64_t t0 = now_ns();
        id<MTLBuffer> buf = [g_dev newBufferWithBytesNoCopy:(void *)alias length:g_npages * PAGE
                                                    options:MTLResourceStorageModeShared
                                                deallocator:nil];
        uint64_t t_buf = now_ns() - t0;
        printf("\n== newBufferWithBytesNoCopy(alias, %llu KiB) -> %s in %.3f ms ==\n",
               g_npages * PAGE / 1024, buf ? "a buffer" : "nil", t_buf / 1e6);
        if (!buf) { g_failures++; goto out; }
        t0 = now_ns();
        uint64_t m = gpu_pass(buf, 1, 0);
        printf("  first GPU pass %.3f ms\n", (now_ns() - t0) / 1e6);
        expect(m == 0, "GPU CHECK(1): guest -> GPU at creation");
        expect(gpu_pass(buf, 9, 0) == all, "negative control: GPU CHECK(9) misses every word");

        for (uint64_t r = 0; r < rounds; r++) {
            uint64_t gsalt = 2 + 2 * r, fsalt = 3 + 2 * r;
            printf("\n== round %llu: GPU WRITE(%llu), guest FILL(%llu) ==\n", r, gsalt, fsalt);
            gpu_pass(buf, gsalt, 1);
            expect(guest_cmd(CMD_CHECK, gsalt) == 0, "guest CHECK: GPU -> guest through stage-2");
            expect(cpu_check_ram(gsalt) == 0, "host reads the GPU's words through the VMM's mapping");
            guest_cmd(CMD_FILL, fsalt);
            expect(gpu_pass(buf, fsalt, 0) == 0, "GPU CHECK, same MTLBuffer: guest -> GPU after wiring");
            expect(cpu_check_alias((const volatile uint64_t *)alias, fsalt) == 0,
                   "host reads the guest's words through the alias");
        }


        printf("\n== teardown ==\n");
        buf = nil;
        KCHECK(mach_vm_deallocate(mach_task_self(), alias, g_npages * PAGE));
        printf("  VM regions after teardown %u\n", count_regions());
        guest_cmd(CMD_FILL, 100);
        expect(guest_cmd(CMD_CHECK, 100) == 0, "guest FILL/CHECK after the alias is gone");
        expect(cpu_check_ram(100) == 0, "host reads it through the VMM's mapping");

        if (cycles) printf("\n== %llu import cycles: remap, buffer, GPU check, teardown ==\n", cycles);
        uint64_t cycle_mism = 0;
        for (uint64_t c = 0; c < cycles; c++) {
            @autoreleasepool {
                mach_vm_address_t a = make_alias();
                id<MTLBuffer> cb = [g_dev newBufferWithBytesNoCopy:(void *)a length:g_npages * PAGE
                                                           options:MTLResourceStorageModeShared
                                                       deallocator:nil];
                if (!cb) { printf("  cycle %llu: nil buffer\n", c); g_failures++; break; }
                cycle_mism += gpu_pass(cb, 100, 0);
                cb = nil;
                KCHECK(mach_vm_deallocate(mach_task_self(), a, g_npages * PAGE));
            }
            if (c < 4 || (c + 1) % (cycles / 8 ? cycles / 8 : 1) == 0)
                printf("  after cycle %llu: VM regions %u\n", c + 1, count_regions());
        }
        if (cycles) expect(cycle_mism == 0, "every cycle's GPU check read the guest's words");

    out:
        guest_cmd(CMD_DONE, 0);
        CHECK(hv_vcpu_destroy(g_vcpu));
        hv_vm_unmap(RAM_BASE, RAM_SIZE);
        CHECK(hv_vm_destroy());
        printf("\nRESULT: %s (%d failure(s))\n", g_failures ? "FAIL" : "PASS", g_failures);
        return g_failures ? 1 : 0;
    }
}
