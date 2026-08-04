// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* kk-modifier-probe.c — standalone acceptance probe for KK's
 * VK_EXT_image_drm_format_modifier (LINEAR-only) implementation.
 *
 * Drives KosmicKrisp's ICD DIRECTLY (dlopen + vk_icdGetInstanceProcAddr) per
 * the recorded dispatch trap: the system loader does not route extension
 * commands to KK in a standalone process. KK replays command buffers at
 * vkQueueSubmit, so the render test really submits and waits.
 *
 * What it proves, in order:
 *   1. the device advertises VK_EXT_image_drm_format_modifier
 *   2. the modifier list for B8G8R8A8_UNORM is exactly [LINEAR], 1 plane,
 *      with COLOR_ATTACHMENT in its features
 *   3. image-format-properties accepts LINEAR (+attachment usage) and rejects
 *      a non-LINEAR modifier and a depth format
 *   4. a LIST=[LINEAR] create succeeds at an odd width (250) and both
 *      GetImageDrmFormatModifierPropertiesEXT and the MEMORY_PLANE_0
 *      subresource layout answer truthfully (pitch >= 1000, aligned)
 *   5. the June-2026 nil-encoder class: a render pass (dynamic rendering,
 *      LOAD_OP_CLEAR) targeting the linear modifier image completes, and the
 *      CPU readback at the REPORTED pitch sees the clear color in all four
 *      corners — the layout claim is true end to end
 *   6. an EXPLICIT create with the reported pitch succeeds and round-trips it;
 *      an EXPLICIT create with a bogus (too-narrow) pitch also succeeds —
 *      invalid plane layouts are app-UB, defined by KK as adopt-the-computed-
 *      pitch (loudly), so the transition-era guest (mesa 0010(b) fabricates
 *      tight-packed pitches) keeps working — and the layout query reports the
 *      TRUE pitch, never the bogus one
 *
 * Build & run:
 *   clang -O0 -g -o /tmp/kk-modifier-probe \
 *     spikes/modifier-necessity/kk-modifier-probe.c -I /Volumes/mesa-cs/mesa/include
 *   /tmp/kk-modifier-probe   # optional arg: path to libvulkan_kosmickrisp.dylib
 */

#include <dlfcn.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#define DRM_FORMAT_MOD_LINEAR 0ull
#define DRM_FORMAT_MOD_BOGUS 0x00ffffffffffff42ull

static int failures = 0;

#define CHECK(cond, ...)                                                       \
   do {                                                                        \
      if (cond) {                                                              \
         printf("ok:   " __VA_ARGS__);                                         \
         printf("\n");                                                         \
      } else {                                                                 \
         printf("FAIL: " __VA_ARGS__);                                         \
         printf("\n");                                                         \
         failures++;                                                           \
      }                                                                        \
   } while (0)

#define DIE(...)                                                               \
   do {                                                                        \
      fprintf(stderr, "fatal: " __VA_ARGS__);                                  \
      fprintf(stderr, "\n");                                                   \
      exit(2);                                                                 \
   } while (0)

static PFN_vkGetInstanceProcAddr gipa;

#define ILOAD(inst, name) ((PFN_##name)gipa(inst, #name))

int
main(int argc, char **argv)
{
   const char *icd = argc > 1 ? argv[1]
                              : "/Volumes/mesa-cs/build-kk/src/kosmickrisp/"
                                "vulkan/libvulkan_kosmickrisp.dylib";
   void *dso = dlopen(icd, RTLD_NOW | RTLD_LOCAL);
   if (!dso)
      DIE("dlopen %s: %s", icd, dlerror());
   gipa = (PFN_vkGetInstanceProcAddr)dlsym(dso, "vk_icdGetInstanceProcAddr");
   if (!gipa)
      DIE("no vk_icdGetInstanceProcAddr in %s", icd);

   /* ---- instance / physical device ---- */
   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "kk-modifier-probe",
      .apiVersion = VK_API_VERSION_1_3,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst;
   VkResult r = ILOAD(NULL, vkCreateInstance)(&ici, NULL, &inst);
   if (r != VK_SUCCESS)
      DIE("vkCreateInstance: %d", r);

   uint32_t npd = 1;
   VkPhysicalDevice pd;
   r = ILOAD(inst, vkEnumeratePhysicalDevices)(inst, &npd, &pd);
   if (npd < 1)
      DIE("no physical device");

   /* 1: extension advertised */
   uint32_t next = 0;
   ILOAD(inst, vkEnumerateDeviceExtensionProperties)(pd, NULL, &next, NULL);
   VkExtensionProperties *exts = calloc(next, sizeof(*exts));
   ILOAD(inst, vkEnumerateDeviceExtensionProperties)(pd, NULL, &next, exts);
   int have_ext = 0;
   for (uint32_t i = 0; i < next; i++)
      if (!strcmp(exts[i].extensionName,
                  VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME))
         have_ext = 1;
   CHECK(have_ext, "VK_EXT_image_drm_format_modifier advertised");
   free(exts);

   /* 2: modifier list for B8G8R8A8_UNORM */
   VkDrmFormatModifierPropertiesListEXT mod_list = {
      .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
   };
   VkFormatProperties2 fp2 = {
      .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2,
      .pNext = &mod_list,
   };
   ILOAD(inst, vkGetPhysicalDeviceFormatProperties2)(pd, VK_FORMAT_B8G8R8A8_UNORM,
                                                     &fp2);
   CHECK(mod_list.drmFormatModifierCount == 1,
         "B8G8R8A8_UNORM offers exactly 1 modifier (got %u)",
         mod_list.drmFormatModifierCount);
   VkDrmFormatModifierPropertiesEXT mod_props = {0};
   mod_list.pDrmFormatModifierProperties = &mod_props;
   ILOAD(inst, vkGetPhysicalDeviceFormatProperties2)(pd, VK_FORMAT_B8G8R8A8_UNORM,
                                                     &fp2);
   CHECK(mod_props.drmFormatModifier == DRM_FORMAT_MOD_LINEAR &&
            mod_props.drmFormatModifierPlaneCount == 1,
         "the modifier is LINEAR with 1 plane (mod=0x%" PRIx64 " planes=%u)",
         mod_props.drmFormatModifier, mod_props.drmFormatModifierPlaneCount);
   CHECK(mod_props.drmFormatModifierTilingFeatures &
            VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT,
         "LINEAR modifier features include COLOR_ATTACHMENT (0x%x)",
         mod_props.drmFormatModifierTilingFeatures);

   /* depth format offers no modifiers */
   VkDrmFormatModifierPropertiesListEXT dlist = {
      .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
   };
   VkFormatProperties2 dfp2 = {
      .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2,
      .pNext = &dlist,
   };
   ILOAD(inst, vkGetPhysicalDeviceFormatProperties2)(pd, VK_FORMAT_D32_SFLOAT,
                                                     &dfp2);
   CHECK(dlist.drmFormatModifierCount == 0,
         "D32_SFLOAT offers no modifiers (got %u)", dlist.drmFormatModifierCount);

   /* 3: image-format-properties accept/reject */
   const VkImageUsageFlags usage =
      VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
      VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT |
      VK_IMAGE_USAGE_INPUT_ATTACHMENT_BIT;
   VkPhysicalDeviceImageDrmFormatModifierInfoEXT mod_info = {
      .sType =
         VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
      .drmFormatModifier = DRM_FORMAT_MOD_LINEAR,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkPhysicalDeviceImageFormatInfo2 ifi = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
      .pNext = &mod_info,
      .format = VK_FORMAT_B8G8R8A8_UNORM,
      .type = VK_IMAGE_TYPE_2D,
      .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
      .usage = usage,
   };
   VkImageFormatProperties2 ifp = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
   };
   r = ILOAD(inst, vkGetPhysicalDeviceImageFormatProperties2)(pd, &ifi, &ifp);
   CHECK(r == VK_SUCCESS,
         "LINEAR modifier + color-attachment usage accepted (r=%d)", r);

   mod_info.drmFormatModifier = DRM_FORMAT_MOD_BOGUS;
   r = ILOAD(inst, vkGetPhysicalDeviceImageFormatProperties2)(pd, &ifi, &ifp);
   CHECK(r == VK_ERROR_FORMAT_NOT_SUPPORTED,
         "non-LINEAR modifier rejected (r=%d)", r);
   mod_info.drmFormatModifier = DRM_FORMAT_MOD_LINEAR;

   ifi.format = VK_FORMAT_D32_SFLOAT;
   ifi.usage = VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT;
   r = ILOAD(inst, vkGetPhysicalDeviceImageFormatProperties2)(pd, &ifi, &ifp);
   CHECK(r == VK_ERROR_FORMAT_NOT_SUPPORTED,
         "depth format with modifier tiling rejected (r=%d)", r);

   /* ---- device ---- */
   const float prio = 1.0f;
   const VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   const char *dev_exts[] = {VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME};
   VkPhysicalDeviceVulkan13Features feat13 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
      .dynamicRendering = VK_TRUE,
   };
   const VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &feat13,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = dev_exts,
   };
   VkDevice dev;
   r = ILOAD(inst, vkCreateDevice)(pd, &dci, NULL, &dev);
   if (r != VK_SUCCESS)
      DIE("vkCreateDevice: %d", r);
   PFN_vkGetDeviceProcAddr gdpa = ILOAD(inst, vkGetDeviceProcAddr);
#define DLOAD(name) ((PFN_##name)gdpa(dev, #name))

   /* 4: LIST create at an odd width; truthful layout answers */
   const uint32_t W = 250, H = 131; /* 250*4 = 1000 — not 16-aligned */
   const uint64_t list_mods[] = {DRM_FORMAT_MOD_LINEAR};
   VkImageDrmFormatModifierListCreateInfoEXT list_ci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
      .drmFormatModifierCount = 1,
      .pDrmFormatModifiers = list_mods,
   };
   VkImageCreateInfo img_ci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .pNext = &list_ci,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_B8G8R8A8_UNORM,
      .extent = {W, H, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
      .usage = usage,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage img;
   r = DLOAD(vkCreateImage)(dev, &img_ci, NULL, &img);
   CHECK(r == VK_SUCCESS, "LIST=[LINEAR] create at %ux%u (r=%d)", W, H, r);
   if (r != VK_SUCCESS)
      DIE("cannot continue without the image");

   VkImageDrmFormatModifierPropertiesEXT img_mod = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT,
   };
   r = DLOAD(vkGetImageDrmFormatModifierPropertiesEXT)(dev, img, &img_mod);
   CHECK(r == VK_SUCCESS && img_mod.drmFormatModifier == DRM_FORMAT_MOD_LINEAR,
         "GetImageDrmFormatModifierPropertiesEXT answers LINEAR (r=%d mod=0x%" PRIx64
         ")",
         r, img_mod.drmFormatModifier);

   const VkImageSubresource subres = {
      .aspectMask = VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT,
   };
   VkSubresourceLayout sl;
   DLOAD(vkGetImageSubresourceLayout)(dev, img, &subres, &sl);
   CHECK(sl.rowPitch >= (uint64_t)W * 4 && sl.rowPitch % 4 == 0,
         "MEMORY_PLANE_0 layout: offset=%" PRIu64 " rowPitch=%" PRIu64
         " size=%" PRIu64,
         sl.offset, sl.rowPitch, sl.size);
   const int pitch_padded = sl.rowPitch > (uint64_t)W * 4;
   printf("info: rowPitch %" PRIu64 " vs tight %u — %s (the guest-side "
          "fabrication would have claimed %u)\n",
          sl.rowPitch, W * 4, pitch_padded ? "PADDED, lie now impossible" : "tight",
          W * 4);

   /* ---- bind host-visible memory ---- */
   VkMemoryRequirements mreq;
   DLOAD(vkGetImageMemoryRequirements)(dev, img, &mreq);
   VkPhysicalDeviceMemoryProperties memp;
   ILOAD(inst, vkGetPhysicalDeviceMemoryProperties)(pd, &memp);
   uint32_t mti = UINT32_MAX;
   for (uint32_t i = 0; i < memp.memoryTypeCount; i++) {
      if (!(mreq.memoryTypeBits & (1u << i)))
         continue;
      const VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                         VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
      if ((memp.memoryTypes[i].propertyFlags & want) == want) {
         mti = i;
         break;
      }
   }
   if (mti == UINT32_MAX)
      DIE("no host-visible+coherent memory type (bits 0x%x)", mreq.memoryTypeBits);
   const VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mreq.size,
      .memoryTypeIndex = mti,
   };
   VkDeviceMemory mem;
   r = DLOAD(vkAllocateMemory)(dev, &mai, NULL, &mem);
   if (r != VK_SUCCESS)
      DIE("vkAllocateMemory: %d", r);
   r = DLOAD(vkBindImageMemory)(dev, img, mem, 0);
   CHECK(r == VK_SUCCESS, "bound %" PRIu64 " B of host-visible memory (r=%d)",
         mreq.size, r);

   /* Prefill the mapping so a no-op render is distinguishable from the clear. */
   void *map;
   r = DLOAD(vkMapMemory)(dev, mem, 0, VK_WHOLE_SIZE, 0, &map);
   if (r != VK_SUCCESS)
      DIE("vkMapMemory: %d", r);
   memset(map, 0x77, mreq.size);

   /* 5: render-encoder clear on the linear modifier image (the June class) */
   const VkCommandPoolCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
   };
   VkCommandPool cp;
   DLOAD(vkCreateCommandPool)(dev, &cpci, NULL, &cp);
   const VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cp,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cb;
   DLOAD(vkAllocateCommandBuffers)(dev, &cbai, &cb);
   const VkCommandBufferBeginInfo cbbi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   DLOAD(vkBeginCommandBuffer)(cb, &cbbi);

   const VkImageMemoryBarrier to_color = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = 0,
      .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = img,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
   };
   DLOAD(vkCmdPipelineBarrier)(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                               VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0,
                               0, NULL, 0, NULL, 1, &to_color);

   const VkRenderingAttachmentInfo att = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = VK_NULL_HANDLE, /* filled below */
      .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .clearValue = {.color = {.float32 = {1.0f, 0.0f, 1.0f, 1.0f}}},
   };
   const VkImageViewCreateInfo ivci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = VK_FORMAT_B8G8R8A8_UNORM,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
   };
   VkImageView iv;
   r = DLOAD(vkCreateImageView)(dev, &ivci, NULL, &iv);
   CHECK(r == VK_SUCCESS, "attachment view on the modifier image (r=%d)", r);
   VkRenderingAttachmentInfo att_live = att;
   att_live.imageView = iv;
   const VkRenderingInfo ri = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
      .renderArea = {{0, 0}, {W, H}},
      .layerCount = 1,
      .colorAttachmentCount = 1,
      .pColorAttachments = &att_live,
   };
   DLOAD(vkCmdBeginRendering)(cb, &ri);
   DLOAD(vkCmdEndRendering)(cb);

   const VkImageMemoryBarrier to_host = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .newLayout = VK_IMAGE_LAYOUT_GENERAL,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = img,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
   };
   DLOAD(vkCmdPipelineBarrier)(cb, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                               VK_PIPELINE_STAGE_HOST_BIT, 0, 0, NULL, 0, NULL,
                               1, &to_host);
   DLOAD(vkEndCommandBuffer)(cb);

   VkQueue q;
   DLOAD(vkGetDeviceQueue)(dev, 0, 0, &q);
   const VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cb,
   };
   r = DLOAD(vkQueueSubmit)(q, 1, &si, VK_NULL_HANDLE);
   CHECK(r == VK_SUCCESS, "submit (KK replays the render pass here) (r=%d)", r);
   r = DLOAD(vkQueueWaitIdle)(q);
   CHECK(r == VK_SUCCESS, "queue idle (r=%d)", r);

   /* readback at the REPORTED pitch: BGRA magenta = FF 00 FF FF */
   const uint8_t *px = (const uint8_t *)map + sl.offset;
   int corners_ok = 1;
   const uint32_t xs[2] = {0, W - 1}, ys[2] = {0, H - 1};
   for (int yi = 0; yi < 2; yi++) {
      for (int xi = 0; xi < 2; xi++) {
         const uint8_t *p = px + ys[yi] * sl.rowPitch + xs[xi] * 4;
         const int ok = p[0] == 0xff && p[1] == 0x00 && p[2] == 0xff &&
                        p[3] == 0xff;
         if (!ok) {
            printf("      corner (%u,%u): %02x %02x %02x %02x\n", xs[xi],
                   ys[yi], p[0], p[1], p[2], p[3]);
            corners_ok = 0;
         }
      }
   }
   CHECK(corners_ok,
         "GPU cleared the linear modifier image; readback at reported pitch "
         "%" PRIu64 " sees magenta in all 4 corners",
         sl.rowPitch);

   /* 6: EXPLICIT create with the reported pitch; then a bogus one */
   const VkSubresourceLayout explicit_plane = {.offset = 0,
                                               .rowPitch = sl.rowPitch};
   VkImageDrmFormatModifierExplicitCreateInfoEXT explicit_ci = {
      .sType =
         VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
      .drmFormatModifier = DRM_FORMAT_MOD_LINEAR,
      .drmFormatModifierPlaneCount = 1,
      .pPlaneLayouts = &explicit_plane,
   };
   img_ci.pNext = &explicit_ci;
   VkImage img2;
   r = DLOAD(vkCreateImage)(dev, &img_ci, NULL, &img2);
   CHECK(r == VK_SUCCESS, "EXPLICIT create with reported pitch %" PRIu64
         " (r=%d)",
         sl.rowPitch, r);
   if (r == VK_SUCCESS) {
      VkSubresourceLayout sl2;
      DLOAD(vkGetImageSubresourceLayout)(dev, img2, &subres, &sl2);
      CHECK(sl2.rowPitch == sl.rowPitch,
            "EXPLICIT image reports the adopted pitch back (%" PRIu64 ")",
            sl2.rowPitch);
      DLOAD(vkDestroyImage)(dev, img2, NULL);
   }

   /* A bogus (too-narrow, misaligned) explicit pitch — the transition-era
    * guest's fabricated tight-packed value. KK defines this app-UB case as
    * adopt-the-computed-pitch: the create succeeds and the layout query
    * reports the TRUE pitch, never echoing the bogus one. */
   const VkSubresourceLayout bogus_plane = {.offset = 0, .rowPitch = W * 4 - 4};
   explicit_ci.pPlaneLayouts = &bogus_plane;
   r = DLOAD(vkCreateImage)(dev, &img_ci, NULL, &img2);
   CHECK(r == VK_SUCCESS,
         "EXPLICIT create with bogus pitch %u tolerated (r=%d)", W * 4 - 4, r);
   if (r == VK_SUCCESS) {
      VkSubresourceLayout sl3;
      DLOAD(vkGetImageSubresourceLayout)(dev, img2, &subres, &sl3);
      CHECK(sl3.rowPitch == sl.rowPitch && sl3.rowPitch != (uint64_t)(W * 4 - 4),
            "bogus-pitch image reports the TRUE pitch %" PRIu64 ", not the lie",
            sl3.rowPitch);
      DLOAD(vkDestroyImage)(dev, img2, NULL);
   }

   printf(failures ? "\nRESULT: %d FAILURES\n" : "\nRESULT: all green\n",
          failures);
   return failures ? 1 : 0;
}
