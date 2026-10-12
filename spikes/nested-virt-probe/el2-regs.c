// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * el2-regs — which EL2 / GIC-hypervisor registers does HVF round-trip with EL2 enabled?
 *
 * A snapshot of a nested VM must carry the guest hypervisor's state. libkrun's vCPU save list
 * (SNAPSHOT_SYS_REGS / SNAPSHOT_ICC_REGS) leaves out everything EL2 because it reads
 * HV_UNSUPPORTED without nesting. This probe answers, per register, with EL2 enabled:
 *   - does get succeed after the vCPU has run,
 *   - does set on a FRESH VM's vCPU accept the saved value,
 *   - does the value read back unchanged.
 * Distinctive values are planted first, so a register that silently ignores writes shows up as
 * a mismatch rather than a zero-equals-zero pass. It also asks how CNTVOFF_EL2 relates to
 * hv_vcpu_{get,set}_vtimer_offset, since libkrun restores the latter, and records the GIC state
 * blob's size with EL2 on.
 *
 * Build/run: build-el2-regs.sh (ad-hoc signed with the hypervisor entitlement). Needs M3+.
 */

#include <Hypervisor/Hypervisor.h>
#include <os/object.h>

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#define RAM_BASE 0x80000000ULL
#define RAM_SIZE (16ULL << 20)
#define MMIO_MEM_START 0x0a000000ULL
#define CPSR_EL2H 0x3C9ULL /* EL2h, DAIF masked — libkrun PSTATE_EL2_FAULT_BITS_64 */

/* movz x1, #0x1000, lsl #16 ; str x0, [x1] ; b .  — one MMIO exit at 0x10000000, then spin */
static const uint32_t PAYLOAD[] = {0xD2A20001, 0xF9000020, 0x14000000};

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

enum kind { SYS, ICC, ICH, ICV };

typedef struct {
    enum kind kind;
    uint16_t reg;
    const char *name;
    bool plant;       /* write `planted` before the run */
    uint64_t planted;
} reg_t;

#define S(n) { SYS, HV_SYS_REG_##n, #n, false, 0 }
#define SP(n, v) { SYS, HV_SYS_REG_##n, #n, true, v }
#define C(n) { ICC, HV_GIC_ICC_REG_##n, "ICC_" #n, false, 0 }
#define H(n) { ICH, HV_GIC_ICH_REG_##n, "ICH_" #n, false, 0 }
#define HP(n, v) { ICH, HV_GIC_ICH_REG_##n, "ICH_" #n, true, v }
#define V(n) { ICV, HV_GIC_ICV_REG_##n, "ICV_" #n, false, 0 }

static const reg_t REGS[] = {
    /* plain storage: planted, so a dropped write cannot pass as zero == zero */
    SP(TPIDR_EL2, 0x1122334455667788ULL), SP(VBAR_EL2, 0x80001000ULL),
    SP(ELR_EL2, 0x80002468ULL), SP(SP_EL2, 0x80fff000ULL), SP(FAR_EL2, 0xdeadbeef000ULL),
    SP(MAIR_EL2, 0x00000000004404ffULL), SP(TTBR0_EL2, 0x80004000ULL),
    SP(VTTBR_EL2, 0x0001000080008000ULL), SP(CNTHP_CVAL_EL2, 0x123456789ULL),
    SP(VPIDR_EL2, 0x610f0000ULL), SP(VMPIDR_EL2, 0x80000000ULL),
    SP(SPSR_EL2, 0x3c5ULL), SP(ESR_EL2, 0x96000045ULL), SP(HPFAR_EL2, 0x10000ULL),
    /* control: keep what the vCPU holds (libkrun sets HCR_EL2 / CNTHCTL_EL2 at boot) */
    S(HCR_EL2), S(CNTHCTL_EL2), S(CPTR_EL2), S(MDCR_EL2), S(SCTLR_EL2), S(TCR_EL2),
    S(TTBR1_EL2), S(VTCR_EL2), S(CNTHP_CTL_EL2), S(CNTHP_TVAL_EL2), S(CNTVOFF_EL2),
    /* the EL1/EL0 set libkrun's snapshot carries for every vCPU (SNAPSHOT_SYS_REGS) */
    S(ACTLR_EL1),
    S(AFSR0_EL1),
    S(AFSR1_EL1),
    S(AMAIR_EL1),
    S(APDAKEYHI_EL1),
    S(APDAKEYLO_EL1),
    S(APDBKEYHI_EL1),
    S(APDBKEYLO_EL1),
    S(APGAKEYHI_EL1),
    S(APGAKEYLO_EL1),
    S(APIAKEYHI_EL1),
    S(APIAKEYLO_EL1),
    S(APIBKEYHI_EL1),
    S(APIBKEYLO_EL1),
    S(CNTKCTL_EL1),
    S(CNTP_CTL_EL0),
    S(CNTP_CVAL_EL0),
    S(CNTV_CTL_EL0),
    S(CNTV_CVAL_EL0),
    S(CONTEXTIDR_EL1),
    S(CPACR_EL1),
    S(CSSELR_EL1),
    S(DBGBCR0_EL1),
    S(DBGBCR10_EL1),
    S(DBGBCR11_EL1),
    S(DBGBCR12_EL1),
    S(DBGBCR13_EL1),
    S(DBGBCR14_EL1),
    S(DBGBCR15_EL1),
    S(DBGBCR1_EL1),
    S(DBGBCR2_EL1),
    S(DBGBCR3_EL1),
    S(DBGBCR4_EL1),
    S(DBGBCR5_EL1),
    S(DBGBCR6_EL1),
    S(DBGBCR7_EL1),
    S(DBGBCR8_EL1),
    S(DBGBCR9_EL1),
    S(DBGBVR0_EL1),
    S(DBGBVR10_EL1),
    S(DBGBVR11_EL1),
    S(DBGBVR12_EL1),
    S(DBGBVR13_EL1),
    S(DBGBVR14_EL1),
    S(DBGBVR15_EL1),
    S(DBGBVR1_EL1),
    S(DBGBVR2_EL1),
    S(DBGBVR3_EL1),
    S(DBGBVR4_EL1),
    S(DBGBVR5_EL1),
    S(DBGBVR6_EL1),
    S(DBGBVR7_EL1),
    S(DBGBVR8_EL1),
    S(DBGBVR9_EL1),
    S(DBGWCR0_EL1),
    S(DBGWCR10_EL1),
    S(DBGWCR11_EL1),
    S(DBGWCR12_EL1),
    S(DBGWCR13_EL1),
    S(DBGWCR14_EL1),
    S(DBGWCR15_EL1),
    S(DBGWCR1_EL1),
    S(DBGWCR2_EL1),
    S(DBGWCR3_EL1),
    S(DBGWCR4_EL1),
    S(DBGWCR5_EL1),
    S(DBGWCR6_EL1),
    S(DBGWCR7_EL1),
    S(DBGWCR8_EL1),
    S(DBGWCR9_EL1),
    S(DBGWVR0_EL1),
    S(DBGWVR10_EL1),
    S(DBGWVR11_EL1),
    S(DBGWVR12_EL1),
    S(DBGWVR13_EL1),
    S(DBGWVR14_EL1),
    S(DBGWVR15_EL1),
    S(DBGWVR1_EL1),
    S(DBGWVR2_EL1),
    S(DBGWVR3_EL1),
    S(DBGWVR4_EL1),
    S(DBGWVR5_EL1),
    S(DBGWVR6_EL1),
    S(DBGWVR7_EL1),
    S(DBGWVR8_EL1),
    S(DBGWVR9_EL1),
    S(ELR_EL1),
    S(ESR_EL1),
    S(FAR_EL1),
    S(ID_AA64DFR0_EL1),
    S(ID_AA64DFR1_EL1),
    S(ID_AA64ISAR0_EL1),
    S(ID_AA64ISAR1_EL1),
    S(ID_AA64MMFR0_EL1),
    S(ID_AA64MMFR1_EL1),
    S(ID_AA64MMFR2_EL1),
    S(ID_AA64PFR0_EL1),
    S(ID_AA64PFR1_EL1),
    S(MAIR_EL1),
    S(MDCCINT_EL1),
    S(MDSCR_EL1),
    S(PAR_EL1),
    S(SCTLR_EL1),
    S(SPSR_EL1),
    S(SP_EL0),
    S(SP_EL1),
    S(TCR_EL1),
    S(TPIDRRO_EL0),
    S(TPIDR_EL0),
    S(TPIDR_EL1),
    S(TTBR0_EL1),
    S(TTBR1_EL1),
    S(VBAR_EL1),
    /* the EL1 GIC CPU interface libkrun's snapshot carries (SNAPSHOT_ICC_REGS) */
    C(PMR_EL1), C(BPR0_EL1), C(AP0R0_EL1), C(AP1R0_EL1), C(BPR1_EL1), C(CTLR_EL1),
    C(SRE_EL1), C(IGRPEN0_EL1), C(IGRPEN1_EL1),
    C(SRE_EL2),
    /* the guest hypervisor's virtual-interrupt state for its own guest */
    HP(LR0_EL2, 0x5000000000000020ULL), /* pending, group 1, vINTID 32 */
    HP(LR1_EL2, 0x5000000000000021ULL), H(LR2_EL2), H(LR3_EL2), H(LR4_EL2), H(LR5_EL2),
    H(LR6_EL2), H(LR7_EL2), H(LR8_EL2), H(LR9_EL2), H(LR10_EL2), H(LR11_EL2), H(LR12_EL2),
    H(LR13_EL2), H(LR14_EL2), H(LR15_EL2),
    HP(AP0R0_EL2, 0x1ULL), HP(AP1R0_EL2, 0x2ULL), HP(VMCR_EL2, 0xf0000003ULL), H(HCR_EL2),
    H(VTR_EL2), H(MISR_EL2), H(EISR_EL2), H(ELRSR_EL2),
    /* the views an L2 guest sees (derived from ICH state, expected) */
    V(PMR_EL1), V(BPR0_EL1), V(AP0R0_EL1), V(AP1R0_EL1), V(RPR_EL1), V(BPR1_EL1),
    V(CTLR_EL1), V(SRE_EL1), V(IGRPEN0_EL1), V(IGRPEN1_EL1),
};
#define NREGS ((int)(sizeof(REGS) / sizeof(REGS[0])))

static hv_return_t get(hv_vcpu_t v, const reg_t *r, uint64_t *out) {
    switch (r->kind) {
    case SYS: return hv_vcpu_get_sys_reg(v, (hv_sys_reg_t)r->reg, out);
    case ICC: return hv_gic_get_icc_reg(v, (hv_gic_icc_reg_t)r->reg, out);
    case ICH: return hv_gic_get_ich_reg(v, (hv_gic_ich_reg_t)r->reg, out);
    case ICV: return hv_gic_get_icv_reg(v, (hv_gic_icv_reg_t)r->reg, out);
    }
    return HV_ERROR;
}

static hv_return_t set(hv_vcpu_t v, const reg_t *r, uint64_t val) {
    switch (r->kind) {
    case SYS: return hv_vcpu_set_sys_reg(v, (hv_sys_reg_t)r->reg, val);
    case ICC: return hv_gic_set_icc_reg(v, (hv_gic_icc_reg_t)r->reg, val);
    case ICH: return hv_gic_set_ich_reg(v, (hv_gic_ich_reg_t)r->reg, val);
    case ICV: return hv_gic_set_icv_reg(v, (hv_gic_icv_reg_t)r->reg, val);
    }
    return HV_ERROR;
}

static void *g_ram;

static void vm_up(bool el2) {
    hv_vm_config_t cfg = hv_vm_config_create();
    if (el2) CHECK(hv_vm_config_set_el2_enabled(cfg, true));
    CHECK(hv_vm_create(cfg));
    os_release(cfg);
    size_t dist = 0, redist = 0;
    CHECK(hv_gic_get_distributor_size(&dist));
    CHECK(hv_gic_get_redistributor_size(&redist));
    hv_gic_config_t g = hv_gic_config_create();
    CHECK(hv_gic_config_set_distributor_base(g, MMIO_MEM_START - dist - redist));
    CHECK(hv_gic_config_set_redistributor_base(g, MMIO_MEM_START - redist));
    CHECK(hv_gic_create(g));
    os_release(g);
    g_ram = mmap(NULL, RAM_SIZE, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    memcpy(g_ram, PAYLOAD, sizeof(PAYLOAD));
    CHECK(hv_vm_map(g_ram, RAM_BASE, RAM_SIZE, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
}

static void vm_down(hv_vcpu_t v) {
    CHECK(hv_vcpu_destroy(v));
    hv_vm_unmap(RAM_BASE, RAM_SIZE);
    munmap(g_ram, RAM_SIZE);
    CHECK(hv_vm_destroy());
}

static hv_vcpu_t vcpu_up(hv_vcpu_exit_t **vexit, bool el2) {
    hv_vcpu_t v;
    CHECK(hv_vcpu_create(&v, vexit, NULL));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_MPIDR_EL1, 0));
    CHECK(hv_vcpu_set_reg(v, HV_REG_CPSR, el2 ? CPSR_EL2H : 0x3C5ULL));
    CHECK(hv_vcpu_set_reg(v, HV_REG_PC, RAM_BASE));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X0, 0xA11CE));
    return v;
}

static bool run_to_mmio(hv_vcpu_t v, hv_vcpu_exit_t *vexit) {
    CHECK(hv_vcpu_run(v));
    uint64_t ec = (vexit->exception.syndrome >> 26) & 0x3f;
    bool ok = vexit->reason == HV_EXIT_REASON_EXCEPTION && (ec == 0x24 || ec == 0x25) &&
              vexit->exception.physical_address == 0x10000000ULL;
    printf("run: reason=%u ec=0x%llx pa=0x%llx -> %s\n", vexit->reason, ec,
           vexit->exception.physical_address, ok ? "the MMIO exit" : "UNEXPECTED");
    return ok;
}

static size_t gic_blob_size(void) {
    hv_gic_state_t s = hv_gic_state_create();
    if (!s) return 0;
    size_t n = 0;
    hv_gic_state_get_size(s, &n);
    os_release(s);
    return n;
}

int main(void) {
    bool supported = false;
    CHECK(hv_vm_config_get_el2_supported(&supported));
    printf("hv_vm_config_get_el2_supported: %s\n", supported ? "yes" : "no");
    if (!supported) return 2;

    hv_vcpu_exit_t *vexit;

    /* the GIC blob without EL2, for comparison */
    vm_up(false);
    hv_vcpu_t v = vcpu_up(&vexit, false);
    size_t blob_el1 = gic_blob_size();
    vm_down(v);

    /* ---- VM 1: plant, run, save ---- */
    vm_up(true);
    v = vcpu_up(&vexit, true);
    for (int i = 0; i < NREGS; i++) {
        if (!REGS[i].plant) continue;
        hv_return_t r = set(v, &REGS[i], REGS[i].planted);
        if (r != HV_SUCCESS)
            printf("plant %-18s -> %s\n", REGS[i].name, rc_name(r));
    }
    if (!run_to_mmio(v, vexit)) return 1;
    size_t blob_el2 = gic_blob_size();
    printf("GIC state blob: %zu bytes without EL2, %zu with\n", blob_el1, blob_el2);

    bool masked = false, pirq = false, pfiq = false;
    printf("vtimer_mask get: %s; ", rc_name(hv_vcpu_get_vtimer_mask(v, &masked)));
    printf("pending IRQ get: %s; ",
           rc_name(hv_vcpu_get_pending_interrupt(v, HV_INTERRUPT_TYPE_IRQ, &pirq)));
    printf("pending FIQ get: %s\n",
           rc_name(hv_vcpu_get_pending_interrupt(v, HV_INTERRUPT_TYPE_FIQ, &pfiq)));

    uint64_t saved[NREGS];
    hv_return_t get_rc[NREGS];
    for (int i = 0; i < NREGS; i++) {
        saved[i] = 0;
        get_rc[i] = get(v, &REGS[i], &saved[i]);
    }

    /* CNTVOFF_EL2 vs the vtimer-offset API: one value or two? */
    uint64_t off = 0, cntvoff = 0;
    CHECK(hv_vcpu_get_vtimer_offset(v, &off));
    hv_return_t rc = hv_vcpu_get_sys_reg(v, HV_SYS_REG_CNTVOFF_EL2, &cntvoff);
    printf("before: vtimer_offset=0x%llx CNTVOFF_EL2=0x%llx (%s)\n", off, cntvoff, rc_name(rc));
    CHECK(hv_vcpu_set_vtimer_offset(v, 0x1000000));
    hv_vcpu_get_vtimer_offset(v, &off);
    hv_vcpu_get_sys_reg(v, HV_SYS_REG_CNTVOFF_EL2, &cntvoff);
    printf("after set_vtimer_offset(0x1000000): vtimer_offset=0x%llx CNTVOFF_EL2=0x%llx\n", off,
           cntvoff);
    rc = hv_vcpu_set_sys_reg(v, HV_SYS_REG_CNTVOFF_EL2, 0x2000000);
    hv_vcpu_get_vtimer_offset(v, &off);
    hv_vcpu_get_sys_reg(v, HV_SYS_REG_CNTVOFF_EL2, &cntvoff);
    printf("after set CNTVOFF_EL2=0x2000000 (%s): vtimer_offset=0x%llx CNTVOFF_EL2=0x%llx\n",
           rc_name(rc), off, cntvoff);
    vm_down(v);

    /* ---- VM 2: fresh, restore, read back ---- */
    vm_up(true);
    v = vcpu_up(&vexit, true);
    int rejected = 0, mismatched = 0, unreadable = 0, planted_lost = 0;
    printf("\n%-18s %-14s %-18s %-14s %s\n", "register", "get", "saved", "set", "readback");
    for (int i = 0; i < NREGS; i++) {
        if (get_rc[i] != HV_SUCCESS) {
            printf("%-18s %-14s\n", REGS[i].name, rc_name(get_rc[i]));
            unreadable++;
            continue;
        }
        hv_return_t s = set(v, &REGS[i], saved[i]);
        uint64_t back = 0;
        hv_return_t g = get(v, &REGS[i], &back);
        const char *verdict = s != HV_SUCCESS ? "REJECTED"
                              : g != HV_SUCCESS ? "unreadable"
                              : back == saved[i] ? "same"
                                                 : "DIFFERS";
        if (s != HV_SUCCESS) rejected++;
        else if (g == HV_SUCCESS && back != saved[i]) mismatched++;
        if (REGS[i].plant && saved[i] != REGS[i].planted) planted_lost++;
        printf("%-18s %-14s 0x%016llx %-14s %s", REGS[i].name, "ok", saved[i], rc_name(s),
               verdict);
        if (g == HV_SUCCESS && back != saved[i]) printf(" (0x%016llx)", back);
        if (REGS[i].plant && saved[i] != REGS[i].planted)
            printf("  [planted 0x%llx did not survive the run]", REGS[i].planted);
        printf("\n");
    }
    printf("\nsummary: %d registers, %d unreadable, %d set rejected, %d read back different, "
           "%d planted values changed by the run\n",
           NREGS, unreadable, rejected, mismatched, planted_lost);
    bool runs = run_to_mmio(v, vexit);
    vm_down(v);
    return runs ? 0 : 1;
}
