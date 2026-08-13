// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* Can we hide SME from a guest by writing its ID registers under HVF?
 *
 * Chrome 151 SIGILLs in the guest on an M4 host: its SME path issues a
 * non-streaming SVE `cntd`, which UNDEFs on SME-without-SVE hardware. The
 * compat fix we want is to stop advertising SME to guests at all.
 *
 * Hypervisor.framework offers hv_vcpu_config_GET_feature_reg but no setter, so
 * the config route is closed. The remaining candidate is the sys-reg route:
 * HV_SYS_REG_ID_AA64PFR1_EL1 and HV_SYS_REG_ID_AA64SMFR0_EL1 exist in
 * hv_sys_reg_t, and hv_vcpu_set_sys_reg accepts any hv_sys_reg_t — but whether
 * HVF HONORS a write to a read-only ID register is undocumented, and the whole
 * approach rests on it. Hence this probe: write, read back, report.
 *
 * A returned HV_SUCCESS is NOT the answer. Only the read-back value is: HVF may
 * accept and discard. That is the premise this exists to test.
 *
 * Fields (Arm ARM):
 *   ID_AA64PFR1_EL1.SME  bits [27:24] — 0 = not implemented
 *   ID_AA64SMFR0_EL1     the SME feature register; 0 = nothing
 *
 * Build+run: ./build-and-run.sh (needs the hypervisor entitlement).
 */
#include <Hypervisor/Hypervisor.h>
#include <inttypes.h>
#include <stdio.h>

#define SME_SHIFT 24
#define SME_MASK (0xfULL << SME_SHIFT)

static const char *rs(hv_return_t r) {
    switch (r) {
    case HV_SUCCESS: return "HV_SUCCESS";
    case HV_ERROR: return "HV_ERROR";
    case HV_BUSY: return "HV_BUSY";
    case HV_BAD_ARGUMENT: return "HV_BAD_ARGUMENT";
    case HV_NO_RESOURCES: return "HV_NO_RESOURCES";
    case HV_NO_DEVICE: return "HV_NO_DEVICE";
    case HV_UNSUPPORTED: return "HV_UNSUPPORTED";
    default: return "?";
    }
}

static void probe(hv_vcpu_t vcpu, hv_sys_reg_t reg, const char *name,
                  uint64_t want) {
    uint64_t before = 0, after = 0;
    hv_return_t rg = hv_vcpu_get_sys_reg(vcpu, reg, &before);
    if (rg != HV_SUCCESS) {
        printf("%-20s GET failed: %s\n", name, rs(rg));
        return;
    }
    hv_return_t rp = hv_vcpu_set_sys_reg(vcpu, reg, want);
    hv_return_t rg2 = hv_vcpu_get_sys_reg(vcpu, reg, &after);
    printf("%-20s before=0x%016" PRIx64 "  set(0x%016" PRIx64 ")=%s  "
           "after=0x%016" PRIx64 "  => %s\n",
           name, before, want, rs(rp), after,
           rg2 != HV_SUCCESS      ? "READBACK FAILED"
           : after == want        ? "HONORED"
           : after == before      ? "IGNORED (accepted and discarded)"
                                  : "PARTIAL");
}

int main(void) {
    hv_return_t r = hv_vm_create(NULL);
    if (r != HV_SUCCESS) {
        printf("hv_vm_create failed: %s (entitlement? another VM running?)\n",
               rs(r));
        return 1;
    }
    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *exit;
    r = hv_vcpu_create(&vcpu, &exit, NULL);
    if (r != HV_SUCCESS) {
        printf("hv_vcpu_create failed: %s\n", rs(r));
        return 1;
    }

    /* The config feature-reg is what the GUEST is told, so read both: on a host
     * without SME they should agree at zero, and if they ever disagree the
     * config value is the one that matters. */
    hv_vcpu_config_t cfg = hv_vcpu_config_create();
    uint64_t cfg_pfr1 = 0, cfg_smfr0 = 0;
    hv_return_t rc1 =
        hv_vcpu_config_get_feature_reg(cfg, HV_FEATURE_REG_ID_AA64PFR1_EL1, &cfg_pfr1);
    hv_return_t rc2 = hv_vcpu_config_get_feature_reg(
        cfg, HV_FEATURE_REG_ID_AA64SMFR0_EL1, &cfg_smfr0);
    printf("config ID_AA64PFR1_EL1 = 0x%016" PRIx64 " (%s), SME field = %" PRIu64
           "\n",
           cfg_pfr1, rs(rc1), (cfg_pfr1 & SME_MASK) >> SME_SHIFT);
    printf("config ID_AA64SMFR0_EL1 = 0x%016" PRIx64 " (%s)\n", cfg_smfr0, rs(rc2));

    uint64_t pfr1 = 0;
    hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_ID_AA64PFR1_EL1, &pfr1);
    printf("vcpu   ID_AA64PFR1_EL1 = 0x%016" PRIx64 "  (SME field = %" PRIu64
           ")\n\n",
           pfr1, (pfr1 & SME_MASK) >> SME_SHIFT);

    /* THE test, and it must not be vacuous. This dev Mac is an M1 Max with no
     * SME, so clearing the SME field writes back the value already there and
     * every result reads "HONORED" while proving nothing. Write a value that
     * DIFFERS from what is there — set the SME field to 1 — and see whether the
     * read-back follows. If HVF honors an arbitrary ID-register write here, it
     * will honor the masking write on an M4; if it silently discards this one,
     * the sys-reg route is closed and the guest cmdline is the only lever. */
    probe(vcpu, HV_SYS_REG_ID_AA64PFR1_EL1, "PFR1 set SME=1",
          (pfr1 & ~SME_MASK) | (1ULL << SME_SHIFT));
    probe(vcpu, HV_SYS_REG_ID_AA64PFR1_EL1, "PFR1 restore", pfr1);
    probe(vcpu, HV_SYS_REG_ID_AA64SMFR0_EL1, "SMFR0 set bit0", 1);

    hv_vcpu_destroy(vcpu);
    hv_vm_destroy();
    return 0;
}
