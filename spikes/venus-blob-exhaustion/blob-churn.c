// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// blob-churn.c — venus blob-exhaustion DECISION PROBE (spikes/venus-blob-exhaustion)
//
// Purpose: determine whether the guest venus driver degrades GRACEFULLY when a HOST3D
// *device-memory* blob create fails on the host, or whether it turns the failure into a
// process abort (the production "wedged until reboot" symptom).
//
// It allocates a fresh blob-backed HOST_VISIBLE VkDeviceMemory + buffer each iteration and
// references it in a trivial copy submit (does NOT free — churns blob creates), checking
// EVERY VkResult and handling failure gracefully. Pair it with the host-side fault injection:
//
//   host worker env:  LIMINA_BLOB_CREATE_FAIL_AFTER=<M>   (optionally ,FAIL_FOR=<K>)
//   guest run:        VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
//                     ./blob-churn [iters] [bytes]      (defaults: 4096 x 65536)
//
// EXPECTED OUTCOMES (this is the decision):
//   * Iteration M's vkAllocateMemory returns VK_ERROR_OUT_OF_DEVICE_MEMORY and the probe
//     prints "graceful: got %d at iter M" and exits 0  => the mainline device-memory create
//     path ALREADY degrades. The production abort therefore comes from a path this probe does
//     NOT exercise (WSI present-blit / scanout re-import, or MAP-time, or reply-shmem) — pivot
//     the repro to the real desktop and/or the map-time / blob_id==0 injection modes.
//   * The probe SIGABRTs (signal, no "graceful" line) => device-memory create-injection DOES
//     reproduce the abort; aim the fix there.
// Either way the result is decisive; a clean exit is INFORMATION, not vindication.
//
// Build (guest): gcc -O0 -g blob-churn.c -o blob-churn -lvulkan
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define TRY(x, label) do { VkResult _r=(x); if(_r){ \
    fprintf(stderr, "graceful: %s returned %d at iter %d\n", label, _r, i); \
    fflush(stderr); return (_r==VK_ERROR_OUT_OF_DEVICE_MEMORY||_r==VK_ERROR_OUT_OF_HOST_MEMORY)?0:2; } } while(0)
#define MUST(x) do { VkResult _r=(x); if(_r){ fprintf(stderr, #x" = %d\n", _r); return 3; } } while(0)

int main(int argc, char **argv) {
    int iters = argc > 1 ? atoi(argv[1]) : 4096;
    VkDeviceSize bytes = argc > 2 ? (VkDeviceSize)atoll(argv[2]) : 65536;
    setbuf(stdout, NULL); setbuf(stderr, NULL);

    VkApplicationInfo app = { .sType=VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion=VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { .sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo=&app };
    VkInstance inst; MUST(vkCreateInstance(&ici, NULL, &inst));
    uint32_t npd=1; VkPhysicalDevice pd; MUST(vkEnumeratePhysicalDevices(inst, &npd, &pd));
    if (!npd) { fprintf(stderr, "no venus physical device (empty enumerate)\n"); return 4; }

    float prio=1.0f; uint32_t qf=0;
    VkDeviceQueueCreateInfo qci = { .sType=VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex=qf, .queueCount=1, .pQueuePriorities=&prio };
    VkDeviceCreateInfo dci = { .sType=VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount=1, .pQueueCreateInfos=&qci };
    VkDevice dev; MUST(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue q; vkGetDeviceQueue(dev, qf, 0, &q);

    VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    int htype = -1;
    for (uint32_t t=0; t<mp.memoryTypeCount; t++)
        if (mp.memoryTypes[t].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) { htype=(int)t; break; }
    if (htype < 0) { fprintf(stderr, "no HOST_VISIBLE memory type\n"); return 5; }

    VkCommandPool pool; VkCommandPoolCreateInfo pci = { .sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex=qf };
    MUST(vkCreateCommandPool(dev, &pci, NULL, &pool));

    fprintf(stderr, "blob-churn: %d iters x %llu bytes, host_visible type=%d\n",
            iters, (unsigned long long)bytes, htype);

    // Churn: each iteration allocates a fresh blob-backed device memory + buffer and references
    // it in a submit. No frees -> unbounded blob creates -> crosses the injection threshold.
    for (int i=0; i<iters; i++) {
        VkBufferCreateInfo bci = { .sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size=bytes,
            .usage=VK_BUFFER_USAGE_TRANSFER_SRC_BIT|VK_BUFFER_USAGE_TRANSFER_DST_BIT, .sharingMode=VK_SHARING_MODE_EXCLUSIVE };
        VkBuffer buf; TRY(vkCreateBuffer(dev, &bci, NULL, &buf), "vkCreateBuffer");
        VkMemoryRequirements mr; vkGetBufferMemoryRequirements(dev, buf, &mr);
        VkMemoryAllocateInfo mai = { .sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize=mr.size, .memoryTypeIndex=(uint32_t)htype };
        VkDeviceMemory mem; TRY(vkAllocateMemory(dev, &mai, NULL, &mem), "vkAllocateMemory");
        TRY(vkBindBufferMemory(dev, buf, mem, 0), "vkBindBufferMemory");

        // touch it (map/write) — the present-blit shadow is CPU-written each frame
        void *p; TRY(vkMapMemory(dev, mem, 0, bytes, 0, &p), "vkMapMemory");
        memset(p, i & 0xff, bytes); vkUnmapMemory(dev, mem);

        // reference it in a submit (a self-copy) so a failed create can surface as a dangling ref
        VkCommandBuffer cb; VkCommandBufferAllocateInfo cbi = { .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            .commandPool=pool, .level=VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount=1 };
        TRY(vkAllocateCommandBuffers(dev, &cbi, &cb), "vkAllocateCommandBuffers");
        VkCommandBufferBeginInfo beg = { .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
        MUST(vkBeginCommandBuffer(cb, &beg));
        VkBufferCopy region = { .srcOffset=0, .dstOffset=0, .size=bytes };
        vkCmdCopyBuffer(cb, buf, buf, 1, &region);
        MUST(vkEndCommandBuffer(cb));
        VkSubmitInfo si = { .sType=VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount=1, .pCommandBuffers=&cb };
        TRY(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE), "vkQueueSubmit");
        TRY(vkQueueWaitIdle(q), "vkQueueWaitIdle");

        if ((i % 256) == 0) fprintf(stderr, "  iter %d ok\n", i);
    }
    fprintf(stderr, "completed all %d iters with no failure (raise iters or lower FAIL_AFTER)\n", iters);
    return 0;
}
