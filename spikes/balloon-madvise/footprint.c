// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * Spike: does host memory reclaim actually lower phys_footprint when the region
 * is mapped into a VM via Hypervisor.framework (hv_vm_map)?
 *
 * This reproduces libkrun's guest-RAM setup: a MAP_ANON region handed to
 * hv_vm_map (stage-2 IPA mapping), exactly like balloon/device.rs reclaims into.
 * We touch (commit) the region, then apply one madvise() reclaim mode and watch
 * phys_footprint (the number macOS bills the process / shows in Activity Monitor).
 *
 * Usage:  footprint <mode> <use_hvf 0|1> <unmap_before 0|1>
 *   mode: dontneed | free | reusable
 *
 * Build + sign: see run.sh (needs com.apple.security.hypervisor).
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/mman.h>
#include <mach/mach.h>
#include <mach/task_info.h>
#include <Hypervisor/Hypervisor.h>

#define REGION_SIZE ((size_t)1 << 30)   /* 1 GiB */
#define HOST_PAGE   ((size_t)16 * 1024) /* Apple Silicon host page */
#define GUEST_IPA   ((hv_ipa_t)0x40000000ULL) /* non-zero; hv_vm_map rejects 0 */

static unsigned long long phys_footprint_bytes(void) {
    task_vm_info_data_t info;
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    kern_return_t kr = task_info(mach_task_self(), TASK_VM_INFO,
                                 (task_info_t)&info, &count);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "task_info failed: %s\n", mach_error_string(kr));
        return 0;
    }
    return (unsigned long long)info.phys_footprint;
}

static double mib(unsigned long long b) { return (double)b / (1024.0 * 1024.0); }

static void report(const char *label, unsigned long long base) {
    unsigned long long f = phys_footprint_bytes();
    long long delta = (long long)f - (long long)base;
    printf("  %-28s phys_footprint = %8.1f MiB   (delta %+8.1f MiB)\n",
           label, mib(f), (double)delta / (1024.0 * 1024.0));
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: %s <dontneed|free|reusable> <use_hvf 0|1> <unmap_before 0|1>\n", argv[0]);
        return 2;
    }
    const char *mode = argv[1];
    int use_hvf = atoi(argv[2]);
    int unmap_before = atoi(argv[3]);

    int adv;
    if (!strcmp(mode, "dontneed"))      adv = MADV_DONTNEED;
    else if (!strcmp(mode, "free"))     adv = MADV_FREE;
    else if (!strcmp(mode, "reusable")) adv = MADV_FREE_REUSABLE;
    else { fprintf(stderr, "bad mode %s\n", mode); return 2; }

    printf("== mode=%s use_hvf=%d unmap_before=%d  (region=%zu MiB, host_page=%zu KiB) ==\n",
           mode, use_hvf, unmap_before, REGION_SIZE >> 20, HOST_PAGE >> 10);

    unsigned long long base = phys_footprint_bytes();
    printf("  %-28s phys_footprint = %8.1f MiB   (baseline)\n", "start", mib(base));

    /* 1. Allocate guest RAM exactly like libkrun: anonymous, private. */
    void *p = mmap(NULL, REGION_SIZE, PROT_READ | PROT_WRITE,
                   MAP_ANON | MAP_PRIVATE, -1, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 1; }
    report("after mmap (reserved)", base);

    /* 2. Commit every host page by touching it (guest faulting RAM in). */
    for (size_t off = 0; off < REGION_SIZE; off += HOST_PAGE)
        ((volatile char *)p)[off] = 1;
    report("after touch (committed)", base);

    /* 3. Optionally map into a VM via HVF, mirroring libkrun's hv_vm_map. */
    if (use_hvf) {
        hv_return_t r = hv_vm_create(NULL);
        if (r != HV_SUCCESS) {
            fprintf(stderr, "hv_vm_create failed: 0x%x (entitlement/codesign?)\n", r);
            return 1;
        }
        r = hv_vm_map(p, GUEST_IPA, REGION_SIZE,
                      HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
        if (r != HV_SUCCESS) {
            fprintf(stderr, "hv_vm_map failed: 0x%x\n", r);
            return 1;
        }
        report("after hv_vm_map", base);
    }

    /* 4. Optionally remove the stage-2 mapping before reclaiming. */
    if (use_hvf && unmap_before) {
        hv_return_t r = hv_vm_unmap(GUEST_IPA, REGION_SIZE);
        if (r != HV_SUCCESS) {
            fprintf(stderr, "hv_vm_unmap failed: 0x%x\n", r);
            return 1;
        }
        report("after hv_vm_unmap", base);
    }

    /* 5. The reclaim under test. */
    errno = 0;
    int rc = madvise(p, REGION_SIZE, adv);
    printf("  madvise(%s) -> rc=%d errno=%d (%s)\n",
           mode, rc, errno, rc ? strerror(errno) : "ok");
    report("after madvise", base);

    /* 6. macOS accounting can lag; re-read after a beat. */
    usleep(300 * 1000);
    report("after 300ms settle", base);

    return 0;
}
