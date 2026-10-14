// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * el2-timer-irq — which generic-timer interrupts reach a vCPU running at EL2?
 *
 * The firmware runs at EL2 when nested virtualization is on, and every UEFI event (the keyboard
 * drivers poll from them) hangs off its timer tick. This probe runs a few dozen instructions at
 * EL2 that set up HVF's GIC and the CPU interface, arm ONE timer, unmask IRQs and WFI. The IRQ
 * vector acknowledges the interrupt and reports its INTID through an MMIO write. A timer whose
 * interrupt never arrives within the deadline is reported with its CTL (ISTATUS) and the
 * redistributor's pending bits, so "the timer never fired" and "it fired but was not delivered"
 * read differently.
 *
 * Timers: v = EL1 virtual (CNTV, INTID 27), p = EL1 physical (CNTP, 30), hp = EL2 physical
 * (CNTHP, 26). HCR_EL2 starts as libkrun sets it (`krun`) or as only RW (`min`).
 *
 * Build/run: build-el2-timer-irq.sh (ad-hoc signed with the hypervisor entitlement). Needs M3+.
 */

#include <Hypervisor/Hypervisor.h>
#include <os/object.h>

#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define RAM_BASE 0x80000000ULL
#define RAM_SIZE (16ULL << 20)
#define MMIO_MEM_START 0x0a000000ULL
#define REPORT_PA 0x10000000ULL
#define CPSR_EL2H 0x3C9ULL /* EL2h, DAIF masked — libkrun PSTATE_EL2_FAULT_BITS_64 */

/* libkrun's HCR_EL2_BITS and CNTHCTL_EL2_BITS (third_party/libkrun/src/hvf/src/lib.rs) */
#define HCR_KRUN                                                                                 \
    ((1ULL << 35) | (1ULL << 31) | (1ULL << 22) | (1ULL << 21) | (1ULL << 20) | (1ULL << 19) | \
     (1ULL << 18) | (1ULL << 14) | (1ULL << 13) | (1ULL << 10) | (1ULL << 9) | (1ULL << 5) |   \
     (1ULL << 4) | (1ULL << 3) | (1ULL << 2) | (1ULL << 1) | (1ULL << 0))
#define HCR_MIN (1ULL << 31)
#define HCR_TGE (1ULL << 27) /* what the firmware's PrePi writes at EL2 (ArchPrePi.c) */
#define CNTHCTL_KRUN 0x3ULL

/*
 * The guest payload, position-independent, copied byte for byte into guest RAM at RAM_BASE.
 * In: x1 = GICD base, x2 = this vCPU's GICR base, x5 = INTID (a PPI), x6 = timer (0 v, 1 p, 2 hp).
 * Out: an MMIO write of the acknowledged INTID to REPORT_PA from the IRQ vector; x9 counts WFI
 * wake-ups that were not the interrupt.
 */
extern const uint8_t payload_start[], payload_end[];
__asm__(".text\n"
        ".p2align 11\n"
        ".globl _payload_start\n"
        "_payload_start:\n"
        "  adr x0, 9f\n"
        "  msr vbar_el2, x0\n"
        "  mov w3, #0x13\n"            /* GICD_CTLR: ARE | EnableGrp1 | EnableGrp0 */
        "  str w3, [x1]\n"
        "  ldr w3, [x2, #0x14]\n"      /* GICR_WAKER: clear ProcessorSleep */
        "  bic w3, w3, #2\n"
        "  str w3, [x2, #0x14]\n"
        "  add x4, x2, #0x10, lsl #12\n" /* SGI/PPI frame */
        "  mov w3, #-1\n"
        "  str w3, [x4, #0x80]\n"      /* GICR_IGROUPR0: all group 1 */
        "  mov w3, #1\n"
        "  lsl w3, w3, w5\n"
        "  str w3, [x4, #0x100]\n"     /* GICR_ISENABLER0 */
        "  mrs x3, icc_sre_el2\n"
        "  mov x4, #0x9\n"              /* SRE | Enable */
        "  orr x3, x3, x4\n"
        "  msr icc_sre_el2, x3\n"
        "  isb\n"
        "  mov x3, #0xff\n"
        "  msr icc_pmr_el1, x3\n"
        "  mov x3, #1\n"
        "  msr icc_igrpen1_el1, x3\n"
        "  isb\n"
        "  mov x8, #0x10000\n"         /* deadline: ~2.7 ms at 24 MHz */
        "  cmp x6, #1\n"
        "  b.eq 2f\n"
        "  b.gt 3f\n"
        "  mrs x7, cntvct_el0\n"
        "  add x7, x7, x8\n"
        "  msr cntv_cval_el0, x7\n"
        "  mov x3, #1\n"
        "  msr cntv_ctl_el0, x3\n"
        "  b 4f\n"
        "2:\n"
        "  mrs x7, cntpct_el0\n"
        "  add x7, x7, x8\n"
        "  msr cntp_cval_el0, x7\n"
        "  mov x3, #1\n"
        "  msr cntp_ctl_el0, x3\n"
        "  b 4f\n"
        "3:\n"
        "  mrs x7, cntpct_el0\n"
        "  add x7, x7, x8\n"
        "  msr cnthp_cval_el2, x7\n"
        "  mov x3, #1\n"
        "  msr cnthp_ctl_el2, x3\n"
        "4:\n"
        "  isb\n"
        "  mov x9, #0\n"
        "  msr daifclr, #2\n"
        "5:\n"
        "  wfi\n"
        "  add x9, x9, #1\n"
        "  b 5b\n"
        ".p2align 11\n"
        "9:\n"                         /* VBAR_EL2; current EL with SPx, IRQ at +0x280 */
        "  .skip 0x280\n"
        "  mrs x10, icc_iar1_el1\n"
        "  mov x11, #0x10000000\n"
        "  str x10, [x11]\n"
        "  b .\n"
        ".globl _payload_end\n"
        "_payload_end:\n");

static const char *rc_name(hv_return_t r) {
    switch ((uint32_t)r) {
    case 0: return "ok";
    case 0xfae94001: return "HV_ERROR";
    case 0xfae94003: return "HV_BAD_ARGUMENT";
    case 0xfae94007: return "HV_DENIED";
    case 0xfae9400f: return "HV_UNSUPPORTED";
    default: return "HV_???";
    }
}

#define CHECK(expr)                                                                     \
    do {                                                                                \
        hv_return_t _r = (expr);                                                        \
        if (_r != HV_SUCCESS) {                                                         \
            fprintf(stderr, "FATAL %s:%d %s -> 0x%x (%s)\n", __FILE__, __LINE__, #expr, \
                    (uint32_t)_r, rc_name(_r));                                         \
            exit(1);                                                                    \
        }                                                                               \
    } while (0)

static hv_vcpu_t g_vcpu;

static void *canceller(void *arg) {
    (void)arg;
    usleep(500 * 1000);
    hv_vcpus_exit(&g_vcpu, 1);
    return NULL;
}

/* One fresh VM per case: hv_vm_create is once per process at a time, so tear down fully. */
static bool run_case(const char *timer_name, int timer, uint32_t intid, const char *hcr_name,
                     uint64_t hcr) {
    hv_vm_config_t cfg = hv_vm_config_create();
    CHECK(hv_vm_config_set_el2_enabled(cfg, true));
    CHECK(hv_vm_create(cfg));
    os_release(cfg);
    size_t dist = 0, redist = 0;
    CHECK(hv_gic_get_distributor_size(&dist));
    CHECK(hv_gic_get_redistributor_size(&redist));
    uint64_t gicd = MMIO_MEM_START - dist - redist, gicr = MMIO_MEM_START - redist;
    hv_gic_config_t g = hv_gic_config_create();
    CHECK(hv_gic_config_set_distributor_base(g, gicd));
    CHECK(hv_gic_config_set_redistributor_base(g, gicr));
    CHECK(hv_gic_create(g));
    os_release(g);

    void *ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    memcpy(ram, payload_start, (size_t)(payload_end - payload_start));
    CHECK(hv_vm_map(ram, RAM_BASE, RAM_SIZE, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));

    hv_vcpu_exit_t *vexit;
    CHECK(hv_vcpu_create(&g_vcpu, &vexit, NULL));
    hv_vcpu_t v = g_vcpu;
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_MPIDR_EL1, 0));
    CHECK(hv_vcpu_set_reg(v, HV_REG_CPSR, CPSR_EL2H));
    CHECK(hv_vcpu_set_reg(v, HV_REG_PC, RAM_BASE));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_HCR_EL2, hcr));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_CNTHCTL_EL2, CNTHCTL_KRUN));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X1, gicd));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X2, gicr));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X5, intid));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X6, (uint64_t)timer));

    pthread_t t;
    pthread_create(&t, NULL, canceller, NULL);
    int vtimer_exits = 0, other_exits = 0;
    bool taken = false, done = false;
    uint64_t got = 0;
    while (!done) {
        CHECK(hv_vcpu_run(v));
        switch (vexit->reason) {
        case HV_EXIT_REASON_CANCELED: done = true; break;
        case HV_EXIT_REASON_VTIMER_ACTIVATED: vtimer_exits++; break;
        case HV_EXIT_REASON_EXCEPTION: {
            uint64_t esr = vexit->exception.syndrome, ec = (esr >> 26) & 0x3f;
            if (ec == 0x24 && vexit->exception.physical_address == REPORT_PA) {
                CHECK(hv_vcpu_get_reg(v, HV_REG_X10, &got));
                taken = true;
                done = true;
            } else {
                uint64_t pc = 0;
                hv_vcpu_get_reg(v, HV_REG_PC, &pc);
                printf("    exit ec=0x%llx esr=0x%llx pc=0x%llx pa=0x%llx\n", ec, esr, pc,
                       vexit->exception.physical_address);
                if (++other_exits > 8) done = true;
                CHECK(hv_vcpu_set_reg(v, HV_REG_PC, pc + 4));
            }
            break;
        }
        default:
            printf("    exit reason %u\n", vexit->reason);
            done = true;
        }
    }
    pthread_join(t, NULL);

    uint64_t wakes = 0, ctl = 0, pc = 0, cpsr = 0;
    hv_vcpu_get_reg(v, HV_REG_X9, &wakes);
    hv_vcpu_get_reg(v, HV_REG_PC, &pc);
    hv_vcpu_get_reg(v, HV_REG_CPSR, &cpsr);
    hv_sys_reg_t ctl_reg = timer == 0   ? HV_SYS_REG_CNTV_CTL_EL0
                           : timer == 1 ? HV_SYS_REG_CNTP_CTL_EL0
                                        : HV_SYS_REG_CNTHP_CTL_EL2;
    hv_return_t ctl_rc = hv_vcpu_get_sys_reg(v, ctl_reg, &ctl);
    uint64_t ispendr = 0, isenabler = 0;
    hv_return_t p_rc =
        hv_gic_get_redistributor_reg(v, HV_GIC_REDISTRIBUTOR_REG_GICR_ISPENDR0, &ispendr);
    hv_gic_get_redistributor_reg(v, HV_GIC_REDISTRIBUTOR_REG_GICR_ISENABLER0, &isenabler);

    if (taken)
        printf("%-3s INTID %u  HCR %-8s  IRQ TAKEN: acknowledged INTID %llu", timer_name, intid,
               hcr_name, got);
    else
        printf("%-3s INTID %u  HCR %-8s  no IRQ in 500 ms: CTL=0x%llx (%s, ISTATUS=%llu) "
               "pc=0x%llx cpsr=0x%llx wfi-wakes=%llu ISPENDR0=0x%llx (%s) ISENABLER0=0x%llx",
               timer_name, intid, hcr_name, ctl, rc_name(ctl_rc), (ctl >> 2) & 1, pc, cpsr,
               wakes, ispendr, rc_name(p_rc), isenabler);
    printf("  vtimer-exits=%d\n", vtimer_exits);

    CHECK(hv_vcpu_destroy(v));
    hv_vm_unmap(RAM_BASE, RAM_SIZE);
    munmap(ram, RAM_SIZE);
    CHECK(hv_vm_destroy());
    return taken;
}

int main(void) {
    bool supported = false;
    CHECK(hv_vm_config_get_el2_supported(&supported));
    if (!supported) {
        printf("no EL2 on this Mac\n");
        return 2;
    }
    uint32_t el2_timer = 0;
    CHECK(hv_gic_get_intid(HV_GIC_INT_EL2_PHYSICAL_TIMER, &el2_timer));
    printf("hv_gic_get_intid(EL2_PHYSICAL_TIMER) = %u\n", el2_timer);

    static const struct {
        const char *name;
        int timer;
        uint32_t intid;
    } timers[] = {{"v", 0, 27}, {"p", 1, 30}, {"hp", 2, 26}};
    static const struct {
        const char *name;
        uint64_t hcr;
    } hcrs[] = {{"krun", HCR_KRUN}, {"min", HCR_MIN}, {"tge", HCR_TGE},
                 {"tge+imo", HCR_TGE | (1ULL << 5) | (1ULL << 4) | (1ULL << 3)},
                 {"krun+tge", HCR_KRUN | HCR_TGE}};
    for (size_t h = 0; h < sizeof(hcrs) / sizeof(hcrs[0]); h++)
        for (size_t i = 0; i < 3; i++)
            run_case(timers[i].name, timers[i].timer, timers[i].intid, hcrs[h].name, hcrs[h].hcr);
    return 0;
}
