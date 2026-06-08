// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// #28 write-direction decider: does a guest cache CLEAN (dc cvac) make the guest's writes to a
// host-visible Vulkan buffer visible to the HOST GPU/CPU?
//
// Allocates a HOST_VISIBLE buffer (the same vkr_mtl_shm_alloc path zink uses), writes a known
// pattern, optionally `dc cvac`s the range (EL0-permitted clean-to-PoC), then vkCmdCopyBuffer's it
// so the host MoltenVK touches the source. The instrumented host MoltenVK prints [LIMINA-COPY] =
// firstNonzeroByte + first floats of what the HOST sees in the source buffer.
//   run `./vkcoh`        (no clean) → expect host sees ZERO  (firstNonzeroByte=-1) = the bug
//   run `./vkcoh clean`  (dc cvac)  → if host sees the pattern (1.0 2.0 3.0...) ⇒ guest-clean is the fix
// Build (guest): gcc vkcoh.c -o vkcoh -lvulkan
// Run (guest):  VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
//               VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback ./vkcoh [clean]
// Host: boot with the instrumented MoltenVK + LIMINA_COPY_DUMP=1; grep worker log for [LIMINA-COPY].
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define VK(x) do { VkResult _r=(x); if(_r){fprintf(stderr,#x" = %d\n",_r); return 1;} } while(0)

// Clean (write-back) the data cache by VA to the Point of Coherency. `dc cvac` is permitted at EL0
// on Linux (SCTLR_EL1.UCI). dsb ish ensures completion before the GPU reads.
static void clean_dcache(void *addr, size_t len) {
    uintptr_t start = (uintptr_t)addr & ~63UL;          // 64B cache line on Apple Silicon
    uintptr_t end   = (uintptr_t)addr + len;
    for (uintptr_t p = start; p < end; p += 64)
        __asm__ volatile("dc cvac, %0" :: "r"(p) : "memory");
    __asm__ volatile("dsb ish" ::: "memory");
}

int main(int argc, char **argv) {
    int do_clean = (argc > 1 && strcmp(argv[1], "clean") == 0);
    setvbuf(stdout, NULL, _IONBF, 0);

    VkApplicationInfo app = { .sType=VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion=VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { .sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo=&app };
    VkInstance inst; VK(vkCreateInstance(&ici, NULL, &inst));

    uint32_t pdc = 1; VkPhysicalDevice pd;
    VK(vkEnumeratePhysicalDevices(inst, &pdc, &pd));
    if (pdc < 1) { fprintf(stderr, "no physical device\n"); return 1; }

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
    int hostType = -1; VkMemoryPropertyFlags hostFlags = 0;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        VkMemoryPropertyFlags f = mp.memoryTypes[i].propertyFlags;
        if (f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) { hostType = (int)i; hostFlags = f; break; }
    }
    if (hostType < 0) { fprintf(stderr, "no host-visible memory type\n"); return 1; }
    printf("hostType=%d flags=0x%x COHERENT=%d CACHED=%d\n", hostType, hostFlags,
           !!(hostFlags & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT), !!(hostFlags & VK_MEMORY_PROPERTY_HOST_CACHED_BIT));

    const VkDeviceSize N = 4096;
    VkBufferCreateInfo sbci = { .sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size=N, .usage=VK_BUFFER_USAGE_TRANSFER_SRC_BIT, .sharingMode=VK_SHARING_MODE_EXCLUSIVE };
    VkBuffer src; VK(vkCreateBuffer(dev, &sbci, NULL, &src));
    VkMemoryRequirements smr; vkGetBufferMemoryRequirements(dev, src, &smr);
    VkMemoryAllocateInfo smai = { .sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize=smr.size, .memoryTypeIndex=(uint32_t)hostType };
    VkDeviceMemory srcMem; VK(vkAllocateMemory(dev, &smai, NULL, &srcMem));
    VK(vkBindBufferMemory(dev, src, srcMem, 0));

    void *map; VK(vkMapMemory(dev, srcMem, 0, N, 0, &map));
    memset(map, 0xAB, N);
    float *fp = (float*)map; for (int i = 0; i < 6; i++) fp[i] = (float)(i + 1);  // 1.0 .. 6.0
    if (do_clean) { clean_dcache(map, N); printf("did dc cvac on [%p,%llu)\n", map, (unsigned long long)N); }
    else          { printf("NO clean\n"); }
    printf("guest readback f0..5: %.1f %.1f %.1f %.1f %.1f %.1f  byte[100]=0x%02x\n",
           fp[0],fp[1],fp[2],fp[3],fp[4],fp[5], ((unsigned char*)map)[100]);

    // Two more host-visible buffers: mid (GPU copies src->mid) and dst2 (GPU copies mid->dst2).
    // The RELIABLE oracle is the [LIMINA-COPY] dump at the SECOND copy's encode: its source is `mid`,
    // which is GPU-WRITTEN (CB1) and thus host-coherent (prior spike Finding 1). That shows what the
    // GPU actually read out of the guest-written `src`.
    VkBuffer mid, dst2; VkDeviceMemory midMem, dst2Mem;
    VkBufferCreateInfo mbci = { .sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size=N, .usage=VK_BUFFER_USAGE_TRANSFER_SRC_BIT|VK_BUFFER_USAGE_TRANSFER_DST_BIT, .sharingMode=VK_SHARING_MODE_EXCLUSIVE };
    VK(vkCreateBuffer(dev, &mbci, NULL, &mid));
    VkMemoryRequirements mmr; vkGetBufferMemoryRequirements(dev, mid, &mmr);
    VkMemoryAllocateInfo mmai = { .sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize=mmr.size, .memoryTypeIndex=(uint32_t)hostType };
    VK(vkAllocateMemory(dev, &mmai, NULL, &midMem)); VK(vkBindBufferMemory(dev, mid, midMem, 0));
    VkBufferCreateInfo d2bci = { .sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size=N, .usage=VK_BUFFER_USAGE_TRANSFER_DST_BIT, .sharingMode=VK_SHARING_MODE_EXCLUSIVE };
    VK(vkCreateBuffer(dev, &d2bci, NULL, &dst2));
    VkMemoryRequirements d2mr; vkGetBufferMemoryRequirements(dev, dst2, &d2mr);
    VkMemoryAllocateInfo d2mai = { .sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize=d2mr.size, .memoryTypeIndex=(uint32_t)hostType };
    VK(vkAllocateMemory(dev, &d2mai, NULL, &dst2Mem)); VK(vkBindBufferMemory(dev, dst2, dst2Mem, 0));

    VkCommandPoolCreateInfo cpci = { .sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex=qf };
    VkCommandPool pool; VK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
    VkCommandBufferAllocateInfo cbai = { .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool=pool, .level=VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount=1 };
    VkCommandBufferBeginInfo cbbi = { .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    VkBufferCopy region = { .srcOffset=0, .dstOffset=0, .size=N };

    // CB1: src -> mid  (the [LIMINA-COPY] dump here reads src = guest-written, host view MAY be stale)
    VkCommandBuffer cb1; VK(vkAllocateCommandBuffers(dev, &cbai, &cb1));
    VK(vkBeginCommandBuffer(cb1, &cbbi)); vkCmdCopyBuffer(cb1, src, mid, 1, &region); VK(vkEndCommandBuffer(cb1));
    VkSubmitInfo si1 = { .sType=VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount=1, .pCommandBuffers=&cb1 };
    VK(vkQueueSubmit(q, 1, &si1, VK_NULL_HANDLE)); VK(vkQueueWaitIdle(q));

    // CB2: mid -> dst2 (the [LIMINA-COPY] dump here reads mid = GPU-written => RELIABLE oracle)
    VkCommandBuffer cb2; VK(vkAllocateCommandBuffers(dev, &cbai, &cb2));
    VK(vkBeginCommandBuffer(cb2, &cbbi)); vkCmdCopyBuffer(cb2, mid, dst2, 1, &region); VK(vkEndCommandBuffer(cb2));
    VkSubmitInfo si2 = { .sType=VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount=1, .pCommandBuffers=&cb2 };
    VK(vkQueueSubmit(q, 1, &si2, VK_NULL_HANDLE)); VK(vkQueueWaitIdle(q));

    printf("submitted+waited (clean=%d) — 2nd [LIMINA-COPY] (mid, GPU-written) is the reliable oracle\n", do_clean);
    return 0;
}
