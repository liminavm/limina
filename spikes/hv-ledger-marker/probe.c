// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * hv-ledger-marker — does the stage-2 (HV) pmap double-bill guest RAM in
 * phys_footprint, and which release step debits it?
 *
 * The field arithmetic (hv-ledger-gap round 8, re-confirmed 2026-08-11 on the
 * dogfood worker): ~6.5 GB of phys_footprint appears in no vmmap category at
 * all; guest content + graphics + malloc can't reach the billed total, and the
 * footprint peak hit 37.4 GB on a 24 GiB VM. Candidate mechanism from the xnu
 * read (spikes/balloon-compressor-zero/xnu-compressor-semantics.md): billing is
 * per-pmap — a page mapped by BOTH the task pmap and the HV stage-2 pmap may be
 * credited twice (2.04:1 field ratio), and cleanup paths (madvise REUSABLE,
 * compression markers) debit only the pmap they walk.
 *
 * Phases (task_vm_info snapshots around every step):
 *   P0  create VM; RAM = 3 GiB MAP_ANON|MAP_SHARED, hv_vm_map'd whole
 *   P1  HOST memsets H-range (1 GiB)          -> delta vs content (stage-2 never faulted)
 *   P2  vCPU dirties every page of G-range (1 GiB) -> delta vs content: 1x or 2x?
 *   P3  ballast forces both ranges into the compressor -> does footprint GROW
 *       beyond content while pages move to the compressor? (the 37G-peak shape)
 *   P4  H-range: hv_vm_unmap + MADV_FREE_REUSABLE (the production release shape)
 *   P5  G-range: MADV_FREE_REUSABLE with stage-2 still mapped (pre-fix shape),
 *       THEN hv_vm_unmap — if the unmap debits ADDITIONAL footprint, the
 *       stage-2 marker phantom is confirmed and quantified
 *   P6  hv_vm_destroy -> what does teardown debit; final residue
 *
 * Build/run/sign: build.sh (needs com.apple.security.hypervisor; sandbox off).
 */

#include <Hypervisor/Hypervisor.h>

#include <errno.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
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
#define RAM_SIZE (3ULL << 30)
#define PAGE_16K 16384ULL
#define MAILBOX_GPA (RAM_BASE + 0x10000ULL) /* payload re-reads the target base here */
#define G_GPA 0x90000000ULL
#define H_GPA 0xE0000000ULL
#define RANGE_SIZE (1ULL << 30)
#define MMIO_BASE 0x10000000ULL

#define BALLAST_CHUNK (1ULL << 30)
#define BALLAST_MAX 26ULL

#define BOOT_CPSR 0x3C5ULL
#define WATCHDOG_NS (30ULL * 1000 * 1000 * 1000)

static const char *hv_err_name(hv_return_t r) {
    switch ((uint32_t)r) {
    case 0: return "HV_SUCCESS";
    case 0xfae94001: return "HV_ERROR";
    case 0xfae94003: return "HV_BAD_ARGUMENT";
    case 0xfae94007: return "HV_DENIED";
    case 0xfae94008: return "HV_FAULT";
    default: return "HV_???";
    }
}

#define CHECK(expr)                                                                     \
    do {                                                                                \
        hv_return_t _r = (expr);                                                        \
        if (_r != HV_SUCCESS) {                                                         \
            fprintf(stderr, "FATAL %s:%d %s -> 0x%x (%s)\n", __FILE__, __LINE__, #expr, \
                    (uint32_t)_r, hv_err_name(_r));                                     \
            exit(1);                                                                    \
        }                                                                               \
    } while (0)

static uint64_t now_ns(void) {
    static mach_timebase_info_data_t tb;
    if (tb.denom == 0) mach_timebase_info(&tb);
    return mach_absolute_time() * tb.numer / tb.denom;
}

static _Atomic uint64_t g_deadline_ns;
static _Atomic bool g_watch_on;
static hv_vcpu_t g_vcpu;

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

static uint8_t *g_ram;
static void *gpa_to_hva(uint64_t gpa) { return g_ram + (gpa - RAM_BASE); }

/* Retarget the payload's touch pass (it re-loads this at the top of every pass). */
static void set_mailbox(uint64_t gpa) { *(volatile uint64_t *)gpa_to_hva(MAILBOX_GPA) = gpa; }

struct snap {
    double fp, comp, res;
};

static struct snap take_snap(void) {
    task_vm_info_data_t info;
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    kern_return_t kr = task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&info, &count);
    if (kr != KERN_SUCCESS) { fprintf(stderr, "task_info: %d\n", kr); exit(1); }
    return (struct snap){info.phys_footprint / 1048576.0, info.compressed / 1048576.0,
                         info.resident_size / 1048576.0};
}

static struct snap show(const char *label, struct snap prev) {
    struct snap s = take_snap();
    printf("%-46s fp=%8.1fM (%+8.1f)  comp=%8.1fM (%+8.1f)  res=%8.1fM\n", label, s.fp,
           s.fp - prev.fp, s.comp, s.comp - prev.comp, s.res);
    fflush(stdout);
    return s;
}

static size_t resident_pages(void *p, size_t len) {
    size_t pages = len / (size_t)getpagesize();
    char *vec = malloc(pages);
    if (mincore(p, len, vec) != 0) { perror("mincore"); free(vec); return (size_t)-1; }
    size_t n = 0;
    for (size_t i = 0; i < pages; i++)
        if (vec[i] & MINCORE_INCORE) n++;
    free(vec);
    return n;
}

/* Cycle-mode heal state: 2 MiB windows over the G-range (the production chunked-heal
 * shape). Faults on released pages heal their window with MADV_FREE_REUSE + hv_vm_map. */
#define HEAL_WIN (2ULL << 20)
#define N_WIN (RANGE_SIZE / HEAL_WIN)
static bool g_cycle_heal;
static uint8_t g_win_mapped[N_WIN];
static int g_heals;

/* Double-bill mode: heal any stage-2 fault in guest RAM by remapping the single
 * faulting 16 KiB page, and count them. A nonzero count after the mprotect
 * remedy cycle means the protection change tore down stage-2, not just the
 * task-pmap PTEs — that's the go/no-go signal for a production sweep. */
static bool g_dbl_heal;
static int g_dbl_faults;

/* Run the vCPU until the payload's DONE MMIO store (PC is advanced past it, so a
 * later run continues into the payload's next touch pass). In cycle mode, stage-2
 * faults on the released G-range are healed production-style and retried. */
static bool run_guest_dirty_pass(hv_vcpu_t vcpu, hv_vcpu_exit_t *vexit) {
    atomic_store(&g_deadline_ns, now_ns() + WATCHDOG_NS);
    atomic_store(&g_watch_on, true);
    for (;;) {
        CHECK(hv_vcpu_run(vcpu));
        if (vexit->reason == HV_EXIT_REASON_CANCELED) {
            uint64_t pc = 0;
            hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
            fprintf(stderr, "WATCHDOG: pc=0x%llx heals=%d\n", pc, g_heals);
            atomic_store(&g_watch_on, false);
            return false;
        }
        if (vexit->reason != HV_EXIT_REASON_EXCEPTION) {
            fprintf(stderr, "unexpected exit reason %u\n", vexit->reason);
            atomic_store(&g_watch_on, false);
            return false;
        }
        uint64_t syn = vexit->exception.syndrome;
        uint64_t ec = (syn >> 26) & 0x3f;
        uint64_t pa = vexit->exception.physical_address;
        if (ec == 0x24 && pa >= MMIO_BASE && pa < MMIO_BASE + 0x1000) {
            uint64_t pc = 0;
            CHECK(hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc));
            CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4));
            atomic_store(&g_watch_on, false);
            return true; /* DONE */
        }
        if (g_dbl_heal && ec == 0x24 && pa >= RAM_BASE && pa < RAM_BASE + RAM_SIZE) {
            uint64_t page = pa & ~(PAGE_16K - 1);
            /* If the hv registration survived, a fresh map overlaps — replace it. */
            if (hv_vm_map(gpa_to_hva(page), page, PAGE_16K,
                          HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC) != HV_SUCCESS) {
                hv_vm_unmap(page, PAGE_16K);
                CHECK(hv_vm_map(gpa_to_hva(page), page, PAGE_16K,
                                HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
            }
            g_dbl_faults++;
            if (g_dbl_faults % 8192 == 0)
                printf("  ...healed %d stage-2 faults\n", g_dbl_faults);
            atomic_store(&g_deadline_ns, now_ns() + WATCHDOG_NS);
            continue; /* no PC advance: retry the access */
        }
        if (g_cycle_heal && ec == 0x24 && pa >= G_GPA && pa < G_GPA + RANGE_SIZE) {
            uint64_t w = (pa - G_GPA) / HEAL_WIN;
            if (!g_win_mapped[w]) {
                uint64_t gpa = G_GPA + w * HEAL_WIN;
                if (madvise(gpa_to_hva(gpa), HEAL_WIN, MADV_FREE_REUSE) != 0)
                    printf("heal REUSE failed: %s\n", strerror(errno));
                CHECK(hv_vm_map(gpa_to_hva(gpa), gpa, HEAL_WIN,
                                HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
                g_win_mapped[w] = 1;
                g_heals++;
                atomic_store(&g_deadline_ns, now_ns() + WATCHDOG_NS);
                continue; /* no PC advance: retry the store */
            }
            fprintf(stderr, "repeat fault in healed window pa=0x%llx\n", pa);
            atomic_store(&g_watch_on, false);
            return false;
        }
        fprintf(stderr, "unhandled exit ec=0x%llx pa=0x%llx syndrome=0x%llx\n", ec, pa, syn);
        atomic_store(&g_watch_on, false);
        return false;
    }
}

int main(int argc, char **argv) {
    const char *payload_path = argc > 1 ? argv[1] : "payload.bin";
    FILE *f = fopen(payload_path, "rb");
    if (!f) { perror(payload_path); return 1; }
    static uint8_t payload[1 << 20];
    size_t payload_len = fread(payload, 1, sizeof(payload), f);
    fclose(f);
    printf("payload: %zu bytes; page=%d\n", payload_len, getpagesize());

    pthread_t wt;
    pthread_create(&wt, NULL, watchdog_main, NULL);

    struct snap s = take_snap();
    printf("%-46s fp=%8.1fM             comp=%8.1fM             res=%8.1fM\n",
           "baseline", s.fp, s.comp, s.res);

    /* P0. RAM flags: production (libkrun via vm-memory) is MAP_ANONYMOUS|MAP_PRIVATE;
     * "shared" keeps the earlier MAP_SHARED runs reproducible. */
    int shared_ram = argc > 3 && strcmp(argv[3], "shared") == 0;
    printf("guest RAM: MAP_ANON|%s\n", shared_ram ? "MAP_SHARED" : "MAP_PRIVATE");
    CHECK(hv_vm_create(NULL));
    g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE,
                 MAP_ANON | (shared_ram ? MAP_SHARED : MAP_PRIVATE), -1, 0);
    if (g_ram == MAP_FAILED) { perror("mmap"); return 1; }
    memcpy(g_ram, payload, payload_len);
    set_mailbox(G_GPA); /* default target; double mode retargets per pass */
    CHECK(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE,
                    HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
    s = show("P0 vm_create + map 3G (code page dirty)", s);

    /* mode "double": the cell the original matrix never measured — the SAME range
     * touched by BOTH sides (H host-then-guest, G guest-then-host). This is the
     * production shape of every disk-fed guest page: virtio-blk writes the buffer
     * through the task mapping, the guest then faults it through stage-2. If the
     * second toucher bills another 1x, the entire guest page cache double-bills
     * phys_footprint — the 2026-08-12 field 2x (ledger 34.2G vs footprint tool 17G,
     * compressed ~0). Then the remedy cells: an mprotect(PROT_NONE->RW) cycle over
     * the both-touched range (does it debit the task-pmap share? does the guest
     * keep running without stage-2 faults?), and a host re-touch (the re-double
     * rate a production sweep would pay on hot virtio pages). */
    if (argc > 2 && strcmp(argv[2], "double") == 0) {
        hv_vcpu_t vcpu;
        hv_vcpu_exit_t *vexit;
        CHECK(hv_vcpu_create(&vcpu, &vexit, NULL));
        g_vcpu = vcpu;
        CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, BOOT_CPSR));
        CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE));

        /* Cell A: host writes first (virtio-blk shape), guest re-touches. */
        memset(gpa_to_hva(H_GPA), 0xa5, RANGE_SIZE);
        s = show("D1 HOST memset H 1G (host-first)", s);
        set_mailbox(H_GPA);
        if (!run_guest_dirty_pass(vcpu, vexit)) return 1;
        s = show("D2 GUEST dirties the SAME H range   << CELL A", s);

        /* Cell B: guest writes first, host re-touches (virtio-net tx shape). */
        set_mailbox(G_GPA);
        if (!run_guest_dirty_pass(vcpu, vexit)) return 1;
        s = show("D3 GUEST dirties G 1G (guest-first)", s);
        memset(gpa_to_hva(G_GPA), 0x5a, RANGE_SIZE);
        s = show("D4 HOST memset the SAME G range     << CELL B", s);

        /* Remedy: drop the task-pmap PTEs over H without touching the content. */
        if (mprotect(gpa_to_hva(H_GPA), RANGE_SIZE, PROT_NONE) != 0)
            printf("D5 mprotect(H, NONE) failed: %s\n", strerror(errno));
        s = show("D5 mprotect(H, PROT_NONE)", s);
        if (mprotect(gpa_to_hva(H_GPA), RANGE_SIZE, PROT_READ | PROT_WRITE) != 0)
            printf("D6 mprotect(H, RW) failed: %s\n", strerror(errno));
        s = show("D6 mprotect(H, RW) restore", s);

        /* Did the cycle tear stage-2? Guest re-touch with per-page healing. */
        g_dbl_heal = true;
        g_dbl_faults = 0;
        set_mailbox(H_GPA);
        if (!run_guest_dirty_pass(vcpu, vexit)) return 1;
        g_dbl_heal = false;
        printf("   stage-2 faults during guest re-touch: %d (0 = stage-2 survived)\n",
               g_dbl_faults);
        s = show("D7 GUEST re-touches H post-cycle", s);

        /* Verify content survived the cycle (it must — these are guest pages). */
        uint8_t probe_byte = *((volatile uint8_t *)gpa_to_hva(H_GPA) + 8);
        printf("   H content spot-check: 0x%02x (0xa5 = survived; payload wrote u64[0])\n",
               probe_byte);

        /* Re-double rate: the host touching the swept range again. */
        memset(gpa_to_hva(H_GPA), 0xb6, RANGE_SIZE);
        s = show("D8 HOST re-memset H (re-double?)", s);

        /* RO-window variant: if a downgrade to PROT_READ also debits, a sweep
         * can leave reads safe for concurrent worker threads and only writes
         * need the fault-retry path. No debit = pmap edits PTEs in place and
         * only the NONE window works. */
        if (mprotect(gpa_to_hva(H_GPA), RANGE_SIZE, PROT_READ) != 0)
            printf("D10 mprotect(H, PROT_READ) failed: %s\n", strerror(errno));
        s = show("D10 mprotect(H, PROT_READ)", s);
        if (mprotect(gpa_to_hva(H_GPA), RANGE_SIZE, PROT_READ | PROT_WRITE) != 0)
            printf("D11 mprotect(H, RW) failed: %s\n", strerror(errno));
        s = show("D11 mprotect(H, RW) restore", s);
        volatile uint8_t sink = *((volatile uint8_t *)gpa_to_hva(H_GPA) + 8);
        (void)sink;
        s = show("D12 HOST reads one byte of H", s);

        CHECK(hv_vcpu_destroy(vcpu));
        hv_vm_unmap(RAM_BASE, RAM_SIZE);
        s = show("D9 unmap all + vcpu destroy", s);
        CHECK(hv_vm_destroy());
        s = show("D9b hv_vm_destroy", s);
        munmap(g_ram, RAM_SIZE);
        show("D9c munmap RAM", s);
        printf("\nread the deltas: D2/D4 ~+1024M = the second pmap bills again (the\n"
               "page-cache 2x); ~0 = double-billing falsified for the both-touch cell.\n"
               "D5 ~-1024M with D7 faults=0 = the sweep remedy is viable; D8 = its\n"
               "steady-state cost on host-hot pages.\n");
        return 0;
    }

    /* P1: host touch */
    memset(gpa_to_hva(H_GPA), 0xa5, RANGE_SIZE);
    s = show("P1 HOST memset H-range 1G", s);

    /* P2: guest touch */
    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *vexit;
    CHECK(hv_vcpu_create(&vcpu, &vexit, NULL));
    g_vcpu = vcpu;
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, BOOT_CPSR));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE));
    if (!run_guest_dirty_pass(vcpu, vexit)) return 1;
    s = show("P2 GUEST dirtied G-range 1G (one u64/page)", s);

    /* mode "cycle": the field's actual loop — release (unmap+REUSABLE, the production
     * shape) -> guest re-touches everything -> 512 chunked heals (REUSE+remap) -> release
     * again. A per-cycle footprint ratchet here replicates the post-fix dogfood excess;
     * flat means the leak is not in the release/heal cycle itself.
     * "cycle-pressure": same, with ~14G of held ballast so the compressor participates. */
    int cycle = argc > 2 && strncmp(argv[2], "cycle", 5) == 0;
    if (cycle) {
        int pressure = strcmp(argv[2], "cycle-pressure") == 0;
        void *ballast[BALLAST_MAX];
        size_t nballast = 0;
        if (pressure) {
            printf("holding 14G dirty ballast through the cycles...\n");
            for (; nballast < 14; nballast++) {
                void *b = mmap(NULL, BALLAST_CHUNK, PROT_READ | PROT_WRITE,
                               MAP_ANON | MAP_PRIVATE, -1, 0);
                if (b == MAP_FAILED) break;
                memset(b, 0x11, BALLAST_CHUNK);
                ballast[nballast] = b;
            }
        }
        g_cycle_heal = true;
        struct snap base = take_snap();
        printf("cycle base: fp=%.1fM comp=%.1fM\n", base.fp, base.comp);
        for (int c = 1; c <= 10; c++) {
            CHECK(hv_vm_unmap(G_GPA, RANGE_SIZE));
            if (madvise(gpa_to_hva(G_GPA), RANGE_SIZE, MADV_FREE_REUSABLE) != 0)
                printf("cycle %d REUSABLE failed: %s\n", c, strerror(errno));
            memset(g_win_mapped, 0, sizeof(g_win_mapped));
            struct snap rel = take_snap();
            g_heals = 0;
            if (!run_guest_dirty_pass(vcpu, vexit)) return 1;
            struct snap tch = take_snap();
            printf("cycle %2d: released fp=%8.1fM comp=%8.1fM | re-touched (heals=%3d) "
                   "fp=%8.1fM comp=%8.1fM | drift vs base %+7.1fM\n",
                   c, rel.fp, rel.comp, g_heals, tch.fp, tch.comp, tch.fp - base.fp);
            fflush(stdout);
        }
        for (size_t i = 0; i < nballast; i++) munmap(ballast[i], BALLAST_CHUNK);
        s = show("cycles done (ballast freed)", take_snap());
        CHECK(hv_vm_unmap(G_GPA, RANGE_SIZE));
        s = show("final hv_vm_unmap G", s);
        if (madvise(gpa_to_hva(G_GPA), RANGE_SIZE, MADV_FREE_REUSABLE) != 0)
            printf("final REUSABLE failed: %s\n", strerror(errno));
        s = show("final REUSABLE G", s);
        CHECK(hv_vcpu_destroy(vcpu));
        hv_vm_unmap(RAM_BASE, RAM_SIZE);
        CHECK(hv_vm_destroy());
        s = show("teardown (vm destroyed)", s);
        munmap(g_ram, RAM_SIZE);
        show("munmap RAM", s);
        return 0;
    }

    /* mode "resident": skip compression entirely — the exact pre-fix L1-test shape
     * (fresh guest-faulted resident pages, stage-2 mapped, REUSABLE straight away). */
    int skip_ballast = argc > 2 && strcmp(argv[2], "resident") == 0;

    /* P3: compress both ranges */
    printf("P3 ballast (up to %lluG) to compress G+H...%s\n", BALLAST_MAX,
           skip_ballast ? " SKIPPED (resident mode)" : "");
    void *ballast[BALLAST_MAX];
    size_t nballast = 0;
    for (; !skip_ballast && nballast < BALLAST_MAX; nballast++) {
        void *b = mmap(NULL, BALLAST_CHUNK, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE,
                       -1, 0);
        if (b == MAP_FAILED) break;
        memset(b, 0x11, BALLAST_CHUNK);
        ballast[nballast] = b;
        size_t gres = resident_pages(gpa_to_hva(G_GPA), RANGE_SIZE);
        size_t hres = resident_pages(gpa_to_hva(H_GPA), RANGE_SIZE);
        struct snap now = take_snap();
        printf("  ballast %2zuG: G res=%6zu H res=%6zu  fp=%8.1fM comp=%8.1fM\n",
               nballast + 1, gres, hres, now.fp, now.comp);
        fflush(stdout);
        if ((gres + hres) * (size_t)getpagesize() < RANGE_SIZE / 2) break;
    }
    for (size_t i = 0; i < nballast; i++) munmap(ballast[i], BALLAST_CHUNK);
    sleep(2);
    s = show("P3 ballast freed, targets compressed", s);
    printf("   G res=%zu pages, H res=%zu pages (of %llu each)\n",
           resident_pages(gpa_to_hva(G_GPA), RANGE_SIZE),
           resident_pages(gpa_to_hva(H_GPA), RANGE_SIZE),
           RANGE_SIZE / PAGE_16K);

    /* P5 first in resident mode: pre-fix shape on G with NO prior hv_vm_unmap in the
     * whole process, so the first madvise runs against a pristine hv mapping. */
    if (madvise(gpa_to_hva(G_GPA), RANGE_SIZE, MADV_FREE_REUSABLE) != 0)
        printf("P5a REUSABLE G failed: %s\n", strerror(errno));
    s = show("P5a MADV_FREE_REUSABLE G (stage-2 MAPPED)", s);
    CHECK(hv_vm_unmap(G_GPA, RANGE_SIZE));
    s = show("P5b hv_vm_unmap G-range afterwards", s);

    /* P4: H-range. Default = production release shape (unmap then REUSABLE).
     * mode argv[4] "h-mapped-first": REUSABLE while still mapped — H has no
     * guest-faulted stage-2 PTEs, so this splits "hv mapping covers the range"
     * from "guest faults populated stage-2 PTEs" as the no-op condition. */
    int h_mapped_first = argc > 4 && strcmp(argv[4], "h-mapped-first") == 0;
    if (h_mapped_first) {
        if (madvise(gpa_to_hva(H_GPA), RANGE_SIZE, MADV_FREE_REUSABLE) != 0)
            printf("P4a REUSABLE H failed: %s\n", strerror(errno));
        s = show("P4a MADV_FREE_REUSABLE H (MAPPED, host-faulted)", s);
        CHECK(hv_vm_unmap(H_GPA, RANGE_SIZE));
        s = show("P4b hv_vm_unmap H-range afterwards", s);
    } else {
        CHECK(hv_vm_unmap(H_GPA, RANGE_SIZE));
        s = show("P4a hv_vm_unmap H-range", s);
        if (madvise(gpa_to_hva(H_GPA), RANGE_SIZE, MADV_FREE_REUSABLE) != 0)
            printf("P4b REUSABLE H failed: %s\n", strerror(errno));
        s = show("P4b MADV_FREE_REUSABLE H-range", s);
    }

    /* P6: teardown */
    CHECK(hv_vcpu_destroy(vcpu));
    hv_vm_unmap(RAM_BASE, RAM_SIZE);
    s = show("P6a unmap rest + vcpu destroy", s);
    CHECK(hv_vm_destroy());
    s = show("P6b hv_vm_destroy", s);
    munmap(g_ram, RAM_SIZE);
    show("P6c munmap RAM", s);

    printf("\nread the deltas: P1/P2 vs 1024M each (2x = per-mapping double-billing at\n"
           "touch time); P3 growth = phantom minted at compression; P5a-vs-P5b split =\n"
           "what REUSABLE misses and hv_vm_unmap recovers; P6 residue = the graveyard.\n");
    return 0;
}
