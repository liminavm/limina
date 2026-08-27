// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Does macOS 26's hv_vm_config_set_ipa_granule() let us map guest physical addresses at 4 KiB
// granularity on a 16 KiB host?
//
// This is THE question behind the venus 4 KiB-guest wall. A guest packs host-visible virtio-gpu
// blobs back to back in one arena, so a blob's offset is the running sum of the sizes before it;
// on a 4 KiB guest those offsets are 4 KiB-granular, and hv_vm_map refused them outright:
//
//   hv_vm_map failed: ret=0xfae94003 host=0x14b314000 guest=0x280021000 size=0x100000
//                     (host%16k=0 guest%16k=4096 size%16k=0)
//
// Everything we designed around that refusal — rounding blob sizes in the guest, pooling
// host-visible memory into one mapped heap — was downstream of assuming the stage-2 granule is
// pinned to the host page size. It isn't, from macOS 26 on. Run the binary once per granule and
// compare; case C is the one that decides whether the whole workaround family is unnecessary.
//
// Usage: granule 4k|16k|default

#include <Hypervisor/Hypervisor.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#define HOST_PAGE 16384ULL
#define GUEST_PAGE 4096ULL

// Well clear of anything a real guest would use, and 16 KiB-aligned so every case controls its own
// misalignment rather than inheriting one. Cases are spaced 1 MiB+ apart on purpose: case B maps a
// full megabyte, and an overlapping map returns HV_ERROR (0xfae94001), which reads exactly like a
// granule refusal if you are not watching the addresses.
#define IPA_BASE 0x400000000ULL

static const char *granule_name(hv_ipa_granule_t g) {
    switch (g) {
    case HV_IPA_GRANULE_4KB:
        return "4KB";
    case HV_IPA_GRANULE_16KB:
        return "16KB";
    default:
        return "unknown";
    }
}

// Host memory that is 16 KiB-aligned, i.e. what a Metal buffer's contents look like. The point of
// every case below is the GUEST address, so the host side is deliberately never the variable.
static void *host_pages(size_t len) {
    void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (p == MAP_FAILED)
        return NULL;
    memset(p, 0xAB, len);
    return p;
}

static bool try_map(const char *what, void *host, uint64_t ipa, uint64_t size) {
    hv_return_t ret = hv_vm_map(host, ipa, size, HV_MEMORY_READ | HV_MEMORY_WRITE);
    printf("  %-58s host=%p ipa=0x%llx size=0x%llx -> %s (0x%x)\n", what, host, ipa, size,
           ret == HV_SUCCESS ? "OK" : "FAILED", ret);
    return ret == HV_SUCCESS;
}

int main(int argc, char **argv) {
    const char *want = argc > 1 ? argv[1] : "default";

    hv_ipa_granule_t dflt;
    hv_return_t ret = hv_vm_config_get_default_ipa_granule(&dflt);
    if (ret != HV_SUCCESS) {
        printf("hv_vm_config_get_default_ipa_granule failed: 0x%x\n", ret);
        return 1;
    }
    printf("default IPA granule on this host: %s\n", granule_name(dflt));

    hv_vm_config_t config = hv_vm_config_create();
    if (!config) {
        printf("hv_vm_config_create returned NULL\n");
        return 1;
    }

    if (strcmp(want, "default") != 0) {
        hv_ipa_granule_t g = strcmp(want, "4k") == 0 ? HV_IPA_GRANULE_4KB : HV_IPA_GRANULE_16KB;
        ret = hv_vm_config_set_ipa_granule(config, g);
        printf("hv_vm_config_set_ipa_granule(%s) -> %s (0x%x)\n", granule_name(g),
               ret == HV_SUCCESS ? "OK" : "FAILED", ret);
        if (ret != HV_SUCCESS)
            return 1;
    }

    hv_ipa_granule_t effective;
    ret = hv_vm_config_get_ipa_granule(config, &effective);
    printf("configured IPA granule: %s (query %s)\n", granule_name(effective),
           ret == HV_SUCCESS ? "OK" : "FAILED");

    ret = hv_vm_create(config);
    if (ret != HV_SUCCESS) {
        // Not codesigned with com.apple.security.hypervisor is by far the likeliest cause.
        printf("hv_vm_create failed: 0x%x\n", ret);
        return 1;
    }
    printf("VM created.\n\n");

    void *a = host_pages(2 * 1024 * 1024);
    void *b = host_pages(HOST_PAGE);
    if (!a || !b) {
        printf("mmap failed\n");
        return 1;
    }

    int failures = 0;

    // A — control. Everything 16 KiB-aligned; must pass under either granule, and proves the
    // mapping machinery itself works before any case blames the granule for a plain mistake.
    printf("A. control: everything 16 KiB-aligned\n");
    if (!try_map("16k-aligned ipa, 16k size", a, IPA_BASE, HOST_PAGE))
        failures++;

    // B — the reported failure, reproduced exactly: a 4 KiB-aligned IPA that is not 16 KiB-aligned,
    // with an aligned host pointer and an aligned size. This is the line from the Debian log.
    printf("\nB. the reported failure: ipa 4 KiB-aligned but not 16 KiB-aligned\n");
    if (!try_map("ipa +0x1000, 1 MiB", a, IPA_BASE + 0x100000ULL + 0x1000ULL, 0x100000ULL))
        failures++;

    // C — the one that decides the design. Two SEPARATE host allocations presented contiguously
    // across a 4 KiB boundary inside one 16 KiB IPA page: exactly two adjacent blobs from
    // different Metal allocations. If this passes, no pooling and no guest-side size rounding is
    // needed, because the guest's packing no longer constrains where host memory lives.
    printf("\nC. two separate host allocations, adjacent across a 4 KiB IPA boundary\n");
    uint64_t split = IPA_BASE + 0x400000ULL;
    if (!try_map("alloc #1 at ipa+0x0, 4 KiB", a, split, GUEST_PAGE))
        failures++;
    if (!try_map("alloc #2 at ipa+0x1000, 4 KiB (different host page)", b, split + GUEST_PAGE,
                 GUEST_PAGE))
        failures++;

    // D — a bare 4 KiB mapping, to separate "the size may be sub-host-page" from "the address may
    // be sub-host-page-aligned". They are different permissions and B/C would not tell them apart.
    printf("\nD. a 4 KiB-sized mapping\n");
    if (!try_map("aligned ipa, 4 KiB size", a, IPA_BASE + 0x500000ULL, GUEST_PAGE))
        failures++;

    printf("\n%s: %d of 5 maps failed\n", failures ? "PARTIAL/NO" : "ALL PASSED", failures);
    return failures ? 2 : 0;
}
