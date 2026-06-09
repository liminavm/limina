// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// #28 follow-up — the PARTIAL-coherency decider (the "split-triangle / stale-icon" defect).
//
// bug A is fixed for small/static buffers (tri.c renders), but the seated desktop shows most UI
// quads with a garbage second triangle and windows that render only their first few (low-offset)
// items. Hypothesis: the guest's writes to a host-visible buffer reach the host GPU only up to some
// OFFSET/size boundary; data written further in is stale → degenerate geometry / missing draws.
//
// This probe measures that boundary directly, with NO instrumented MoltenVK and NO reboot:
//   - allocate ONE large host-visible buffer `src` (default 256 KiB = 16 host-pages @ 16 KiB)
//   - fill word[i] = 0xC0DE0000 | (i & 0xFFFF)  (unique across the whole buffer; nwords == 65536)
//   - GPU copies src -> mid -> dst2.  `dst2` is GPU-WRITTEN, hence host/guest-coherent to read back
//     (prior spike Finding 1), so dst2[i] == what the GPU actually READ out of the guest-written src.
//   - scan dst2: first mismatching word = the offset where the GPU stops seeing guest writes; we also
//     classify the bad value (0 = GPU saw zero / stale-other) and print a per-page coherency map.
//
// Build (guest): gcc vkcoh-offset.c -o vkcoh-offset -lvulkan
// Run   (guest): VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
//                VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback \
//                ./vkcoh-offset [bytes] [clean]
//   [bytes] optional buffer size (default 262144).  [clean] = dc cvac the whole src before the copy
//   (tests whether a guest cache-clean now extends coherency on the single map_ptr mapping).
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define VK(x) do { VkResult _r=(x); if(_r){fprintf(stderr,#x" = %d\n",_r); return 1;} } while(0)
#define MAGIC 0xC0DE0000u
#define PAT(i) (MAGIC | ((i) & 0xFFFFu))

static void clean_dcache(void *addr, size_t len) {
    uintptr_t start = (uintptr_t)addr & ~63UL;          // 64B cache line on Apple Silicon
    uintptr_t end   = (uintptr_t)addr + len;
    for (uintptr_t p = start; p < end; p += 64)
        __asm__ volatile("dc cvac, %0" :: "r"(p) : "memory");
    __asm__ volatile("dsb ish" ::: "memory");
}

static int alloc_hostvis(VkDevice dev, uint32_t type, VkDeviceSize n, VkBufferUsageFlags usage,
                         VkBuffer *buf, VkDeviceMemory *mem) {
    VkBufferCreateInfo bci = { .sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size=n, .usage=usage, .sharingMode=VK_SHARING_MODE_EXCLUSIVE };
    VK(vkCreateBuffer(dev, &bci, NULL, buf));
    VkMemoryRequirements mr; vkGetBufferMemoryRequirements(dev, *buf, &mr);
    VkMemoryAllocateInfo mai = { .sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize=mr.size, .memoryTypeIndex=type };
    VK(vkAllocateMemory(dev, &mai, NULL, mem));
    VK(vkBindBufferMemory(dev, *buf, *mem, 0));
    return 0;
}

int main(int argc, char **argv) {
    VkDeviceSize N = (argc > 1) ? (VkDeviceSize)strtoull(argv[1], NULL, 0) : (256u*1024u);
    int do_clean = 0;
    for (int i = 1; i < argc; i++) if (!strcmp(argv[i], "clean")) do_clean = 1;
    N &= ~3ULL; if (N < 64) N = 64;
    setvbuf(stdout, NULL, _IONBF, 0);

    VkApplicationInfo app = { .sType=VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion=VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { .sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo=&app };
    VkInstance inst; VK(vkCreateInstance(&ici, NULL, &inst));
    uint32_t pdc = 1; VkPhysicalDevice pd; VK(vkEnumeratePhysicalDevices(inst, &pdc, &pd));

    uint32_t qfc = 0; vkGetPhysicalDeviceQueueFamilyProperties(pd, &qfc, NULL);
    if (qfc > 16) qfc = 16; VkQueueFamilyProperties qfp[16];
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qfc, qfp);
    uint32_t qf = 0;
    for (uint32_t i = 0; i < qfc; i++) if (qfp[i].queueFlags & VK_QUEUE_TRANSFER_BIT) { qf = i; break; }
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = { .sType=VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex=qf, .queueCount=1, .pQueuePriorities=&prio };
    VkDeviceCreateInfo dci = { .sType=VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount=1, .pQueueCreateInfos=&qci };
    VkDevice dev; VK(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue q; vkGetDeviceQueue(dev, qf, 0, &q);

    VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    int hostType = -1; VkMemoryPropertyFlags hf = 0;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        VkMemoryPropertyFlags f = mp.memoryTypes[i].propertyFlags;
        if (f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) { hostType = (int)i; hf = f; break; }
    }
    if (hostType < 0) { fprintf(stderr, "no host-visible memory type\n"); return 1; }
    uint32_t nwords = (uint32_t)(N / 4);
    printf("N=%llu bytes (%u words, %llu x 16KiB host-pages) hostType=%d COHERENT=%d CACHED=%d clean=%d\n",
           (unsigned long long)N, nwords, (unsigned long long)(N/16384), hostType,
           !!(hf & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT), !!(hf & VK_MEMORY_PROPERTY_HOST_CACHED_BIT), do_clean);

    VkBuffer src, mid, dst2; VkDeviceMemory srcMem, midMem, dst2Mem;
    if (alloc_hostvis(dev, hostType, N, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, &src, &srcMem)) return 1;
    if (alloc_hostvis(dev, hostType, N, VK_BUFFER_USAGE_TRANSFER_SRC_BIT|VK_BUFFER_USAGE_TRANSFER_DST_BIT, &mid, &midMem)) return 1;
    if (alloc_hostvis(dev, hostType, N, VK_BUFFER_USAGE_TRANSFER_DST_BIT, &dst2, &dst2Mem)) return 1;

    void *smap; VK(vkMapMemory(dev, srcMem, 0, N, 0, &smap));
    uint32_t *sw = (uint32_t*)smap;
    for (uint32_t i = 0; i < nwords; i++) sw[i] = PAT(i);
    if (do_clean) clean_dcache(smap, N);
    // Pre-poison dst2 so "GPU didn't write" is distinguishable from "GPU wrote zero".
    void *dmap; VK(vkMapMemory(dev, dst2Mem, 0, N, 0, &dmap));
    memset(dmap, 0x5A, N);
    clean_dcache(dmap, N);

    VkCommandPoolCreateInfo cpci = { .sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex=qf };
    VkCommandPool pool; VK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
    VkCommandBufferAllocateInfo cbai = { .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool=pool, .level=VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount=1 };
    VkCommandBufferBeginInfo cbbi = { .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    VkBufferCopy region = { .srcOffset=0, .dstOffset=0, .size=N };

    VkCommandBuffer cb; VK(vkAllocateCommandBuffers(dev, &cbai, &cb));
    VK(vkBeginCommandBuffer(cb, &cbbi));
    vkCmdCopyBuffer(cb, src, mid, 1, &region);
    VkMemoryBarrier mb = { .sType=VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask=VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask=VK_ACCESS_TRANSFER_READ_BIT };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
    vkCmdCopyBuffer(cb, mid, dst2, 1, &region);
    VK(vkEndCommandBuffer(cb));
    VkSubmitInfo si = { .sType=VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount=1, .pCommandBuffers=&cb };
    VK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE)); VK(vkQueueWaitIdle(q));

    // dst2 is GPU-written → coherent to read. Each word == what the GPU read out of guest-written src.
    clean_dcache(dmap, N);
    __asm__ volatile("dsb ish" ::: "memory");
    uint32_t *rw = (uint32_t*)dmap;
    int firstBad = -1; uint32_t good = 0, zero = 0, poison = 0, other = 0;
    for (uint32_t i = 0; i < nwords; i++) {
        uint32_t v = rw[i];
        if (v == PAT(i)) { good++; continue; }
        if (firstBad < 0) firstBad = (int)i;
        if (v == 0) zero++; else if (v == 0x5A5A5A5Au) poison++; else other++;
    }
    printf("good=%u zero=%u poison=%u other=%u  (of %u words)\n", good, zero, poison, other, nwords);
    if (firstBad < 0) {
        printf("RESULT: FULLY COHERENT across all %llu bytes — NOT an offset/size boundary.\n", (unsigned long long)N);
    } else {
        printf("RESULT: first stale word at index %d = BYTE OFFSET %d (0x%x); got 0x%08x expected 0x%08x\n",
               firstBad, firstBad*4, firstBad*4, rw[firstBad], PAT((uint32_t)firstBad));
    }
    // Per-16KiB-page coherency map (each char: '#'=fully good, '.'=fully stale, digit=tenths good).
    uint32_t pageWords = 16384/4, npages = (nwords + pageWords - 1)/pageWords;
    printf("page map (16KiB each, #=ok .=stale): ");
    for (uint32_t p = 0; p < npages; p++) {
        uint32_t g = 0, t = 0;
        for (uint32_t i = p*pageWords; i < (p+1)*pageWords && i < nwords; i++, t++)
            if (rw[i] == PAT(i)) g++;
        char c = (g==t) ? '#' : (g==0) ? '.' : (char)('0' + (g*10/t));
        putchar(c);
    }
    putchar('\n');
    return 0;
}
