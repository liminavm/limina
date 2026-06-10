// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* #32 deep-fix oracle: can venus create/alloc/bind a DEPTH-STENCIL image the way
 * zink would for the window framebuffer's Z24S8 (substituted to D32S8) attachment?
 *
 * Distinguishes HOST-side failure (vkCreateImage/Alloc/Bind through venus/vkr/MoltenVK
 * fails -> fix in our host stack) from GUEST-side logic (raw Vulkan works -> the gap is
 * in zink/mesa-st before it ever reaches venus).
 *
 * Build (guest): gcc -o vkds vkds.c -lvulkan
 * Run: VK_ICD_FILENAMES=/opt/mesa-zink/share/vulkan/icd.d/virtio_icd.aarch64.json \
 *      LD_LIBRARY_PATH=/opt/mesa-zink/lib64 ./vkds
 */
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan.h>

static const char *fmt_name(VkFormat f) {
    switch (f) {
    case VK_FORMAT_D24_UNORM_S8_UINT: return "D24_UNORM_S8_UINT";
    case VK_FORMAT_D32_SFLOAT_S8_UINT: return "D32_SFLOAT_S8_UINT";
    case VK_FORMAT_S8_UINT: return "S8_UINT";
    case VK_FORMAT_D32_SFLOAT: return "D32_SFLOAT";
    default: return "?";
    }
}

static void try_ds_image(VkPhysicalDevice pd, VkDevice dev, VkFormat fmt,
                         VkImageUsageFlags usage) {
    printf("== %s (fmt=%d) usage=0x%x\n", fmt_name(fmt), fmt, usage);

    VkFormatProperties fp;
    vkGetPhysicalDeviceFormatProperties(pd, fmt, &fp);
    printf("  fmtprops: optimal=0x%x linear=0x%x\n",
           fp.optimalTilingFeatures, fp.linearTilingFeatures);

    VkPhysicalDeviceImageFormatInfo2 ifi = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
        .format = fmt,
        .type = VK_IMAGE_TYPE_2D,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = usage,
    };
    VkImageFormatProperties2 ifp = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
    };
    VkResult r = vkGetPhysicalDeviceImageFormatProperties2(pd, &ifi, &ifp);
    printf("  IFP2: %d%s\n", r, r ? " (UNSUPPORTED)" : "");
    if (r)
        return;

    VkImageCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = fmt,
        .extent = { 256, 256, 1 },
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = usage,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkImage img;
    r = vkCreateImage(dev, &ici, NULL, &img);
    printf("  vkCreateImage: %d\n", r);
    if (r)
        return;

    VkMemoryRequirements mr;
    vkGetImageMemoryRequirements(dev, img, &mr);
    printf("  memreq: size=%llu align=%llu typeBits=0x%x\n",
           (unsigned long long)mr.size, (unsigned long long)mr.alignment,
           mr.memoryTypeBits);

    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    int picked = -1;
    for (unsigned i = 0; i < mp.memoryTypeCount; i++) {
        if ((mr.memoryTypeBits & (1u << i)) &&
            (mp.memoryTypes[i].propertyFlags &
             VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) {
            picked = (int)i;
            break;
        }
    }
    if (picked < 0) {
        /* fall back to any allowed type */
        for (unsigned i = 0; i < mp.memoryTypeCount; i++)
            if (mr.memoryTypeBits & (1u << i)) { picked = (int)i; break; }
    }
    printf("  memtype picked=%d flags=0x%x\n", picked,
           picked >= 0 ? mp.memoryTypes[picked].propertyFlags : 0);

    VkMemoryDedicatedAllocateInfo ded = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .image = img,
    };
    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &ded,
        .allocationSize = mr.size,
        .memoryTypeIndex = (uint32_t)picked,
    };
    VkDeviceMemory mem;
    r = vkAllocateMemory(dev, &mai, NULL, &mem);
    printf("  vkAllocateMemory (dedicated): %d\n", r);
    if (!r) {
        r = vkBindImageMemory(dev, img, mem, 0);
        printf("  vkBindImageMemory: %d  %s\n", r, r ? "FAIL" : "OK ALL GOOD");
        vkFreeMemory(dev, mem, NULL);
    } else {
        /* retry non-dedicated */
        mai.pNext = NULL;
        r = vkAllocateMemory(dev, &mai, NULL, &mem);
        printf("  vkAllocateMemory (plain): %d\n", r);
        if (!r) {
            r = vkBindImageMemory(dev, img, mem, 0);
            printf("  vkBindImageMemory: %d  %s\n", r, r ? "FAIL" : "OK ALL GOOD");
            vkFreeMemory(dev, mem, NULL);
        }
    }
    vkDestroyImage(dev, img, NULL);
}

int main(void) {
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .apiVersion = VK_API_VERSION_1_2,
    };
    VkInstanceCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance inst;
    VkResult r = vkCreateInstance(&ici, NULL, &inst);
    if (r) { printf("vkCreateInstance: %d\n", r); return 1; }

    uint32_t n = 1;
    VkPhysicalDevice pd;
    vkEnumeratePhysicalDevices(inst, &n, &pd);
    if (!n) { printf("no device\n"); return 1; }
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    printf("device: %s\n", props.deviceName);

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
    r = vkCreateDevice(pd, &dci, NULL, &dev);
    if (r) { printf("vkCreateDevice: %d\n", r); return 1; }

    /* zink's window depth-stencil shape: DS attachment + sampled + transfer both ways */
    VkImageUsageFlags zink_usage = VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT |
                                   VK_IMAGE_USAGE_SAMPLED_BIT |
                                   VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
                                   VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    try_ds_image(pd, dev, VK_FORMAT_D32_SFLOAT_S8_UINT, zink_usage);
    try_ds_image(pd, dev, VK_FORMAT_S8_UINT, zink_usage);
    try_ds_image(pd, dev, VK_FORMAT_D24_UNORM_S8_UINT, zink_usage);
    /* minimal usage variant in case the extra bits are what kills it */
    try_ds_image(pd, dev, VK_FORMAT_D32_SFLOAT_S8_UINT,
                 VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT);

    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    return 0;
}
