// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Which of HVF's per-vCPU pending interrupt line calls a host accepts, with and without the
// in-kernel GIC. `./probe gic` creates the GIC first; `./probe nogic` does not.
// Build/run: build.sh. Results: RESULTS.md.

#include <Hypervisor/Hypervisor.h>
#include <stdio.h>
#include <string.h>

static const char *name(hv_return_t r) {
    switch (r) {
    case HV_SUCCESS: return "HV_SUCCESS";
    case HV_ERROR: return "HV_ERROR";
    case HV_BUSY: return "HV_BUSY";
    case HV_BAD_ARGUMENT: return "HV_BAD_ARGUMENT";
    case HV_NO_RESOURCES: return "HV_NO_RESOURCES";
    case HV_NO_DEVICE: return "HV_NO_DEVICE";
    case HV_DENIED: return "HV_DENIED";
    case HV_UNSUPPORTED: return "HV_UNSUPPORTED";
    default: return "?";
    }
}

int main(int argc, char **argv) {
    int gic = argc > 1 && strcmp(argv[1], "gic") == 0;
    hv_return_t r = hv_vm_create(NULL);
    printf("hv_vm_create -> %s\n", name(r));
    if (r != HV_SUCCESS) return 1;
    if (gic) {
        hv_gic_config_t cfg = hv_gic_config_create();
        hv_gic_config_set_distributor_base(cfg, 0x09fd0000);
        hv_gic_config_set_redistributor_base(cfg, 0x09fe0000);
        r = hv_gic_create(cfg);
        printf("hv_gic_create -> %s\n", name(r));
        if (r != HV_SUCCESS) return 1;
    }
    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *exit;
    r = hv_vcpu_create(&vcpu, &exit, NULL);
    printf("hv_vcpu_create -> %s\n", name(r));
    if (r != HV_SUCCESS) return 1;

    const struct { hv_interrupt_type_t ty; const char *n; } lines[] = {
        { HV_INTERRUPT_TYPE_IRQ, "IRQ" },
        { HV_INTERRUPT_TYPE_FIQ, "FIQ" },
    };
    for (unsigned i = 0; i < 2; i++) {
        bool p = false;
        r = hv_vcpu_get_pending_interrupt(vcpu, lines[i].ty, &p);
        printf("%s get -> %s (pending=%d)\n", lines[i].n, name(r), p);
        r = hv_vcpu_set_pending_interrupt(vcpu, lines[i].ty, false);
        printf("%s set false -> %s\n", lines[i].n, name(r));
        r = hv_vcpu_set_pending_interrupt(vcpu, lines[i].ty, true);
        printf("%s set true -> %s\n", lines[i].n, name(r));
        r = hv_vcpu_get_pending_interrupt(vcpu, lines[i].ty, &p);
        printf("%s get after set -> %s (pending=%d)\n", lines[i].n, name(r), p);
    }
    return 0;
}
