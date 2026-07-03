// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * vkmap-stress — prove venus host-visible memory MAPPING works on a 4 KiB guest.
 *
 * The 16 KiB-host blob-map alignment bug (memory limina-blob-map-16k-alignment):
 * odd-size / packed-offset host-visible blobs couldn't be hv_vm_map'd, so venus on a
 * stock 4 KiB guest failed vkMapMemory unpredictably once the window packing went
 * misaligned. Fixed by libkrun 0043 (size rounding, host) + the 16 KiB-aligned
 * window allocation in the guest kernel (patches/linux/0004 in-tree, or the
 * limina-virtio-gpu DKMS module on stock kernels).
 *
 * This deliberately allocates MANY odd-size host-visible VkDeviceMemory objects
 * (sizes cycle through every 4 KiB residue mod 16 KiB, with a live working set so
 * the window actually packs), maps each, writes both ends, reads them back, and
 * frees half the set each round to force drm_mm hole reuse. Pre-fix this fails
 * within the first few allocations; post-fix all N complete.
 *
 * Build (in the guest): gcc -O2 -o vkmap-stress vkmap-stress.c -lvulkan
 *   (dnf install gcc vulkan-loader-devel vulkan-headers)
 * Run: ./vkmap-stress [device-name-substring]   # default: pick a "Virtio" device
 * Exit 0 + "PASS" line = every allocation mapped and verified.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define ROUNDS 4
#define LIVE_PER_ROUND 24

static const char *want = "Virtio";

static VkDeviceMemory alloc_and_verify(VkDevice dev, uint32_t type, VkDeviceSize size,
                                       int idx)
{
    VkMemoryAllocateInfo ai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = size,
        .memoryTypeIndex = type,
    };
    VkDeviceMemory mem;
    VkResult r = vkAllocateMemory(dev, &ai, NULL, &mem);
    if (r != VK_SUCCESS) {
        printf("FAIL: vkAllocateMemory #%d size=0x%llx -> %d\n", idx,
               (unsigned long long)size, r);
        exit(1);
    }
    void *p;
    r = vkMapMemory(dev, mem, 0, size, 0, &p);
    if (r != VK_SUCCESS) {
        printf("FAIL: vkMapMemory #%d size=0x%llx -> %d (the alignment bug)\n", idx,
               (unsigned long long)size, r);
        exit(1);
    }
    unsigned char *b = p;
    b[0] = 0xa5;
    b[size - 1] = 0x5a;
    if (b[0] != 0xa5 || b[size - 1] != 0x5a) {
        printf("FAIL: readback #%d size=0x%llx\n", idx, (unsigned long long)size);
        exit(1);
    }
    vkUnmapMemory(dev, mem);
    return mem;
}

int main(int argc, char **argv)
{
    if (argc > 1)
        want = argv[1];

    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "vkmap-stress",
        .apiVersion = VK_API_VERSION_1_1,
    };
    VkInstanceCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance inst;
    if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) {
        printf("FAIL: vkCreateInstance\n");
        return 1;
    }

    uint32_t n = 0;
    vkEnumeratePhysicalDevices(inst, &n, NULL);
    if (n == 0) {
        printf("FAIL: no Vulkan physical devices\n");
        return 1;
    }
    VkPhysicalDevice devs[16];
    if (n > 16)
        n = 16;
    vkEnumeratePhysicalDevices(inst, &n, devs);

    VkPhysicalDevice phys = VK_NULL_HANDLE;
    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceProperties pr;
        vkGetPhysicalDeviceProperties(devs[i], &pr);
        printf("device %u: %s\n", i, pr.deviceName);
        if (!phys && strstr(pr.deviceName, want))
            phys = devs[i];
    }
    if (!phys) {
        printf("SKIP: no device matching \"%s\"\n", want);
        return 2;
    }
    VkPhysicalDeviceProperties pr;
    vkGetPhysicalDeviceProperties(phys, &pr);
    printf("using: %s\n", pr.deviceName);

    /* Any HOST_VISIBLE|HOST_COHERENT type. */
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(phys, &mp);
    uint32_t type = UINT32_MAX;
    VkMemoryPropertyFlags need =
        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((mp.memoryTypes[i].propertyFlags & need) == need) {
            type = i;
            break;
        }
    if (type == UINT32_MAX) {
        printf("FAIL: no HOST_VISIBLE|HOST_COHERENT memory type\n");
        return 1;
    }

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = 0,
        .queueCount = 1,
        .pQueuePriorities = &prio,
    };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
    };
    VkDevice dev;
    if (vkCreateDevice(phys, &dci, NULL, &dev) != VK_SUCCESS) {
        printf("FAIL: vkCreateDevice\n");
        return 1;
    }

    /* Odd sizes cycling every 4 KiB residue mod 16 KiB; a live working set packs the
     * window, freeing half each round forces hole reuse. */
    VkDeviceMemory live[LIVE_PER_ROUND] = {0};
    int total = 0;
    for (int round = 0; round < ROUNDS; round++) {
        for (int i = 0; i < LIVE_PER_ROUND; i++) {
            if (live[i] && (i % 2 == 0)) {
                vkFreeMemory(dev, live[i], NULL);
                live[i] = VK_NULL_HANDLE;
            }
        }
        for (int i = 0; i < LIVE_PER_ROUND; i++) {
            if (live[i])
                continue;
            VkDeviceSize size = 0x1000 * (1 + (total % 33)); /* 4 KiB .. 132 KiB */
            live[i] = alloc_and_verify(dev, type, size, total);
            total++;
        }
    }
    printf("PASS: %d host-visible allocations mapped + verified on %s\n", total,
           pr.deviceName);
    return 0;
}
