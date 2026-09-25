// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// What hv_vm_unmap and hv_vm_map do to ranges that are not wholly in the state the call expects:
// an unmap of a page already unmapped, an unmap spanning mapped and unmapped pages, an unmap
// spanning several separate map calls, and a map overlapping a live mapping. ReleasedRam's
// bookkeeping (libkrun src/hvf/src/released_ram.rs) has to agree with whatever these return.
//
// Each case gets a fresh VM over four host pages at GPA 0x8000_0000. `state` then probes every
// page by trying to map it alone: a page that maps was unmapped (and is unmapped again after).

#include <Hypervisor/Hypervisor.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

#define GPA 0x80000000ull
#define PAGES 4
static size_t page;
static void *host;
static const hv_memory_flags_t RWX = HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC;

static hv_return_t map(int first, int n) {
    return hv_vm_map((char *)host + first * page, GPA + first * page, n * page, RWX);
}
static hv_return_t unmap(int first, int n) { return hv_vm_unmap(GPA + first * page, n * page); }

static void state(const char *label) {
    printf("    %-34s", label);
    for (int p = 0; p < PAGES; p++) {
        int was_unmapped = map(p, 1) == HV_SUCCESS;
        if (was_unmapped) unmap(p, 1);
        printf(" p%d=%s", p, was_unmapped ? "unmapped" : "mapped  ");
    }
    printf("\n");
}

static void fresh(const char *name) {
    hv_vm_destroy();
    if (hv_vm_create(NULL) != HV_SUCCESS) { fprintf(stderr, "hv_vm_create failed\n"); exit(1); }
    printf("%s\n", name);
}

#define SHOW(call) printf("    %-34s -> %#x\n", #call, (unsigned)(call))

int main(void) {
    page = getpagesize();
    host = mmap(NULL, PAGES * page, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    printf("host page %zu bytes; HV_SUCCESS=%#x\n\n", page, HV_SUCCESS);

    fresh("A. unmap a page twice");
    SHOW(map(0, 4));
    SHOW(unmap(1, 1));
    SHOW(unmap(1, 1));
    state("after");

    fresh("B. unmap a range whose middle is already unmapped");
    SHOW(map(0, 4));
    SHOW(unmap(1, 1));
    SHOW(unmap(0, 4));
    state("after");

    fresh("C. unmap a range with a never-mapped page at its end");
    SHOW(map(0, 3));
    SHOW(unmap(0, 4));
    state("after");

    fresh("D. one unmap across four separate maps");
    for (int p = 0; p < PAGES; p++) SHOW(map(p, 1));
    SHOW(unmap(0, 4));
    state("after");

    fresh("E. split a mapping in its middle, then unmap its head");
    SHOW(map(0, 4));
    SHOW(unmap(1, 2));
    state("after middle unmap");
    SHOW(unmap(0, 1));
    state("after head unmap");

    fresh("F. map over a live mapping, whole and partial");
    SHOW(map(0, 2));
    SHOW(map(0, 2));
    SHOW(map(1, 2));
    state("after");

    hv_vm_destroy();
    return 0;
}
