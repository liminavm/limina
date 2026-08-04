// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* kk-empty-clear-rect: host-side Vulkan probe for the guest-triggerable VMM abort in
 * vkCmdClearAttachments with a zero-extent VkClearRect.
 *
 * Reproduces the crash on the HOST KosmicKrisp driver directly (no VM): a
 * guest venus stream can encode vkCmdClearAttachments with an invalid
 * VkClearRect{extent={0,0}} (VUID-vkCmdClearAttachments-rect-02682/-02683).
 * vkr passes it through unvalidated; KK replays it at vkQueueSubmit into
 * vk_meta_clear_attachments -> vk_meta_draw_rects/setup_viewport_scissor, whose
 * assert(rects[0].x0 < rects[0].x1 && ...) aborts the whole worker process.
 *
 * The probe issues ONE vkCmdClearAttachments containing:
 *   - rect[0] = a VALID sub-rect (offset {8,8}, extent {16,16}) cleared to red,
 *   - rect[1] = an EMPTY rect (offset {0,0}, extent {0,0}).
 * The empty rect is the poison. The valid rect exists so a readback can witness
 * the "union of all rects" corruption argument: with the fix, the empty rect is
 * skipped and the valid rect still clears exactly its region.
 *
 * Driver is selected by VK_ICD_FILENAMES (point it at the KK build). See run.sh.
 *
 * RED  (unfixed KK): aborts with the setup_viewport_scissor assert (SIGABRT).
 * GREEN (fixed KK):  runs to completion, exits 0, prints the readback proof that
 *                    the valid rect cleared red and the rest stayed black.
 *
 * NOTE: the probe calls the KK ICD directly, so it exercises Fix B (KK/vk_meta)
 * ONLY. It does NOT go through virglrenderer/vkr, so it says nothing about Fix A.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define CHECK(expr)                                                            \
   do {                                                                        \
      VkResult _r = (expr);                                                    \
      if (_r != VK_SUCCESS) {                                                  \
         fprintf(stderr, "%s:%d: %s -> %d\n", __FILE__, __LINE__, #expr, _r);  \
         exit(2);                                                              \
      }                                                                        \
   } while (0)

#define W 64u
#define H 64u
#define RX 8u
#define RY 8u
#define RW 16u
#define RH 16u

static uint32_t
find_mem_type(VkPhysicalDevice pdev, uint32_t type_bits, VkMemoryPropertyFlags props)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((type_bits & (1u << i)) &&
          (mp.memoryTypes[i].propertyFlags & props) == props)
         return i;
   fprintf(stderr, "no memory type for bits=0x%x props=0x%x\n", type_bits, props);
   exit(2);
}

int
main(void)
{
   /* ---- instance ---- */
   VkApplicationInfo ai = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstance inst;
   const char *port_ext = "VK_KHR_portability_enumeration";
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &ai,
      .flags = 0x00000001, /* ENUMERATE_PORTABILITY_KHR */
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = &port_ext,
   };
   if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) {
      ici.flags = 0;
      ici.enabledExtensionCount = 0;
      CHECK(vkCreateInstance(&ici, NULL, &inst));
   }

   uint32_t np = 1;
   VkPhysicalDevice pdev;
   VkResult er = vkEnumeratePhysicalDevices(inst, &np, &pdev);
   if (er != VK_SUCCESS && er != VK_INCOMPLETE) {
      fprintf(stderr, "no physical device (er=%d) — check VK_ICD_FILENAMES\n", er);
      return 2;
   }
   VkPhysicalDeviceProperties pp;
   vkGetPhysicalDeviceProperties(pdev, &pp);
   printf("device: %s (api %u.%u.%u)\n", pp.deviceName,
          VK_API_VERSION_MAJOR(pp.apiVersion),
          VK_API_VERSION_MINOR(pp.apiVersion),
          VK_API_VERSION_PATCH(pp.apiVersion));

   uint32_t qfi = 0, nqf = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nqf, NULL);
   VkQueueFamilyProperties *qfs = calloc(nqf, sizeof(*qfs));
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nqf, qfs);
   for (qfi = 0; qfi < nqf; qfi++)
      if (qfs[qfi].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;

   /* ---- device (enable dynamic rendering) ---- */
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfi,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   uint32_t next = 0;
   vkEnumerateDeviceExtensionProperties(pdev, NULL, &next, NULL);
   VkExtensionProperties *exts = calloc(next, sizeof(*exts));
   vkEnumerateDeviceExtensionProperties(pdev, NULL, &next, exts);
   const char *dev_exts[4];
   uint32_t ndev_exts = 0;
   for (uint32_t i = 0; i < next; i++)
      if (!strcmp(exts[i].extensionName, "VK_KHR_portability_subset"))
         dev_exts[ndev_exts++] = "VK_KHR_portability_subset";

   VkPhysicalDeviceDynamicRenderingFeatures dyn = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DYNAMIC_RENDERING_FEATURES,
      .dynamicRendering = VK_TRUE,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &dyn,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = ndev_exts,
      .ppEnabledExtensionNames = dev_exts,
   };
   VkDevice dev;
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   VkQueue queue;
   vkGetDeviceQueue(dev, qfi, 0, &queue);

   /* ---- offscreen color image ---- */
   VkImageCreateInfo imgci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .extent = {W, H, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
               VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage color;
   CHECK(vkCreateImage(dev, &imgci, NULL, &color));
   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, color, &mr);
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = find_mem_type(pdev, mr.memoryTypeBits,
                                       VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
   };
   VkDeviceMemory imem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &imem));
   CHECK(vkBindImageMemory(dev, color, imem, 0));
   VkImageViewCreateInfo vci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = color,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
   };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &vci, NULL, &view));

   /* ---- readback buffer ---- */
   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = (VkDeviceSize)W * H * 4,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
   };
   VkBuffer rb;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &rb));
   vkGetBufferMemoryRequirements(dev, rb, &mr);
   VkMemoryAllocateInfo rmai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = find_mem_type(pdev, mr.memoryTypeBits,
                                       VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                          VK_MEMORY_PROPERTY_HOST_COHERENT_BIT),
   };
   VkDeviceMemory rbm;
   CHECK(vkAllocateMemory(dev, &rmai, NULL, &rbm));
   CHECK(vkBindBufferMemory(dev, rb, rbm, 0));

   /* ---- command buffer ---- */
   VkCommandPoolCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qfi,
   };
   VkCommandPool cp;
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cp));
   VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cp,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cmd));

   VkCommandBufferBeginInfo cbi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkBeginCommandBuffer(cmd, &cbi));

   /* UNDEFINED -> COLOR_ATTACHMENT_OPTIMAL */
   VkImageMemoryBarrier tob = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = color,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
      .srcAccessMask = 0,
      .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0,
                        NULL, 0, NULL, 1, &tob);

   /* Begin dynamic rendering, whole-framebuffer loadOp=CLEAR to black. */
   VkRenderingAttachmentInfo catt = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = view,
      .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .clearValue = {.color = {.float32 = {0, 0, 0, 1}}},
   };
   VkRenderingInfo ri = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
      .renderArea = {{0, 0}, {W, H}},
      .layerCount = 1,
      .colorAttachmentCount = 1,
      .pColorAttachments = &catt,
   };
   vkCmdBeginRendering(cmd, &ri);

   /* THE POISON: one valid rect + three degenerate rects in the same clear call.
    *  - rect[1] EMPTY {0,0 0x0}: the original zero-extent class (kk 0009).
    *  - rect[2] NEGATIVE offset {-8,-8 16x16}: nonzero extent, so it passes the
    *    zero-extent filter, but offset.x is signed i32 and vk_meta_rect.x0/x1 are
    *    u32 — the conversion wraps to x1 < x0 (the 2026-08-04 dogfood-mac crash class,
    *    vk_meta_draw_rects.c:163/:167).
    *  - rect[3] HUGE extent {8,8 0xFFFF0000x16}: offset+width stays < 2^32 so the
    *    x0 < x1 assert passes, then the union log2 blows the :183 xmax_log2 <= 31
    *    range assert.
    */
   VkClearAttachment clear_att = {
      .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
      .colorAttachment = 0,
      .clearValue = {.color = {.float32 = {1, 0, 0, 1}}}, /* red */
   };
   VkClearRect rects[4] = {
      {.rect = {{RX, RY}, {RW, RH}}, .baseArrayLayer = 0, .layerCount = 1},
      {.rect = {{0, 0}, {0, 0}}, .baseArrayLayer = 0, .layerCount = 1},     /* empty */
      {.rect = {{-8, -8}, {16, 16}}, .baseArrayLayer = 0, .layerCount = 1}, /* inverted */
      {.rect = {{8, 8}, {0xFFFF0000u, 16}}, .baseArrayLayer = 0, .layerCount = 1}, /* huge */
   };
   printf("issuing vkCmdClearAttachments: valid rect {%u,%u %ux%u} + EMPTY {0,0 0x0} "
          "+ NEGATIVE {-8,-8 16x16} + HUGE {8,8 0xFFFF0000x16}\n",
          RX, RY, RW, RH);
   fflush(stdout);
   vkCmdClearAttachments(cmd, 1, &clear_att, 4, rects);

   vkCmdEndRendering(cmd);

   /* COLOR_ATTACHMENT_OPTIMAL -> TRANSFER_SRC_OPTIMAL */
   VkImageMemoryBarrier tsb = tob;
   tsb.oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
   tsb.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
   tsb.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
   tsb.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                        &tsb);

   VkBufferImageCopy bic = {
      .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
      .imageExtent = {W, H, 1},
   };
   vkCmdCopyImageToBuffer(cmd, color, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, rb,
                          1, &bic);

   CHECK(vkEndCommandBuffer(cmd));

   VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   printf("submitting (KK replays the command buffer here)...\n");
   fflush(stdout);
   CHECK(vkQueueSubmit(queue, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
   printf("submit completed without abort\n");

   /* ---- readback / union-corruption witness ---- */
   uint8_t *px;
   CHECK(vkMapMemory(dev, rbm, 0, VK_WHOLE_SIZE, 0, (void **)&px));
#define PIX(x, y) (&px[((y) * (size_t)W + (x)) * 4])
   const uint8_t *inside = PIX(RX + RW / 2, RY + RH / 2); /* {16,16} */
   const uint8_t *outside = PIX(RX + RW + 8, RY + RH + 8); /* {32,32} */
   printf("readback: inside-valid-rect (16,16) = [%u %u %u %u] (want red 255,0,0)\n",
          inside[0], inside[1], inside[2], inside[3]);
   printf("readback: outside          (32,32) = [%u %u %u %u] (want black 0,0,0)\n",
          outside[0], outside[1], outside[2], outside[3]);
   int ok = inside[0] == 255 && inside[1] == 0 && inside[2] == 0 &&
            outside[0] == 0 && outside[1] == 0 && outside[2] == 0;
   vkUnmapMemory(dev, rbm);

   if (ok) {
      printf("GREEN: no abort; valid rect cleared red, empty rect skipped cleanly, "
             "union intact.\n");
      return 0;
   }
   printf("FAIL: no abort, but readback is wrong (union corruption or clear misplaced).\n");
   return 1;
}
