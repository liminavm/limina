// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * guest-ram-objects — does guest RAM built from owned, ledger-tagged memory objects
 * bill a page once when both the host and the guest touch it, and does replacing a
 * block free its resident and compressed pages? (docs/design/guest-ram-objects.md §4.1)
 *
 * A real HVF VM with one vCPU running payload.S (it dirties one u64 in every 16 KiB page
 * of a 1 GiB range whose base it reads from a mailbox). Guest RAM is allocated one of
 * four ways ("legs"):
 *   anon        mmap(MAP_ANON|MAP_PRIVATE), today's shape (the control)
 *   hvalloc     hv_vm_allocate()
 *   tagged:<K>  a no-access reservation filled with <K> KiB memory entries made
 *               MAP_MEM_NAMED_CREATE|MAP_MEM_LEDGER_TAGGED, mapped with VM_FLAGS_OVERWRITE
 *   purgeable:<K> the same with MAP_MEM_PURGABLE added (non-volatile)
 * and the probe walks:
 *   S1  one hv_vm_map over the whole RAM (does it span many map entries?)
 *   S2  host memsets H (1 GiB)                          footprint +1x either way
 *   S3  guest dirties the SAME H                        anon: +1x again; tagged: ~0?
 *   S4  MADV_FREE_REUSABLE on H, still stage-2 mapped   tagged: a no-op?
 *   [compressed] ballast pushes H into the compressor
 *   S5  release H: hv_vm_unmap, then replace its blocks (anon: REUSABLE, production)
 *       -> footprint and the HOST's compressor page count, at once and 1 s later
 *   S6  guest re-touches H, healed by mapping H back on its first stage-2 fault
 *   S7  teardown, and the host compressor count 1 s later
 *
 * Usage: probe <leg> [compressed] [ram-GiB]   (payload.bin from build.sh, cwd)
 * Needs com.apple.security.hypervisor (build.sh signs it). An idle host: the
 * compressor counts are host-wide.
 */

#include <Hypervisor/Hypervisor.h>

#include <errno.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/mach_vm.h>
#include <mach/memory_object_types.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define RAM_BASE 0x80000000ULL
#define PAGE_16K 16384ULL
#define MAILBOX_GPA (RAM_BASE + 0x10000ULL)
#define H_GPA (RAM_BASE + (1ULL << 30))
#define RANGE_SIZE (1ULL << 30)
#define MMIO_BASE 0x10000000ULL
#define BOOT_CPSR 0x3C5ULL
#define WATCHDOG_NS (60ULL * 1000 * 1000 * 1000)

#define CHECK(expr)                                                                     \
    do {                                                                                \
        hv_return_t _r = (expr);                                                        \
        if (_r != HV_SUCCESS) {                                                         \
            fprintf(stderr, "FATAL %s:%d %s -> 0x%x\n", __FILE__, __LINE__, #expr,      \
                    (uint32_t)_r);                                                      \
            exit(1);                                                                    \
        }                                                                               \
    } while (0)

#define KCHECK(expr)                                                                    \
    do {                                                                                \
        kern_return_t _k = (expr);                                                      \
        if (_k != KERN_SUCCESS) {                                                       \
            fprintf(stderr, "FATAL %s:%d %s -> %d (%s)\n", __FILE__, __LINE__, #expr,   \
                    _k, mach_error_string(_k));                                         \
            exit(1);                                                                    \
        }                                                                               \
    } while (0)

enum leg { LEG_ANON, LEG_HVALLOC, LEG_TAGGED, LEG_PURGEABLE };

static enum leg g_leg;
static uint64_t g_block;     /* tagged/purgeable block size in bytes */
static uint64_t g_ram_size;
static uint8_t *g_ram;

static uint64_t now_ns(void) {
    static mach_timebase_info_data_t tb;
    if (tb.denom == 0) mach_timebase_info(&tb);
    return mach_absolute_time() * tb.numer / tb.denom;
}

static void *gpa_to_hva(uint64_t gpa) { return g_ram + (gpa - RAM_BASE); }
static void set_mailbox(uint64_t gpa) { *(volatile uint64_t *)gpa_to_hva(MAILBOX_GPA) = gpa; }

/* ---- measurement ------------------------------------------------------------------- */

struct snap {
    double fp, internal, comp, res, host_comp;
};

/* Pages the host's compressor holds, system-wide, in MiB of logical (uncompressed) data. */
static double host_compressor_mib(void) {
    vm_statistics64_data_t vs;
    mach_msg_type_number_t count = HOST_VM_INFO64_COUNT;
    KCHECK(host_statistics64(mach_host_self(), HOST_VM_INFO64, (host_info64_t)&vs, &count));
    return (double)vs.total_uncompressed_pages_in_compressor * (double)vm_page_size / 1048576.0;
}

static struct snap take_snap(void) {
    task_vm_info_data_t info;
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    KCHECK(task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&info, &count));
    return (struct snap){info.phys_footprint / 1048576.0, info.internal / 1048576.0,
                         info.compressed / 1048576.0, info.resident_size / 1048576.0,
                         host_compressor_mib()};
}

static struct snap show(const char *label, struct snap prev) {
    struct snap s = take_snap();
    printf("%-44s fp=%8.1fM (%+8.1f) int=%8.1fM (%+8.1f) comp=%7.1fM (%+8.1f) "
           "res=%8.1fM hostcomp=%9.1fM (%+8.1f)\n",
           label, s.fp, s.fp - prev.fp, s.internal, s.internal - prev.internal, s.comp,
           s.comp - prev.comp, s.res, s.host_comp, s.host_comp - prev.host_comp);
    fflush(stdout);
    return s;
}

/* ---- guest RAM ----------------------------------------------------------------------- */

/* Map a fresh owned, ledger-tagged object over [addr, addr+len): the block allocation and
 * the release primitive both. Overwriting drops the old object's mapping, and with it the
 * old object, which is what frees its pages. */
static void place_block(mach_vm_address_t addr, mach_vm_size_t len) {
    memory_object_size_t sz = len;
    mach_port_t entry = MACH_PORT_NULL;
    vm_prot_t perm = MAP_MEM_NAMED_CREATE | MAP_MEM_LEDGER_TAGGED | VM_PROT_READ | VM_PROT_WRITE;
    if (g_leg == LEG_PURGEABLE) perm |= MAP_MEM_PURGABLE;
    KCHECK(mach_make_memory_entry_64(mach_task_self(), &sz, 0, perm, &entry, MACH_PORT_NULL));
    mach_vm_address_t a = addr;
    KCHECK(mach_vm_map(mach_task_self(), &a, len, 0, VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE, entry,
                       0, FALSE, VM_PROT_READ | VM_PROT_WRITE, VM_PROT_READ | VM_PROT_WRITE,
                       VM_INHERIT_NONE));
    KCHECK(mach_port_deallocate(mach_task_self(), entry));
}

static void alloc_ram(void) {
    uint64_t t0 = now_ns();
    switch (g_leg) {
    case LEG_ANON:
        g_ram = mmap(NULL, g_ram_size, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
        if (g_ram == MAP_FAILED) { perror("mmap"); exit(1); }
        break;
    case LEG_HVALLOC: {
        void *p = NULL;
        CHECK(hv_vm_allocate(&p, g_ram_size, HV_ALLOCATE_DEFAULT));
        g_ram = p;
        break;
    }
    case LEG_TAGGED:
    case LEG_PURGEABLE: {
        mach_vm_address_t a = 0;
        KCHECK(mach_vm_map(mach_task_self(), &a, g_ram_size, 0, VM_FLAGS_ANYWHERE,
                           MEMORY_OBJECT_NULL, 0, FALSE, VM_PROT_NONE,
                           VM_PROT_READ | VM_PROT_WRITE, VM_INHERIT_NONE));
        for (uint64_t off = 0; off < g_ram_size; off += g_block)
            place_block(a + off, g_block);
        g_ram = (uint8_t *)a;
        break;
    }
    }
    printf("allocated %llu GiB in %.3f s\n", g_ram_size >> 30, (now_ns() - t0) / 1e9);
}

/* Replace every block of [hva, hva+len) with a fresh one; for hv_vm_allocate memory, the
 * blocks are its 128 MiB entries, but a fresh tagged entry over any sub-range works the
 * same way, so H is replaced as tagged blocks of the probe's block size. */
static void replace_range(uint8_t *hva, uint64_t len) {
    uint64_t block = g_block ? g_block : (2ULL << 20);
    for (uint64_t off = 0; off < len; off += block)
        place_block((mach_vm_address_t)(hva + off), block);
}

/* ---- the vCPU -------------------------------------------------------------------------- */

static _Atomic uint64_t g_deadline_ns;
static _Atomic bool g_watch_on;
static hv_vcpu_t g_vcpu;
static bool g_heal_h;
static int g_heals;

static void *watchdog_main(void *arg) {
    (void)arg;
    for (;;) {
        usleep(100 * 1000);
        if (atomic_load(&g_watch_on) && now_ns() > atomic_load(&g_deadline_ns)) {
            hv_vcpu_t v = g_vcpu;
            hv_vcpus_exit(&v, 1);
        }
    }
    return NULL;
}

/* Run the payload's touch pass to its DONE store. With g_heal_h, the first stage-2 fault
 * in H maps H back whole (the released range), as the production heal does per window. */
static bool guest_pass(hv_vcpu_exit_t *vexit) {
    atomic_store(&g_deadline_ns, now_ns() + WATCHDOG_NS);
    atomic_store(&g_watch_on, true);
    for (;;) {
        CHECK(hv_vcpu_run(g_vcpu));
        if (vexit->reason == HV_EXIT_REASON_CANCELED) {
            fprintf(stderr, "WATCHDOG\n");
            atomic_store(&g_watch_on, false);
            return false;
        }
        if (vexit->reason != HV_EXIT_REASON_EXCEPTION) {
            fprintf(stderr, "unexpected exit reason %u\n", vexit->reason);
            atomic_store(&g_watch_on, false);
            return false;
        }
        uint64_t ec = (vexit->exception.syndrome >> 26) & 0x3f;
        uint64_t pa = vexit->exception.physical_address;
        if (ec == 0x24 && pa >= MMIO_BASE && pa < MMIO_BASE + 0x1000) {
            uint64_t pc = 0;
            CHECK(hv_vcpu_get_reg(g_vcpu, HV_REG_PC, &pc));
            CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_PC, pc + 4));
            atomic_store(&g_watch_on, false);
            return true;
        }
        if (g_heal_h && ec == 0x24 && pa >= H_GPA && pa < H_GPA + RANGE_SIZE) {
            CHECK(hv_vm_map(gpa_to_hva(H_GPA), H_GPA, RANGE_SIZE,
                            HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
            g_heal_h = false;
            g_heals++;
            continue;
        }
        fprintf(stderr, "unhandled exit ec=0x%llx pa=0x%llx\n", ec, pa);
        atomic_store(&g_watch_on, false);
        return false;
    }
}

/* Resident 16 KiB pages of [p, p+len), by mincore. */
static uint64_t resident_pages(void *p, uint64_t len) {
    uint64_t pages = len / PAGE_16K, n = 0;
    char *vec = malloc(pages);
    if (!vec || mincore(p, len, vec) != 0) { perror("mincore"); free(vec); return 0; }
    for (uint64_t i = 0; i < pages; i++)
        if (vec[i] & MINCORE_INCORE) n++;
    free(vec);
    return n;
}

#ifndef MADV_PAGEOUT
#define MADV_PAGEOUT 10
#endif

/* Push H into the compressor: MADV_PAGEOUT on H itself when the kernel takes it from userspace,
 * otherwise 1 GiB ballast chunks until most of H is no longer resident (the kernel compresses
 * the oldest pages first, so other processes' idle memory goes before H). The ballast is let go
 * before returning; H stays compressed until touched. */
static void compress_h(void) {
    uint8_t *h = gpa_to_hva(H_GPA);
    uint64_t total = RANGE_SIZE / PAGE_16K;
    int r = madvise(h, RANGE_SIZE, MADV_PAGEOUT);
    printf("MADV_PAGEOUT on H: %s; H resident %llu/%llu pages\n", r == 0 ? "ok" : strerror(errno),
           resident_pages(h, RANGE_SIZE), total);
    sleep(2);
    if (resident_pages(h, RANGE_SIZE) < total / 4) return;
    const uint64_t chunk = 1ULL << 30;
    uint8_t *chunks[40];
    int n = 0;
    while (n < 40 && resident_pages(h, RANGE_SIZE) >= total / 4) {
        uint8_t *c = mmap(NULL, chunk, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
        if (c == MAP_FAILED) break;
        /* Incompressible, or the compressor absorbs the ballast and pressure never builds. H is
         * older than every ballast page, so it is compressed first. */
        uint64_t x = 0x9e3779b97f4a7c15ULL ^ (uint64_t)n;
        for (uint64_t *w = (uint64_t *)c, *e = (uint64_t *)(c + chunk); w < e; w++) {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            *w = x;
        }
        chunks[n++] = c;
    }
    printf("ballast: %d GiB pushed; H resident %llu/%llu pages\n", n,
           resident_pages(h, RANGE_SIZE), total);
    for (int i = 0; i < n; i++) munmap(chunks[i], chunk);
}

/* ---- main -------------------------------------------------------------------------------- */

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s anon|hvalloc|tagged:<KiB>|purgeable:<KiB> [compressed] "
                        "[ram-GiB]\n", argv[0]);
        return 2;
    }
    const char *leg = argv[1];
    if (strcmp(leg, "anon") == 0) g_leg = LEG_ANON;
    else if (strcmp(leg, "hvalloc") == 0) g_leg = LEG_HVALLOC;
    else if (strncmp(leg, "tagged:", 7) == 0) { g_leg = LEG_TAGGED; g_block = strtoull(leg + 7, NULL, 10) << 10; }
    else if (strncmp(leg, "purgeable:", 10) == 0) { g_leg = LEG_PURGEABLE; g_block = strtoull(leg + 10, NULL, 10) << 10; }
    else { fprintf(stderr, "unknown leg %s\n", leg); return 2; }
    if ((g_leg == LEG_TAGGED || g_leg == LEG_PURGEABLE) &&
        (g_block < PAGE_16K || g_block % PAGE_16K || RANGE_SIZE % g_block)) {
        fprintf(stderr, "block must be a multiple of 16 KiB dividing 1 GiB\n");
        return 2;
    }
    bool compressed = argc > 2 && strcmp(argv[2], "compressed") == 0;
    g_ram_size = (argc > 3 ? strtoull(argv[3], NULL, 10) : 3) << 30;
    if (g_ram_size < (3ULL << 30)) { fprintf(stderr, "ram must be at least 3 GiB\n"); return 2; }

    FILE *f = fopen("payload.bin", "rb");
    if (!f) { perror("payload.bin"); return 1; }
    static uint8_t payload[1 << 16];
    size_t payload_len = fread(payload, 1, sizeof(payload), f);
    fclose(f);

    pthread_t wt;
    pthread_create(&wt, NULL, watchdog_main, NULL);
    printf("leg=%s block=%llu KiB compressed=%d ram=%llu GiB page=%d\n", leg, g_block >> 10,
           compressed, g_ram_size >> 30, getpagesize());

    struct snap s = take_snap();
    CHECK(hv_vm_create(NULL));
    alloc_ram();
    memcpy(g_ram, payload, payload_len);
    set_mailbox(H_GPA);
    s = show("S0 allocate guest RAM", s);

    uint64_t t0 = now_ns();
    hv_return_t r = hv_vm_map(g_ram, RAM_BASE, g_ram_size,
                              HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
    printf("S1 one hv_vm_map over the whole RAM: 0x%x in %.3f ms\n", (uint32_t)r,
           (now_ns() - t0) / 1e6);
    if (r != HV_SUCCESS) {
        /* Fall back to one map per block so the rest of the probe still runs. */
        uint64_t block = g_block ? g_block : (128ULL << 20);
        t0 = now_ns();
        for (uint64_t off = 0; off < g_ram_size; off += block)
            CHECK(hv_vm_map(g_ram + off, RAM_BASE + off, block,
                            HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
        printf("   fell back to per-block maps: %.3f ms\n", (now_ns() - t0) / 1e6);
    }
    s = show("S1 hv_vm_map", s);

    hv_vcpu_exit_t *vexit;
    CHECK(hv_vcpu_create(&g_vcpu, &vexit, NULL));
    CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_CPSR, BOOT_CPSR));
    CHECK(hv_vcpu_set_reg(g_vcpu, HV_REG_PC, RAM_BASE));

    memset(gpa_to_hva(H_GPA), 0xa5, RANGE_SIZE);
    s = show("S2 host memsets H 1G", s);
    if (!guest_pass(vexit)) return 1;
    s = show("S3 guest dirties the SAME H     << billing", s);

    if (madvise(gpa_to_hva(H_GPA), RANGE_SIZE, MADV_FREE_REUSABLE) != 0)
        printf("S4 madvise REUSABLE failed: %s\n", strerror(errno));
    s = show("S4 MADV_FREE_REUSABLE on H (mapped)", s);

    if (compressed) {
        /* Re-dirty H from both sides so it is resident again, then compress it. */
        memset(gpa_to_hva(H_GPA), 0x3c, RANGE_SIZE);
        if (!guest_pass(vexit)) return 1;
        s = show("S4b re-dirty H from both sides", s);
        compress_h();
        s = show("S4c H pushed to the compressor", s);
    }

    t0 = now_ns();
    CHECK(hv_vm_unmap(H_GPA, RANGE_SIZE));
    if (g_leg == LEG_ANON) {
        if (madvise(gpa_to_hva(H_GPA), RANGE_SIZE, MADV_FREE_REUSABLE) != 0)
            printf("S5 madvise REUSABLE failed: %s\n", strerror(errno));
    } else {
        replace_range(gpa_to_hva(H_GPA), RANGE_SIZE);
    }
    printf("S5 release took %.3f ms\n", (now_ns() - t0) / 1e6);
    s = show("S5 release H (unmap + replace)  << freeing", s);
    sleep(1);
    s = show("S5b one second later", s);

    if (g_leg == LEG_ANON && madvise(gpa_to_hva(H_GPA), RANGE_SIZE, MADV_FREE_REUSE) != 0)
        printf("S6 madvise REUSE failed: %s\n", strerror(errno));
    g_heal_h = true;
    if (!guest_pass(vexit)) return 1;
    printf("   heals: %d\n", g_heals);
    s = show("S6 guest re-touches H (healed)", s);
    uint8_t first = *(volatile uint8_t *)gpa_to_hva(H_GPA + 8);
    printf("   H content after release: byte 8 = 0x%02x (0x00 = a fresh object)\n", first);

    CHECK(hv_vcpu_destroy(g_vcpu));
    (void)hv_vm_unmap(RAM_BASE, g_ram_size); /* H may already be unmapped */
    CHECK(hv_vm_destroy());
    if (g_leg == LEG_HVALLOC) CHECK(hv_vm_deallocate(g_ram, g_ram_size));
    else KCHECK(mach_vm_deallocate(mach_task_self(), (mach_vm_address_t)g_ram, g_ram_size));
    s = show("S7 teardown", s);
    sleep(1);
    show("S7b one second later", s);
    return 0;
}
