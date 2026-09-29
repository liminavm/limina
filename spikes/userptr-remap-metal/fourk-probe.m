// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * What a process can do at 4 KiB granularity. Run it directly (a normal 16 KiB
 * process) and under fourk-spawn (a 4 KiB-page address space) and compare:
 *
 *   1. the page size the process sees;
 *   2. mach_vm_remap of scattered 4 KiB pages into one contiguous alias, and
 *      whether a lone 4 KiB remap exposes only 4 KiB;
 *   3. a no-copy Metal buffer over that alias, read and written by the GPU;
 *   4. hv_vm_map, with the 4 KiB IPA granule, of host addresses that are
 *      4 KiB- but not 16 KiB-aligned (the migrate route's open question).
 *
 * Also builds as x86_64 (fourk-probe-x86, run under Rosetta), minus the hv_vm_map step.
 *
 * Build/sign: build.sh. Sandbox off (hv_vm_*).
 */

#if !defined(__x86_64__)
#include <Hypervisor/Hypervisor.h>
#endif
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include <dlfcn.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#define K4 4096ULL
#define SRC_PAGES 64
#define WORDS (K4 / 8)

static uint64_t word(uint64_t page, uint64_t w, uint64_t salt) {
    return (salt << 48) | (page << 16) | w;
}

static void fill(uint64_t *src, uint64_t salt) {
    for (uint64_t p = 0; p < SRC_PAGES; p++)
        for (uint64_t w = 0; w < WORDS; w++) src[p * WORDS + w] = word(p, w, salt);
}

/* Words of `buf` (laid out as pages list[0..n)) not holding `salt`'s pattern. */
static uint64_t count(const volatile uint64_t *buf, const uint64_t *list, int n, uint64_t salt) {
    uint64_t m = 0;
    for (int i = 0; i < n; i++)
        for (uint64_t w = 0; w < WORDS; w++)
            if (buf[i * WORDS + w] != word(list[i], w, salt)) m++;
    return m;
}

static const char *kr(kern_return_t k) { return k == KERN_SUCCESS ? "KERN_SUCCESS" : mach_error_string(k); }

int main(void) {
    @autoreleasepool {
        printf("== page size ==\n  getpagesize()=%d vm_page_size=%lu vm_kernel_page_size=%lu\n",
               getpagesize(), (unsigned long)vm_page_size, (unsigned long)vm_kernel_page_size);

        mach_vm_address_t src = 0;
        kern_return_t k = mach_vm_allocate(mach_task_self(), &src, SRC_PAGES * K4, VM_FLAGS_ANYWHERE);
        if (k) { printf("allocate: %s\n", kr(k)); return 1; }
        fill((uint64_t *)src, 1);

        /* ---- 2. scattered 4 KiB remap ---- */
        static const uint64_t list[] = {5, 17, 2, 40, 41, 63, 0, 30};
        const int n = sizeof(list) / sizeof(list[0]);
        printf("\n== remap %d scattered 4 KiB pages into one alias ==\n", n);
        mach_vm_address_t alias = 0;
        k = mach_vm_allocate(mach_task_self(), &alias, n * K4, VM_FLAGS_ANYWHERE);
        printf("  allocate alias (%d x 4 KiB) at 0x%llx: %s\n", n, alias, kr(k));
        bool remapped = k == KERN_SUCCESS;
        for (int i = 0; remapped && i < n; i++) {
            mach_vm_address_t dst = alias + i * K4;
            vm_prot_t cur, max;
            k = mach_vm_remap(mach_task_self(), &dst, K4, 0, VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
                              mach_task_self(), src + list[i] * K4, FALSE, &cur, &max, VM_INHERIT_NONE);
            if (k || dst != alias + i * K4) {
                printf("  remap page %llu to alias+0x%llx: %s, landed at 0x%llx\n", list[i],
                       (uint64_t)i * K4, kr(k), dst);
                remapped = false;
            }
        }
        if (remapped) {
            printf("  all remaps landed; alias vs source pattern: %llu of %llu words wrong\n",
                   count((const volatile uint64_t *)alias, list, n, 1), n * WORDS);
            ((volatile uint64_t *)src)[list[3] * WORDS + 7] = 0xabcdef;
            printf("  a store through the source shows through the alias: %s\n",
                   ((volatile uint64_t *)alias)[3 * WORDS + 7] == 0xabcdef ? "yes" : "NO");
            fill((uint64_t *)src, 1);
        }

        /* A lone 4 KiB remap from 4 KiB into a 16 KiB-aligned block: what does it expose? */
        mach_vm_address_t lone = 0;
        vm_prot_t cur, max;
        k = mach_vm_remap(mach_task_self(), &lone, K4, 0, VM_FLAGS_ANYWHERE, mach_task_self(),
                          src + 9 * K4, FALSE, &cur, &max, VM_INHERIT_NONE);
        if (k == KERN_SUCCESS) {
            mach_vm_address_t ra = lone;
            mach_vm_size_t rs = 0;
            vm_region_basic_info_data_64_t info;
            mach_msg_type_number_t cnt = VM_REGION_BASIC_INFO_COUNT_64;
            mach_port_t obj;
            mach_vm_region(mach_task_self(), &ra, &rs, VM_REGION_BASIC_INFO_64,
                           (vm_region_info_t)&info, &cnt, &obj);
            bool right = ((volatile uint64_t *)lone)[0] == word(9, 0, 1);
            printf("  lone 4 KiB remap of page 9 -> 0x%llx (addr %% 16K = 0x%llx); region there is "
                   "0x%llx..0x%llx (%llu bytes); first word is page 9's: %s\n",
                   lone, lone % 16384, ra, ra + rs, rs, right ? "yes" : "NO");
        } else {
            printf("  lone 4 KiB remap: %s\n", kr(k));
        }

        /* ---- 3. Metal over the 4 KiB alias ---- */
        printf("\n== Metal over the alias ==\n");
        id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
        /* Is a refusal about the alias's alignment or about 4 KiB-granular backing? Control: a
         * plain 16 KiB-aligned allocation. Then 4 scattered 4 KiB pages remapped into a 16 KiB-
         * aligned, 16 KiB-long alias. */
        {
            mach_vm_address_t plain = 0, a16 = 0;
            mach_vm_map(mach_task_self(), &plain, 16384, 16383, VM_FLAGS_ANYWHERE, MACH_PORT_NULL, 0,
                        FALSE, VM_PROT_DEFAULT, VM_PROT_ALL, VM_INHERIT_DEFAULT);
            id<MTLBuffer> b = [dev newBufferWithBytesNoCopy:(void *)plain length:16384
                                                    options:MTLResourceStorageModeShared
                                                deallocator:nil];
            printf("  control: plain 16 KiB-aligned allocation at 0x%llx -> %s\n", plain,
                   b ? "a buffer" : "nil");
            b = nil;
            mach_vm_map(mach_task_self(), &a16, 16384, 16383, VM_FLAGS_ANYWHERE, MACH_PORT_NULL, 0,
                        FALSE, VM_PROT_DEFAULT, VM_PROT_ALL, VM_INHERIT_DEFAULT);
            static const uint64_t l4[] = {5, 17, 2, 40};
            bool ok = true;
            for (int i = 0; i < 4; i++) {
                mach_vm_address_t dst = a16 + i * K4;
                vm_prot_t c, m;
                ok &= mach_vm_remap(mach_task_self(), &dst, K4, 0, VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
                                    mach_task_self(), src + l4[i] * K4, FALSE, &c, &m,
                                    VM_INHERIT_NONE) == KERN_SUCCESS && dst == a16 + i * K4;
            }
            b = ok ? [dev newBufferWithBytesNoCopy:(void *)a16 length:16384
                                           options:MTLResourceStorageModeShared
                                       deallocator:nil]
                   : nil;
            printf("  4 scattered 4 KiB pages in a 16 KiB-aligned alias at 0x%llx (remaps %s) -> %s\n",
                   a16, ok ? "landed" : "FAILED", b ? "a buffer" : "nil");
            b = nil;
        }
        id<MTLBuffer> buf = remapped ? [dev newBufferWithBytesNoCopy:(void *)alias length:n * K4
                                                             options:MTLResourceStorageModeShared
                                                         deallocator:nil]
                                     : nil;
        printf("  newBufferWithBytesNoCopy(alias, %d x 4 KiB) -> %s\n", n, buf ? "a buffer" : "nil");
        if (buf) {
            NSError *err = nil;
            id<MTLLibrary> lib = [dev newLibraryWithSource:
                @"kernel void k(device ulong *b [[buffer(0)]], device ulong *o [[buffer(1)]],\n"
                 "              uint i [[thread_position_in_grid]]) {\n"
                 "    o[i] = b[i]; b[i] = (ulong(2) << 48) | (b[i] & 0xffffffffffffUL); }\n"
                                                   options:nil error:&err];
            id<MTLComputePipelineState> p =
                [dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"k"] error:&err];
            id<MTLBuffer> out = [dev newBufferWithLength:n * K4 options:MTLResourceStorageModeShared];
            id<MTLCommandQueue> q = [dev newCommandQueue];
            id<MTLCommandBuffer> cb = [q commandBuffer];
            id<MTLComputeCommandEncoder> e = [cb computeCommandEncoder];
            [e setComputePipelineState:p];
            [e setBuffer:buf offset:0 atIndex:0];
            [e setBuffer:out offset:0 atIndex:1];
            [e dispatchThreads:MTLSizeMake(n * WORDS, 1, 1) threadsPerThreadgroup:MTLSizeMake(64, 1, 1)];
            [e endEncoding];
            [cb commit];
            [cb waitUntilCompleted];
            if (cb.error) printf("  command buffer error: %s\n", cb.error.description.UTF8String);
            printf("  GPU read (source pattern, salt 1): %llu of %llu words wrong\n",
                   count(out.contents, list, n, 1), n * WORDS);
            /* The GPU rewrote salt 1 -> 2 through the alias; check it through the SOURCE pages. */
            uint64_t m = 0;
            for (int i = 0; i < n; i++)
                for (uint64_t w = 0; w < WORDS; w++)
                    if (((volatile uint64_t *)src)[list[i] * WORDS + w] != word(list[i], w, 2)) m++;
            printf("  GPU write seen through the source pages: %llu of %llu words wrong\n", m, n * WORDS);
            buf = nil;
        }

#if !defined(__x86_64__)
        /* ---- 4. hv_vm_map with 4 KiB-aligned host addresses ---- */
        printf("\n== hv_vm_map, 4 KiB IPA granule, host addresses 4 KiB- but not 16 KiB-aligned ==\n");
        typedef hv_vm_config_t (*create_fn)(void);
        typedef hv_return_t (*granule_fn)(hv_vm_config_t, uint32_t);
        create_fn create = (create_fn)dlsym(RTLD_DEFAULT, "hv_vm_config_create");
        granule_fn set = (granule_fn)dlsym(RTLD_DEFAULT, "hv_vm_config_set_ipa_granule");
        hv_vm_config_t cfg = create ? create() : NULL;
        hv_return_t r = (cfg && set) ? set(cfg, 0 /* HV_IPA_GRANULE_4KB */) : HV_UNSUPPORTED;
        printf("  set_ipa_granule(4KB): 0x%x\n", (uint32_t)r);
        r = hv_vm_create(cfg);
        printf("  hv_vm_create: 0x%x\n", (uint32_t)r);
        if (r == HV_SUCCESS) {
            /* Consecutive 4 KiB pieces of one host buffer at scattered guest addresses. */
            struct { uint64_t host_page, gpa; } maps[] = {{1, 0x80000000}, {2, 0x80005000}, {3, 0x80013000}};
            for (int i = 0; i < 3; i++) {
                uint64_t h = src + maps[i].host_page * K4;
                r = hv_vm_map((void *)h, maps[i].gpa, K4, HV_MEMORY_READ | HV_MEMORY_WRITE);
                printf("  hv_vm_map(host=0x%llx (%% 16K = 0x%llx), gpa=0x%llx, 4 KiB): 0x%x\n", h,
                       h % 16384, maps[i].gpa, (uint32_t)r);
            }
            hv_vm_destroy();
        }
#else
        printf("\n(hv_vm_map skipped: no Hypervisor.framework arm64 API in an x86_64 process)\n");
#endif
        return 0;
    }
}
