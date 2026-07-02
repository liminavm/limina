// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * m9-hvf-state-roundtrip host driver — M9.0 spike #2.
 *
 * Question: can Hypervisor.framework round-trip FULL vCPU + GIC state into a FRESH VM
 * (created in the same process after hv_vm_destroy) such that the guest continues
 * correctly? This is the go/no-go gate for the M9 host-side VM snapshot design
 * (docs/design/m9-suspend-resume.md §M9.0 item 2, §"What HVF gives us").
 *
 * Three phases, one process:
 *   [control]  VM #1: boot payload.bin, run uninterrupted to checkpoint B.
 *              Records the reference checksum + interrupt count. VM destroyed.
 *   [vm1]      VM #2: boot identically, run to checkpoint A (MMIO write of 0xA11CE).
 *              Save EVERYTHING: X0-X30/PC/CPSR/FPCR/FPSR, Q0-Q31, the full EL1 sysreg
 *              list HVF exposes (per-reg rc recorded), GIC ICC regs, the GIC state blob
 *              (hv_gic_state_create/get_size/get_data), vtimer offset+mask, pending
 *              IRQ/FIQ, and a RAM snapshot. Then hv_vcpu_destroy + hv_vm_destroy.
 *   [restore]  VM #3: fresh hv_vm_create IN THE SAME PROCESS, fresh RAM mapping filled
 *              from the snapshot, fresh GIC (same config), hv_gic_set_state(blob),
 *              fresh vCPU, SET every saved piece back (every rejecting call recorded
 *              with its error code), deliberately sleep 150ms so timer #1 (armed
 *              pre-snapshot) expires INSIDE the gap, then run to checkpoint B.
 *
 * Verdict: GREEN iff restore reaches DONE with checksum == control checksum and
 * irq_count == control irq_count (and no guest-side ERR write).
 *
 * Build/run/sign: build.sh (needs com.apple.security.hypervisor — hv.entitlements).
 */

#include <Hypervisor/Hypervisor.h>
#include <os/object.h>

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
#define SEED 0x5EED0001ULL
#define BOOT_CPSR 0x3C5ULL /* EL1h, DAIF masked — libkrun PSTATE_EL1_FAULT_BITS_64 */

/* libkrun in-kernel GICv3 layout (hvfgicv3.rs:89-101): dist/redist packed just below
 * MMIO_MEM_START = 0x0a000000. Sizes asserted at runtime against hv_gic_get_*_size. */
#define MMIO_MEM_START 0x0a000000ULL
#define GICD_BASE 0x09fd0000ULL
#define GICR_BASE 0x09fe0000ULL

/* Guest checkpoint MMIO page (unmapped IPA -> data-abort exits) */
#define MMIO_BASE 0x10000000ULL
#define M_CKPT_A 0x00
#define M_FINAL_SUM 0x08
#define M_IRQ_COUNT 0x10
#define M_DONE 0x18
#define M_INTID 0x20
#define M_ERR 0x28
#define M_VCT_A 0x30
#define M_VCT_DELTA 0x38
#define M_VCT_FINAL 0x40

#define MAGIC_CKPT_A 0xA11CEULL
#define MAGIC_DONE 0xD05EULL

#define WATCHDOG_NS (20ULL * 1000 * 1000 * 1000)

/* ------------------------------------------------------------------ utils */

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

static uint64_t fnv1a(const uint8_t *p, size_t n) {
    uint64_t h = 0xcbf29ce484222325ULL;
    for (size_t i = 0; i < n; i++) { h ^= p[i]; h *= 0x100000001b3ULL; }
    return h;
}

/* ------------------------------------------------------------- watchdog */

static _Atomic uint64_t g_deadline_ns;
static _Atomic bool g_watch_on;
static hv_vcpu_t g_vcpu; /* only touched by main thread between phases */

static void *watchdog_main(void *arg) {
    (void)arg;
    for (;;) {
        usleep(100 * 1000);
        if (atomic_load(&g_watch_on) && now_ns() > atomic_load(&g_deadline_ns)) {
            hv_vcpu_t v = g_vcpu;
            hv_vcpus_exit(&v, 1); /* -> HV_EXIT_REASON_CANCELED in the run loop */
        }
    }
    return NULL;
}

/* ------------------------------------------------------------ reg tables */

typedef struct {
    hv_sys_reg_t reg;
    const char *name;
    int norestore; /* get for info only; never set on restore */
} sysreg_def_t;

#define R(n) { HV_SYS_REG_##n, #n, 0 }
#define RI(n) { HV_SYS_REG_##n, #n, 1 }
#define DBG4(i) R(DBGBVR##i##_EL1), R(DBGBCR##i##_EL1), R(DBGWVR##i##_EL1), R(DBGWCR##i##_EL1)

static const sysreg_def_t SYSREGS[] = {
    /* debug */
    DBG4(0), DBG4(1), DBG4(2), DBG4(3), DBG4(4), DBG4(5), DBG4(6), DBG4(7),
    DBG4(8), DBG4(9), DBG4(10), DBG4(11), DBG4(12), DBG4(13), DBG4(14), DBG4(15),
    R(MDCCINT_EL1), R(MDSCR_EL1),
    /* identification (writable pre-run per libkrun's MPIDR/PFR writes; record rc) */
    R(MIDR_EL1), R(MPIDR_EL1),
    R(ID_AA64PFR0_EL1), R(ID_AA64PFR1_EL1), R(ID_AA64ZFR0_EL1), R(ID_AA64SMFR0_EL1),
    R(ID_AA64DFR0_EL1), R(ID_AA64DFR1_EL1), R(ID_AA64ISAR0_EL1), R(ID_AA64ISAR1_EL1),
    R(ID_AA64MMFR0_EL1), R(ID_AA64MMFR1_EL1), R(ID_AA64MMFR2_EL1),
    /* control/translation */
    R(SCTLR_EL1), R(ACTLR_EL1), R(CPACR_EL1), R(TTBR0_EL1), R(TTBR1_EL1), R(TCR_EL1),
    /* pointer-auth keys */
    R(APIAKEYLO_EL1), R(APIAKEYHI_EL1), R(APIBKEYLO_EL1), R(APIBKEYHI_EL1),
    R(APDAKEYLO_EL1), R(APDAKEYHI_EL1), R(APDBKEYLO_EL1), R(APDBKEYHI_EL1),
    R(APGAKEYLO_EL1), R(APGAKEYHI_EL1),
    /* exception/fault state */
    R(SPSR_EL1), R(ELR_EL1), R(SP_EL0), R(SP_EL1),
    R(AFSR0_EL1), R(AFSR1_EL1), R(ESR_EL1), R(FAR_EL1), R(PAR_EL1),
    /* memory attributes */
    R(MAIR_EL1), R(AMAIR_EL1),
    /* misc EL1/EL0 */
    R(VBAR_EL1), R(CONTEXTIDR_EL1), R(TPIDR_EL1), R(CNTKCTL_EL1), R(CSSELR_EL1),
    R(TPIDR_EL0), R(TPIDRRO_EL0),
    /* timers: CVAL is the canonical state; TVAL is derived (never restore it) */
    R(CNTV_CTL_EL0), R(CNTV_CVAL_EL0), R(CNTP_CTL_EL0), R(CNTP_CVAL_EL0),
    RI(CNTP_TVAL_EL0),
    /* EL2 probes — expected to fail without nested virt; informational */
    RI(CNTVOFF_EL2), RI(CNTHCTL_EL2),
};
#define NSYS ((int)(sizeof(SYSREGS) / sizeof(SYSREGS[0])))

typedef struct {
    hv_gic_icc_reg_t reg;
    const char *name;
} icc_def_t;
#define IC(n) { HV_GIC_ICC_REG_##n, "ICC_" #n }
static const icc_def_t ICCREGS[] = {
    IC(PMR_EL1), IC(BPR0_EL1), IC(AP0R0_EL1), IC(AP1R0_EL1), IC(RPR_EL1),
    IC(BPR1_EL1), IC(CTLR_EL1), IC(SRE_EL1), IC(IGRPEN0_EL1), IC(IGRPEN1_EL1),
    IC(SRE_EL2),
};
#define NICC ((int)(sizeof(ICCREGS) / sizeof(ICCREGS[0])))

/* ------------------------------------------------------------- snapshot */

typedef struct {
    uint64_t x[31], pc, cpsr, fpcr, fpsr;
    hv_simd_fp_uchar16_t q[32];
    uint64_t sys[NSYS];
    hv_return_t sys_get_rc[NSYS];
    uint64_t icc[NICC];
    hv_return_t icc_get_rc[NICC];
    uint64_t vtimer_offset;
    bool vtimer_mask;
    bool pend_irq, pend_fiq;
    size_t gic_size;
    uint8_t *gic_blob;
    uint8_t *ram;
} snap_t;

/* -------------------------------------------------------- guest results */

typedef struct {
    bool ckpt_a, done, err;
    uint64_t final_sum, irq_count, err_val;
    uint64_t vct_a, vct_delta, vct_final;
    uint64_t intids[16];
    int n_intid;
    int vtimer_exits;
    int unknown_mmio;
} run_state_t;

/* --------------------------------------------------------------- VM bits */

static void *g_ram; /* current VM's mapped RAM */

static void gic_create(void) {
    size_t dist_size = 0, redist_size = 0;
    CHECK(hv_gic_get_distributor_size(&dist_size));
    CHECK(hv_gic_get_redistributor_size(&redist_size));
    /* libkrun formula (hvfgicv3.rs:89-90), 1 vCPU */
    uint64_t dist_addr = MMIO_MEM_START - dist_size - redist_size;
    uint64_t redist_addr = MMIO_MEM_START - redist_size;
    if (dist_addr != GICD_BASE || redist_addr != GICR_BASE) {
        fprintf(stderr, "FATAL: GIC layout mismatch: dist=0x%llx redist=0x%llx "
                        "(payload baked 0x%llx/0x%llx); sizes dist=0x%zx redist=0x%zx\n",
                dist_addr, redist_addr, GICD_BASE, GICR_BASE, dist_size, redist_size);
        exit(1);
    }
    hv_gic_config_t cfg = hv_gic_config_create();
    CHECK(hv_gic_config_set_distributor_base(cfg, dist_addr));
    CHECK(hv_gic_config_set_redistributor_base(cfg, redist_addr));
    CHECK(hv_gic_create(cfg));
    os_release(cfg);
}

static void vm_create_with_gic(const char *tag) {
    hv_return_t r = hv_vm_create(NULL);
    printf("[%s] hv_vm_create -> 0x%x (%s)\n", tag, (uint32_t)r, hv_err_name(r));
    if (r != HV_SUCCESS) {
        /* the same-process-re-create question: record precisely, then give up */
        fprintf(stderr, "[%s] SAME-PROCESS hv_vm_create FAILED: 0x%x (%s) — a "
                        "fork/exec restore path would be required\n",
                tag, (uint32_t)r, hv_err_name(r));
        exit(3);
    }
    gic_create();
}

static void map_ram(const uint8_t *payload, size_t payload_len, const uint8_t *restore_from) {
    g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (g_ram == MAP_FAILED) { perror("mmap"); exit(1); }
    if (restore_from)
        memcpy(g_ram, restore_from, RAM_SIZE);
    else
        memcpy(g_ram, payload, payload_len);
    CHECK(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
}

static void unmap_ram(void) {
    hv_vm_unmap(RAM_BASE, RAM_SIZE);
    munmap(g_ram, RAM_SIZE);
    g_ram = NULL;
}

/* ------------------------------------------------------------- run loop */

typedef enum { RUN_DONE, RUN_CKPT_A, RUN_FAIL } run_result_t;

static bool g_vtimer_masked;

static void vtimer_sync(hv_vcpu_t vcpu) {
    /* libkrun's hvf_sync_vtimer: unmask once the guest disarmed/EOI'd (CNTV_CTL
     * no longer ISTATUS|ENABLE). */
    if (!g_vtimer_masked) return;
    uint64_t ctl = 0;
    CHECK(hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_CNTV_CTL_EL0, &ctl));
    if ((ctl & 0b101) != 0b101) {
        CHECK(hv_vcpu_set_vtimer_mask(vcpu, false));
        g_vtimer_masked = false;
    }
}

static run_result_t run_guest(const char *tag, hv_vcpu_t vcpu, hv_vcpu_exit_t *vexit,
                              run_state_t *st, bool stop_at_ckpt_a) {
    atomic_store(&g_deadline_ns, now_ns() + WATCHDOG_NS);
    atomic_store(&g_watch_on, true);

    for (;;) {
        CHECK(hv_vcpu_run(vcpu));

        switch (vexit->reason) {
        case HV_EXIT_REASON_CANCELED: {
            uint64_t pc = 0, x27 = 0, ctl = 0, cval = 0;
            hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
            hv_vcpu_get_reg(vcpu, HV_REG_X27, &x27);
            hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_CNTV_CTL_EL0, &ctl);
            hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_CNTV_CVAL_EL0, &cval);
            fprintf(stderr,
                    "[%s] WATCHDOG: guest made no progress; pc=0x%llx irqs(x27)=%llu "
                    "cntv_ctl=0x%llx cntv_cval=0x%llx vtimer_exits=%d\n",
                    tag, pc, x27, ctl, cval, st->vtimer_exits);
            atomic_store(&g_watch_on, false);
            return RUN_FAIL;
        }
        case HV_EXIT_REASON_VTIMER_ACTIVATED:
            st->vtimer_exits++;
            g_vtimer_masked = true; /* HVF masked it; unmask after guest disarms */
            vtimer_sync(vcpu);
            continue;
        case HV_EXIT_REASON_EXCEPTION:
            break;
        default:
            fprintf(stderr, "[%s] unexpected exit reason %u\n", tag, vexit->reason);
            atomic_store(&g_watch_on, false);
            return RUN_FAIL;
        }

        uint64_t syn = vexit->exception.syndrome;
        uint64_t ec = (syn >> 26) & 0x3f;
        uint64_t pa = vexit->exception.physical_address;

        if (ec == 0x24 || ec == 0x25) { /* data abort (lower/same EL) */
            bool iswrite = (syn >> 6) & 1;
            uint32_t srt = (syn >> 16) & 0x1f;
            uint32_t sas = (syn >> 22) & 3;
            (void)sas;

            uint64_t pc = 0;
            CHECK(hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc));
            CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4)); /* advance past the access */

            if (!iswrite) {
                if (srt < 31) CHECK(hv_vcpu_set_reg(vcpu, HV_REG_X0 + srt, 0));
                if (pa < MMIO_BASE || pa >= MMIO_BASE + 0x1000) st->unknown_mmio++;
                vtimer_sync(vcpu);
                continue;
            }

            uint64_t val = 0;
            if (srt < 31) CHECK(hv_vcpu_get_reg(vcpu, HV_REG_X0 + srt, &val));

            if (pa >= MMIO_BASE && pa < MMIO_BASE + 0x1000) {
                switch (pa - MMIO_BASE) {
                case M_CKPT_A:
                    st->ckpt_a = true;
                    printf("[%s] checkpoint A (magic 0x%llx)\n", tag, val);
                    if (stop_at_ckpt_a) {
                        atomic_store(&g_watch_on, false);
                        return RUN_CKPT_A; /* PC already advanced past the store */
                    }
                    break;
                case M_FINAL_SUM: st->final_sum = val; break;
                case M_IRQ_COUNT: st->irq_count = val; break;
                case M_DONE:
                    st->done = true;
                    printf("[%s] checkpoint B/DONE (magic 0x%llx)\n", tag, val);
                    atomic_store(&g_watch_on, false);
                    return RUN_DONE;
                case M_INTID:
                    if (st->n_intid < 16) st->intids[st->n_intid++] = val;
                    printf("[%s] guest IRQ handler: intid=%llu\n", tag, val);
                    break;
                case M_ERR:
                    st->err = true;
                    st->err_val = val;
                    fprintf(stderr, "[%s] GUEST ERROR write: 0x%llx (sync-exception ESR "
                                    "or 0xBAD7 = CNTVCT went backwards)\n",
                            tag, val);
                    atomic_store(&g_watch_on, false);
                    return RUN_FAIL;
                case M_VCT_A: st->vct_a = val; break;
                case M_VCT_DELTA:
                    st->vct_delta = val;
                    printf("[%s] guest CNTVCT delta across checkpoint A: %llu ticks (%.1f ms "
                           "@24MHz)\n",
                           tag, val, (double)val / 24000.0);
                    break;
                case M_VCT_FINAL: st->vct_final = val; break;
                default:
                    st->unknown_mmio++;
                    break;
                }
            } else {
                st->unknown_mmio++;
                printf("[%s] unexpected MMIO write pa=0x%llx val=0x%llx (GIC MMIO not "
                       "in-kernel?)\n",
                       tag, pa, val);
            }
            vtimer_sync(vcpu);
            continue;
        }

        if (ec == 0x01) { /* WFx — payload has none, but be graceful */
            uint64_t pc = 0;
            CHECK(hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc));
            CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4));
            vtimer_sync(vcpu);
            continue;
        }

        uint64_t pc = 0;
        hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc);
        fprintf(stderr, "[%s] unhandled exception: ec=0x%llx syndrome=0x%llx pc=0x%llx pa=0x%llx\n",
                tag, ec, syn, pc, pa);
        atomic_store(&g_watch_on, false);
        return RUN_FAIL;
    }
}

/* ------------------------------------------------------------ save state */

static void save_state(const char *tag, hv_vcpu_t vcpu, snap_t *s) {
    for (int i = 0; i < 31; i++) CHECK(hv_vcpu_get_reg(vcpu, HV_REG_X0 + i, &s->x[i]));
    CHECK(hv_vcpu_get_reg(vcpu, HV_REG_PC, &s->pc));
    CHECK(hv_vcpu_get_reg(vcpu, HV_REG_CPSR, &s->cpsr));
    CHECK(hv_vcpu_get_reg(vcpu, HV_REG_FPCR, &s->fpcr));
    CHECK(hv_vcpu_get_reg(vcpu, HV_REG_FPSR, &s->fpsr));
    for (int i = 0; i < 32; i++)
        CHECK(hv_vcpu_get_simd_fp_reg(vcpu, HV_SIMD_FP_REG_Q0 + i, &s->q[i]));

    int nfail = 0;
    for (int i = 0; i < NSYS; i++) {
        s->sys[i] = 0;
        s->sys_get_rc[i] = hv_vcpu_get_sys_reg(vcpu, SYSREGS[i].reg, &s->sys[i]);
        if (s->sys_get_rc[i] != HV_SUCCESS) {
            printf("[%s] get_sys_reg %-18s -> 0x%x (%s)\n", tag, SYSREGS[i].name,
                   (uint32_t)s->sys_get_rc[i], hv_err_name(s->sys_get_rc[i]));
            nfail++;
        }
    }
    printf("[%s] sysregs read: %d ok, %d failed (of %d)\n", tag, NSYS - nfail, nfail, NSYS);

    for (int i = 0; i < NICC; i++) {
        s->icc[i] = 0;
        s->icc_get_rc[i] = hv_gic_get_icc_reg(vcpu, ICCREGS[i].reg, &s->icc[i]);
        if (s->icc_get_rc[i] != HV_SUCCESS)
            printf("[%s] gic_get_icc %-16s -> 0x%x (%s)\n", tag, ICCREGS[i].name,
                   (uint32_t)s->icc_get_rc[i], hv_err_name(s->icc_get_rc[i]));
    }

    CHECK(hv_vcpu_get_vtimer_offset(vcpu, &s->vtimer_offset));
    CHECK(hv_vcpu_get_vtimer_mask(vcpu, &s->vtimer_mask));
    CHECK(hv_vcpu_get_pending_interrupt(vcpu, HV_INTERRUPT_TYPE_IRQ, &s->pend_irq));
    CHECK(hv_vcpu_get_pending_interrupt(vcpu, HV_INTERRUPT_TYPE_FIQ, &s->pend_fiq));
    printf("[%s] vtimer_offset=0x%llx mask=%d pending_irq=%d pending_fiq=%d\n", tag,
           s->vtimer_offset, s->vtimer_mask, s->pend_irq, s->pend_fiq);
    printf("[%s] pc=0x%llx cpsr=0x%llx fpcr=0x%llx fpsr=0x%llx\n", tag, s->pc, s->cpsr,
           s->fpcr, s->fpsr);
    for (int i = 0; i < NSYS; i++)
        if (s->sys_get_rc[i] == HV_SUCCESS)
            printf("[%s]   %-18s = 0x%016llx\n", tag, SYSREGS[i].name, s->sys[i]);
    for (int i = 0; i < NICC; i++)
        if (s->icc_get_rc[i] == HV_SUCCESS)
            printf("[%s]   %-18s = 0x%016llx\n", tag, ICCREGS[i].name, s->icc[i]);

    /* GIC state blob. Header: "the VM must be in a stopped state" — no vCPU is
     * running (we're between hv_vcpu_run calls); record whether that suffices. */
    hv_gic_state_t gst = hv_gic_state_create();
    if (gst == NULL) {
        printf("[%s] hv_gic_state_create -> NULL with a live (not-running) vCPU\n", tag);
        s->gic_size = 0;
        s->gic_blob = NULL;
        return;
    }
    CHECK(hv_gic_state_get_size(gst, &s->gic_size));
    s->gic_blob = malloc(s->gic_size);
    CHECK(hv_gic_state_get_data(gst, s->gic_blob));
    os_release(gst);
    printf("[%s] GIC state blob: %zu bytes, fnv1a=0x%016llx, head=", tag, s->gic_size,
           fnv1a(s->gic_blob, s->gic_size));
    for (size_t i = 0; i < 16 && i < s->gic_size; i++) printf("%02x", s->gic_blob[i]);
    printf("\n");

    /* RAM */
    s->ram = malloc(RAM_SIZE);
    memcpy(s->ram, g_ram, RAM_SIZE);
    printf("[%s] RAM snapshot: %llu MiB, fnv1a(first 1MiB)=0x%016llx\n", tag,
           RAM_SIZE >> 20, fnv1a(s->ram, 1 << 20));
}

/* --------------------------------------------------------- restore state */

static int g_restore_set_failures;

static void try_set_sys(const char *tag, hv_vcpu_t vcpu, const sysreg_def_t *d, uint64_t val) {
    hv_return_t r = hv_vcpu_set_sys_reg(vcpu, d->reg, val);
    if (r != HV_SUCCESS) {
        printf("[%s] SET REJECTED %-18s val=0x%llx -> 0x%x (%s)\n", tag, d->name, val,
               (uint32_t)r, hv_err_name(r));
        g_restore_set_failures++;
    }
}

static void restore_state(const char *tag, hv_vcpu_t vcpu, const snap_t *s) {
    g_restore_set_failures = 0;

    /* MPIDR first: the in-kernel GIC matches redistributors by MPIDR (libkrun
     * lib.rs:384-389), so identity must be in place before GIC/ICC restore. */
    for (int i = 0; i < NSYS; i++)
        if (SYSREGS[i].reg == HV_SYS_REG_MPIDR_EL1 && s->sys_get_rc[i] == HV_SUCCESS)
            try_set_sys(tag, vcpu, &SYSREGS[i], s->sys[i]);

    /* GIC distributor/redistributor state blob */
    if (s->gic_blob) {
        hv_return_t r = hv_gic_set_state(s->gic_blob, s->gic_size);
        printf("[%s] hv_gic_set_state(%zu bytes) -> 0x%x (%s)\n", tag, s->gic_size, (uint32_t)r,
               hv_err_name(r));
        if (r != HV_SUCCESS) g_restore_set_failures++;
    }

    /* GIC CPU-interface regs (explicitly NOT in the blob per hv_gic_state.h) */
    for (int i = 0; i < NICC; i++) {
        if (s->icc_get_rc[i] != HV_SUCCESS) continue;
        hv_return_t r = hv_gic_set_icc_reg(vcpu, ICCREGS[i].reg, s->icc[i]);
        if (r != HV_SUCCESS) {
            printf("[%s] SET REJECTED %-16s val=0x%llx -> 0x%x (%s)\n", tag, ICCREGS[i].name,
                   s->icc[i], (uint32_t)r, hv_err_name(r));
            g_restore_set_failures++;
        }
    }

    /* system registers */
    for (int i = 0; i < NSYS; i++) {
        if (SYSREGS[i].norestore || s->sys_get_rc[i] != HV_SUCCESS) continue;
        if (SYSREGS[i].reg == HV_SYS_REG_MPIDR_EL1) continue; /* done above */
        try_set_sys(tag, vcpu, &SYSREGS[i], s->sys[i]);
    }

    /* GP / PC / PSTATE / FP */
    for (int i = 0; i < 31; i++) CHECK(hv_vcpu_set_reg(vcpu, HV_REG_X0 + i, s->x[i]));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, s->pc));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, s->cpsr));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_FPCR, s->fpcr));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_FPSR, s->fpsr));
    for (int i = 0; i < 32; i++)
        CHECK(hv_vcpu_set_simd_fp_reg(vcpu, HV_SIMD_FP_REG_Q0 + i, s->q[i]));

    /* vtimer + pending interrupt lines */
    {
        hv_return_t r = hv_vcpu_set_vtimer_offset(vcpu, s->vtimer_offset);
        printf("[%s] hv_vcpu_set_vtimer_offset(0x%llx) -> 0x%x (%s)\n", tag, s->vtimer_offset,
               (uint32_t)r, hv_err_name(r));
        if (r != HV_SUCCESS) g_restore_set_failures++;
        CHECK(hv_vcpu_set_vtimer_mask(vcpu, s->vtimer_mask));
        g_vtimer_masked = s->vtimer_mask;
        CHECK(hv_vcpu_set_pending_interrupt(vcpu, HV_INTERRUPT_TYPE_IRQ, s->pend_irq));
        CHECK(hv_vcpu_set_pending_interrupt(vcpu, HV_INTERRUPT_TYPE_FIQ, s->pend_fiq));
    }

    /* verify pass: catch silent drops (set returned success but value didn't stick) */
    int mismatches = 0;
    for (int i = 0; i < NSYS; i++) {
        if (SYSREGS[i].norestore || s->sys_get_rc[i] != HV_SUCCESS) continue;
        uint64_t v = 0;
        if (hv_vcpu_get_sys_reg(vcpu, SYSREGS[i].reg, &v) != HV_SUCCESS) continue;
        if (v != s->sys[i]) {
            printf("[%s] VERIFY MISMATCH %-18s saved=0x%llx readback=0x%llx\n", tag,
                   SYSREGS[i].name, s->sys[i], v);
            mismatches++;
        }
    }
    for (int i = 0; i < NICC; i++) {
        if (s->icc_get_rc[i] != HV_SUCCESS) continue;
        uint64_t v = 0;
        if (hv_gic_get_icc_reg(vcpu, ICCREGS[i].reg, &v) != HV_SUCCESS) continue;
        if (v != s->icc[i]) {
            printf("[%s] VERIFY MISMATCH %-18s saved=0x%llx readback=0x%llx\n", tag,
                   ICCREGS[i].name, s->icc[i], v);
            mismatches++;
        }
    }
    /* GIC blob readback compare */
    if (s->gic_blob) {
        hv_gic_state_t gst = hv_gic_state_create();
        if (gst) {
            size_t sz = 0;
            CHECK(hv_gic_state_get_size(gst, &sz));
            uint8_t *buf = malloc(sz);
            CHECK(hv_gic_state_get_data(gst, buf));
            if (sz == s->gic_size && memcmp(buf, s->gic_blob, sz) == 0)
                printf("[%s] GIC blob readback: IDENTICAL (%zu bytes)\n", tag, sz);
            else {
                size_t diff = 0;
                for (size_t i = 0; i < sz && i < s->gic_size; i++)
                    if (buf[i] != s->gic_blob[i]) diff++;
                printf("[%s] GIC blob readback: differs (size %zu vs %zu, %zu differing "
                       "bytes) — may be benign\n",
                       tag, sz, s->gic_size, diff);
            }
            free(buf);
            os_release(gst);
        } else {
            printf("[%s] post-restore hv_gic_state_create -> NULL\n", tag);
        }
    }
    printf("[%s] restore complete: %d set-call failures, %d readback mismatches\n", tag,
           g_restore_set_failures, mismatches);
}

/* ------------------------------------------------------------------ main */

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

    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *vexit;
    run_state_t control = {0}, vm1 = {0}, restored = {0};
    snap_t snap = {0};

    /* ---------------- phase 1: control (uninterrupted reference run) -------- */
    printf("\n=== phase 1: control run (VM #1, uninterrupted) ===\n");
    vm_create_with_gic("control");
    map_ram(payload, payload_len, NULL);
    CHECK(hv_vcpu_create(&vcpu, &vexit, NULL));
    g_vcpu = vcpu;
    g_vtimer_masked = false;
    CHECK(hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_MPIDR_EL1, 0)); /* like libkrun */
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, BOOT_CPSR));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_X0, SEED));
    run_result_t cres = run_guest("control", vcpu, vexit, &control, false);
    printf("[control] result=%d sum=0x%016llx irqs=%llu vtimer_exits=%d unknown_mmio=%d\n",
           cres, control.final_sum, control.irq_count, control.vtimer_exits,
           control.unknown_mmio);
    CHECK(hv_vcpu_destroy(vcpu));
    unmap_ram();
    CHECK(hv_vm_destroy());
    if (cres != RUN_DONE || control.irq_count == 0) {
        fprintf(stderr, "control run failed — vehicle broken, no verdict possible\n");
        return 2;
    }

    /* ---------------- phase 2: run to checkpoint A and save ----------------- */
    printf("\n=== phase 2: snapshot run (VM #2, stop at checkpoint A) ===\n");
    vm_create_with_gic("vm1");
    map_ram(payload, payload_len, NULL);
    CHECK(hv_vcpu_create(&vcpu, &vexit, NULL));
    g_vcpu = vcpu;
    g_vtimer_masked = false;
    CHECK(hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_MPIDR_EL1, 0));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, BOOT_CPSR));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE));
    CHECK(hv_vcpu_set_reg(vcpu, HV_REG_X0, SEED));
    run_result_t r1 = run_guest("vm1", vcpu, vexit, &vm1, true);
    if (r1 != RUN_CKPT_A) {
        fprintf(stderr, "vm1 did not reach checkpoint A (result=%d)\n", r1);
        return 2;
    }
    save_state("vm1", vcpu, &snap);
    uint64_t teardown_t0 = now_ns();
    CHECK(hv_vcpu_destroy(vcpu));
    unmap_ram();
    CHECK(hv_vm_destroy());
    printf("[vm1] VM torn down (vcpu_destroy + vm_destroy OK)\n");

    /* ---------------- phase 3: fresh VM, restore, continue ------------------ */
    printf("\n=== phase 3: restore into a FRESH VM (VM #3, same process) ===\n");
    vm_create_with_gic("restore");
    map_ram(NULL, 0, snap.ram);
    CHECK(hv_vcpu_create(&vcpu, &vexit, NULL));
    g_vcpu = vcpu;
    restore_state("restore", vcpu, &snap);
    printf("[restore] teardown+rebuild+restore wall time: %.1f ms\n",
           (double)(now_ns() - teardown_t0) / 1e6);

    /* force the interesting case: let timer #1 (armed pre-snapshot, ~125ms)
     * expire INSIDE the host-side gap, so it must fire from restored state */
    usleep(150 * 1000);
    printf("[restore] slept 150ms — timer #1's CVAL is now in the past; resuming\n");

    run_result_t r2 = run_guest("restore", vcpu, vexit, &restored, false);
    printf("[restore] result=%d sum=0x%016llx irqs=%llu vtimer_exits=%d unknown_mmio=%d\n",
           r2, restored.final_sum, restored.irq_count, restored.vtimer_exits,
           restored.unknown_mmio);
    CHECK(hv_vcpu_destroy(vcpu));
    unmap_ram();
    CHECK(hv_vm_destroy());

    /* ---------------- verdict ------------------------------------------------ */
    printf("\n=== verdict ===\n");
    printf("control : sum=0x%016llx irqs=%llu intids:", control.final_sum, control.irq_count);
    for (int i = 0; i < control.n_intid; i++) printf(" %llu", control.intids[i]);
    printf("\nrestored: sum=0x%016llx irqs=%llu intids:", restored.final_sum, restored.irq_count);
    for (int i = 0; i < restored.n_intid; i++) printf(" %llu", restored.intids[i]);
    printf("\n");

    bool green = (r2 == RUN_DONE) && restored.done && !restored.err &&
                 restored.final_sum == control.final_sum &&
                 restored.irq_count == control.irq_count && restored.irq_count > 0;
    if (green) {
        printf("VERDICT: GREEN — full vCPU+GIC state round-tripped into a fresh "
               "same-process VM; guest continued from checkpoint A to B with the "
               "correct checksum and %llu timer interrupts\n",
               restored.irq_count);
        return 0;
    }
    printf("VERDICT: RED/PARTIAL — see mismatches above (result=%d done=%d err=%d "
           "sum_match=%d irq_match=%d)\n",
           r2, restored.done, restored.err, restored.final_sum == control.final_sum,
           restored.irq_count == control.irq_count);
    return 1;
}
