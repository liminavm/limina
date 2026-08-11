// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * balloon-unmap-fault host driver — the mechanism spike for the FRQ/balloon
 * stage-2 unmap fix (spikes/hv-ledger-gap round 8c; hand-rolled mmu notifier).
 *
 * Questions this must answer before the libkrun implementation:
 *  Q1  Does hv_vm_unmap of a 16 KiB page inside live guest RAM turn a guest STR
 *      into an EC 0x24 data-abort exit with a valid physical_address?
 *  Q2  Does guest EXECUTION from an unmapped page arrive as EC 0x20 (instruction
 *      abort) and is physical_address valid there too?
 *  Q3  Does an ISV=0 access (STP — the memset/memcpy shape) fault the same way,
 *      with a valid physical_address?
 *  Q4  Can the vCPU thread itself hv_vm_map the page back mid-exit and retry the
 *      instruction (no PC advance) — MADV_FREE_REUSE + remap + rerun => correct
 *      guest execution and the store landing in the same host backing?
 *  Q5  API semantics: hv_vm_map over an existing mapping; hv_vm_map spanning a
 *      hole + live mapping; double hv_vm_unmap; hv_vm_unmap spanning
 *      mapped+unmapped; madvise(MADV_FREE_REUSE) on a non-reusable range.
 *      (Decides precise range-set bookkeeping vs blind unmap+map chunking.)
 *
 * Verdict: GREEN iff the guest reaches DONE with all checkpoint values correct
 * and every fault was healed by remap-and-retry (with per-PA livelock caps).
 *
 * Build/run/sign: build.sh (needs com.apple.security.hypervisor).
 */

#include <Hypervisor/Hypervisor.h>

#include <errno.h>
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
#define RAM_SIZE (64ULL << 20)
#define PAGE_16K 16384ULL

#define DATA_GPA 0x80100000ULL
#define EXEC_GPA 0x80200000ULL
#define STP_GPA 0x80300000ULL

#define MMIO_BASE 0x10000000ULL
#define M_CKPT1 0x00
#define M_CKPT2 0x08
#define M_CKPT3 0x10
#define M_CKPT4 0x18
#define M_DONE 0x20

#define BOOT_CPSR 0x3C5ULL /* EL1h, DAIF masked */
#define WATCHDOG_NS (20ULL * 1000 * 1000 * 1000)

static const char *hv_err_name(hv_return_t r) {
    switch ((uint32_t)r) {
    case 0: return "HV_SUCCESS";
    case 0xfae94001: return "HV_ERROR";
    case 0xfae94002: return "HV_BUSY";
    case 0xfae94003: return "HV_BAD_ARGUMENT";
    case 0xfae94005: return "HV_NO_RESOURCES";
    case 0xfae94006: return "HV_NO_DEVICE";
    case 0xfae94007: return "HV_DENIED";
    case 0xfae94008: return "HV_FAULT";
    case 0xfae9400f: return "HV_UNSUPPORTED";
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

/* ------------------------------------------------- Q5: API semantics probes */

static void probe_api_semantics(void) {
    printf("\n=== Q5: hv_vm_map/unmap + madvise API semantics ===\n");
    /* Scratch range well away from the payload: RAM_BASE + 48 MiB, 4 pages. */
    uint64_t s = RAM_BASE + (48ULL << 20);
    hv_return_t r;

    r = hv_vm_map(gpa_to_hva(s), s, 4 * PAGE_16K,
                  HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
    printf("map over an ALREADY-MAPPED range      -> 0x%x (%s)\n", (uint32_t)r, hv_err_name(r));

    CHECK(hv_vm_unmap(s + PAGE_16K, PAGE_16K)); /* punch a one-page hole */
    r = hv_vm_map(gpa_to_hva(s), s, 4 * PAGE_16K,
                  HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
    printf("map spanning HOLE + live mappings     -> 0x%x (%s)\n", (uint32_t)r, hv_err_name(r));

    /* Whatever the answer above, restore the hole precisely for the next probes. */
    hv_vm_unmap(s + PAGE_16K, PAGE_16K);
    r = hv_vm_map(gpa_to_hva(s + PAGE_16K), s + PAGE_16K, PAGE_16K,
                  HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
    printf("map EXACTLY the hole                  -> 0x%x (%s)\n", (uint32_t)r, hv_err_name(r));

    CHECK(hv_vm_unmap(s, PAGE_16K));
    r = hv_vm_unmap(s, PAGE_16K);
    printf("unmap an ALREADY-UNMAPPED page        -> 0x%x (%s)\n", (uint32_t)r, hv_err_name(r));

    r = hv_vm_unmap(s, 3 * PAGE_16K); /* page 0 unmapped, pages 1-2 mapped */
    printf("unmap spanning UNMAPPED + mapped      -> 0x%x (%s)\n", (uint32_t)r, hv_err_name(r));

    /* Restore whatever state the probes left, precisely. */
    hv_vm_unmap(s, 4 * PAGE_16K);
    r = hv_vm_map(gpa_to_hva(s), s, 4 * PAGE_16K,
                  HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
    printf("restore map of the scratch range      -> 0x%x (%s)\n", (uint32_t)r, hv_err_name(r));

    int rc = madvise(gpa_to_hva(s), PAGE_16K, MADV_FREE_REUSE);
    printf("MADV_FREE_REUSE on non-reusable range -> rc=%d errno=%d (%s)\n", rc,
           rc ? errno : 0, rc ? strerror(errno) : "-");
}

/* ------------------------------------------------------------ fault healing */

typedef struct {
    uint64_t pa;
    int count;
} fault_rec_t;

static fault_rec_t g_faults[16];
static int g_nfaults;
static int g_heals;

/* Records the fault, REUSEs + remaps the containing 16 KiB page, enforces the
 * per-PA livelock cap. Returns false if the cap is blown. */
static bool heal_ram_fault(const char *kind, uint64_t pa, uint64_t syndrome) {
    uint64_t page = pa & ~(PAGE_16K - 1);
    int isv = (syndrome >> 24) & 1;
    uint64_t xfsc = syndrome & 0x3f;

    for (int i = 0; i < g_nfaults; i++) {
        if (g_faults[i].pa == page && ++g_faults[i].count > 8) {
            fprintf(stderr, "LIVELOCK: page 0x%llx faulted %d times despite remap\n", page,
                    g_faults[i].count);
            return false;
        }
    }
    bool seen = false;
    for (int i = 0; i < g_nfaults; i++) seen |= (g_faults[i].pa == page);
    if (!seen && g_nfaults < 16) g_faults[g_nfaults++] = (fault_rec_t){page, 1};

    printf("[fault] %s pa=0x%llx page=0x%llx isv=%d xfsc=0x%llx -> REUSE + remap + retry\n",
           kind, pa, page, isv, xfsc);

    int rc = madvise(gpa_to_hva(page), PAGE_16K, MADV_FREE_REUSE);
    if (rc != 0)
        printf("[fault]   MADV_FREE_REUSE rc=%d errno=%d (%s) — continuing\n", rc, errno,
               strerror(errno));
    hv_return_t r = hv_vm_map(gpa_to_hva(page), page, PAGE_16K,
                              HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
    if (r != HV_SUCCESS) {
        fprintf(stderr, "[fault]   remap FAILED: 0x%x (%s)\n", (uint32_t)r, hv_err_name(r));
        return false;
    }
    g_heals++;
    return true;
}

/* ------------------------------------------------------------------ run loop */

typedef struct {
    bool done;
    uint64_t ckpt2, ckpt3, ckpt4;
    int unmap_done;
} run_state_t;

static bool run_guest(hv_vcpu_t vcpu, hv_vcpu_exit_t *vexit, run_state_t *st) {
    atomic_store(&g_deadline_ns, now_ns() + WATCHDOG_NS);
    atomic_store(&g_watch_on, true);

    for (;;) {
        CHECK(hv_vcpu_run(vcpu));

        if (vexit->reason == HV_EXIT_REASON_CANCELED) {
            uint64_t pc = 0;
            hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
            fprintf(stderr, "WATCHDOG: no progress; pc=0x%llx heals=%d\n", pc, g_heals);
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
        uint64_t va = vexit->exception.virtual_address;

        if (ec == 0x20) { /* instruction abort, lower EL */
            printf("[exit] INSTR ABORT pa=0x%llx va=0x%llx syndrome=0x%llx\n", pa, va, syn);
            if (pa < RAM_BASE || pa >= RAM_BASE + RAM_SIZE) {
                fprintf(stderr, "instruction abort OUTSIDE RAM — pa invalid?\n");
                atomic_store(&g_watch_on, false);
                return false;
            }
            if (!heal_ram_fault("instr", pa, syn)) {
                atomic_store(&g_watch_on, false);
                return false;
            }
            continue; /* no PC advance: retry the fetch */
        }

        if (ec == 0x24) { /* data abort, lower EL */
            bool iswrite = (syn >> 6) & 1;
            uint32_t srt = (syn >> 16) & 0x1f;

            if (pa >= RAM_BASE && pa < RAM_BASE + RAM_SIZE) {
                /* The access under test: heal and RETRY (no PC advance). */
                if (!heal_ram_fault(iswrite ? "data-w" : "data-r", pa, syn)) {
                    atomic_store(&g_watch_on, false);
                    return false;
                }
                continue;
            }

            /* MMIO checkpoint protocol: emulate and advance the PC. */
            uint64_t pc = 0;
            CHECK(hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc));
            CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4));

            if (!iswrite) {
                if (srt < 31) CHECK(hv_vcpu_set_reg(vcpu, HV_REG_X0 + srt, 0));
                continue;
            }
            uint64_t val = 0;
            if (srt < 31) CHECK(hv_vcpu_get_reg(vcpu, HV_REG_X0 + srt, &val));

            if (pa < MMIO_BASE || pa >= MMIO_BASE + 0x1000) {
                fprintf(stderr, "unexpected MMIO write pa=0x%llx\n", pa);
                continue;
            }
            switch (pa - MMIO_BASE) {
            case M_CKPT1: {
                printf("[ckpt1] unmapping DATA/EXEC/STP pages + MADV_FREE_REUSABLE\n");
                uint64_t pages[3] = {DATA_GPA, EXEC_GPA, STP_GPA};
                for (int i = 0; i < 3; i++) {
                    CHECK(hv_vm_unmap(pages[i], PAGE_16K));
                    if (madvise(gpa_to_hva(pages[i]), PAGE_16K, MADV_FREE_REUSABLE) != 0)
                        printf("[ckpt1]   REUSABLE 0x%llx failed: %s\n", pages[i],
                               strerror(errno));
                }
                st->unmap_done = 1;
                break;
            }
            case M_CKPT2:
                st->ckpt2 = val;
                printf("[ckpt2] healed STR readback = 0x%llx (expect 0x22); host VA sees 0x%llx\n",
                       val, *(volatile uint64_t *)gpa_to_hva(DATA_GPA));
                break;
            case M_CKPT3:
                st->ckpt3 = val;
                printf("[ckpt3] healed BLR returned = 0x%llx (expect 0x33)\n", val);
                break;
            case M_CKPT4:
                st->ckpt4 = val;
                printf("[ckpt4] healed STP/LDP sum  = 0x%llx (expect 0x99)\n", val);
                break;
            case M_DONE:
                printf("[done] guest reached DONE\n");
                st->done = true;
                atomic_store(&g_watch_on, false);
                return true;
            default:
                break;
            }
            continue;
        }

        uint64_t pc = 0;
        hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
        fprintf(stderr, "unhandled exception ec=0x%llx syndrome=0x%llx pc=0x%llx pa=0x%llx\n",
                ec, syn, pc, pa);
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
    printf("payload: %s (%zu bytes)\n", payload_path, payload_len);

    pthread_t wt;
    pthread_create(&wt, NULL, watchdog_main, NULL);

    CHECK(hv_vm_create(NULL));
    g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (g_ram == MAP_FAILED) { perror("mmap"); return 1; }
    memcpy(g_ram, payload, payload_len);

    /* Plant the EXEC-page function: movz x0,#0x33 ; ret */
    uint32_t fn[2] = {0xD2800660, 0xD65F03C0};
    memcpy(gpa_to_hva(EXEC_GPA), fn, sizeof(fn));

    CHECK(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE,
                    HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));

    probe_api_semantics();

    printf("\n=== guest run: unmap -> fault -> REUSE+remap -> retry ===\n");
    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *vexit;
    CHECK(hv_vcpu_create(&vcpu, &vexit, NULL));
    g_vcpu = vcpu;
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, BOOT_CPSR));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE));

    run_state_t st = {0};
    bool ran = run_guest(vcpu, vexit, &st);

    printf("\n=== verdict ===\n");
    printf("heals=%d ckpt2=0x%llx ckpt3=0x%llx ckpt4=0x%llx done=%d\n", g_heals, st.ckpt2,
           st.ckpt3, st.ckpt4, st.done);
    bool green = ran && st.done && st.ckpt2 == 0x22 && st.ckpt3 == 0x33 && st.ckpt4 == 0x99 &&
                 g_heals >= 3;
    printf("VERDICT: %s — unmap->fault->REUSE+remap->retry %s (data ISV=1, exec, ISV=0 all "
           "healed: %s)\n",
           green ? "GREEN" : "RED", green ? "works end-to-end" : "FAILED",
           green ? "yes" : "no");

    CHECK(hv_vcpu_destroy(vcpu));
    hv_vm_unmap(RAM_BASE, RAM_SIZE);
    CHECK(hv_vm_destroy());
    return green ? 0 : 1;
}
