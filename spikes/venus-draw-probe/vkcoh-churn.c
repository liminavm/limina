// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// #28 follow-up 2 — the CONCURRENCY/ORDERING decider for the seated-desktop "stale vertex" defect.
//
// vkcoh-offset.c proved a large STATIC single-shot host-visible buffer is fully coherent at every
// offset. So the desktop defect (most UI quads draw with one garbage vertex; windows render only
// their first few items) is NOT a size/offset boundary. The remaining suspect is producer->consumer
// ordering UNDER CONCURRENCY: mesa/zink streams vertices into a persistently-mapped, bump-allocated
// host-visible buffer and fires many pipelined submits while the GPU is still executing earlier ones.
// If the GPU fetches a region the guest CPU wrote but which is not yet ordered-visible at execution
// time, that region reads stale → degenerate geometry.
//
// This probe mimics u_upload_mgr: ONE persistent host-visible src, F records bump-allocated into it,
// each record written then immediately submitted as its own copy (NO per-iter wait — GPU executes
// record f while the CPU writes+submits record f+1), wait ONCE at the end, then verify every record
// holds ITS OWN tag (GPU-written dst is coherent to read back). A stale/torn record = the bug.
//
// Build (guest): gcc vkcoh-churn.c -o vkcoh-churn -lvulkan
// Run   (guest): VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
//                VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback \
//                ./vkcoh-churn [records] [recBytes] [clean]
//   defaults: 256 records x 4096 bytes = 1 MiB.  clean = dc cvac each record before its submit.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define VK(x) do { VkResult _r=(x); if(_r){fprintf(stderr,#x" = %d\n",_r); return 1;} } while(0)
// word value at record f, word j within the record: top 16 bits = f, low 16 = j. Unique per slot.
#define TAG(f,j) (((uint32_t)(f) << 16) | ((uint32_t)(j) & 0xFFFFu))

static void clean_dcache(void *addr, size_t len) {
    uintptr_t start = (uintptr_t)addr & ~63UL;
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
    uint32_t F   = (argc > 1) ? (uint32_t)strtoul(argv[1], NULL, 0) : 256;
    uint32_t REC = (argc > 2) ? (uint32_t)strtoul(argv[2], NULL, 0) : 4096;
    int do_clean = 0;
    for (int i = 1; i < argc; i++) if (!strcmp(argv[i], "clean")) do_clean = 1;
    REC &= ~3u; if (REC < 64) REC = 64;
    VkDeviceSize N = (VkDeviceSize)F * REC;
    uint32_t rwords = REC / 4;
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
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) { hostType=(int)i; hf=mp.memoryTypes[i].propertyFlags; break; }
    if (hostType < 0) { fprintf(stderr, "no host-visible memory type\n"); return 1; }
    printf("F=%u records x REC=%u bytes = %llu total, COHERENT=%d CACHED=%d clean=%d\n",
           F, REC, (unsigned long long)N, !!(hf&VK_MEMORY_PROPERTY_HOST_COHERENT_BIT), !!(hf&VK_MEMORY_PROPERTY_HOST_CACHED_BIT), do_clean);

    VkBuffer src, dst; VkDeviceMemory srcMem, dstMem;
    if (alloc_hostvis(dev, hostType, N, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, &src, &srcMem)) return 1;
    if (alloc_hostvis(dev, hostType, N, VK_BUFFER_USAGE_TRANSFER_DST_BIT, &dst, &dstMem)) return 1;
    void *smap, *dmap;
    VK(vkMapMemory(dev, srcMem, 0, N, 0, &smap));    // persistent map, like u_upload_mgr
    VK(vkMapMemory(dev, dstMem, 0, N, 0, &dmap));
    memset(dmap, 0x5A, N); clean_dcache(dmap, N);

    VkCommandPoolCreateInfo cpci = { .sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags=VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex=qf };
    VkCommandPool pool; VK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
    VkCommandBufferAllocateInfo cbai = { .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool=pool, .level=VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount=F };
    VkCommandBuffer *cbs = calloc(F, sizeof(*cbs));
    VK(vkAllocateCommandBuffers(dev, &cbai, cbs));
    VkCommandBufferBeginInfo cbbi = { .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };

    // Bump-allocate: write record f, submit its copy, DON'T wait — GPU runs f while CPU does f+1.
    uint32_t *sw = (uint32_t*)smap;
    for (uint32_t f = 0; f < F; f++) {
        uint32_t base = f * rwords;
        for (uint32_t j = 0; j < rwords; j++) sw[base + j] = TAG(f, j);
        if (do_clean) clean_dcache(&sw[base], REC);
        VkBufferCopy region = { .srcOffset=(VkDeviceSize)f*REC, .dstOffset=(VkDeviceSize)f*REC, .size=REC };
        VK(vkBeginCommandBuffer(cbs[f], &cbbi));
        vkCmdCopyBuffer(cbs[f], src, dst, 1, &region);
        VK(vkEndCommandBuffer(cbs[f]));
        VkSubmitInfo si = { .sType=VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount=1, .pCommandBuffers=&cbs[f] };
        VK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE));   // no wait → pipelined / concurrent
    }
    VK(vkQueueWaitIdle(q));

    clean_dcache(dmap, N); __asm__ volatile("dsb ish" ::: "memory");
    uint32_t *rw = (uint32_t*)dmap;
    uint32_t badRecords = 0, zeroWords = 0, poisonWords = 0, crossWords = 0; int firstBadRec = -1;
    for (uint32_t f = 0; f < F; f++) {
        uint32_t base = f * rwords, bad = 0;
        for (uint32_t j = 0; j < rwords; j++) {
            uint32_t v = rw[base + j];
            if (v == TAG(f, j)) continue;
            bad++;
            if (v == 0) zeroWords++; else if (v == 0x5A5A5A5Au) poisonWords++; else crossWords++;
        }
        if (bad) { badRecords++; if (firstBadRec < 0) firstBadRec = (int)f; }
    }
    printf("badRecords=%u/%u  zeroWords=%u poisonWords=%u crossWords=%u\n",
           badRecords, F, zeroWords, poisonWords, crossWords);
    if (!badRecords) printf("RESULT: FULLY COHERENT under pipelined churn — ordering is NOT the defect.\n");
    else {
        printf("RESULT: STALE under churn — first bad record=%d (byte 0x%x). Sample word0: got 0x%08x expected 0x%08x\n",
               firstBadRec, firstBadRec*REC, rw[firstBadRec*rwords], TAG((uint32_t)firstBadRec, 0));
    }
    return 0;
}
