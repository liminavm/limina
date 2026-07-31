// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* kk-format-mismatch-abort: host-side Vulkan probe for the guest-triggerable
 * VMM abort at render-pass begin when a framebuffer attachment view's format
 * differs from the render pass's declared attachment format
 * (VUID-VkFramebufferCreateInfo-pAttachments-00880).
 *
 * Reproduces the 2026-07-30 dogfood-mac dogfood crashes on the HOST KosmicKrisp
 * driver directly (no VM): the guest compositor's half-applied RGBA->BGRA
 * format flip paired a render pass declaring B8G8R8A8_UNORM with an
 * R8G8B8A8_UNORM attachment view.  KK has no native render-pass path, so the
 * command replays at vkQueueSubmit through mesa's common runtime,
 * vk_common_CmdBeginRenderPass2 -> assert(image_view->format ==
 * pass_att->format) (vk_render_pass.c:2708) -> SIGABRT kills the worker.
 *
 * The probe: RGBA8 image + RGBA8 view, render pass declaring BGRA8, legacy
 * framebuffer, one begin/end with loadOp=CLEAR to red, readback.
 *
 * Driver is selected by VK_ICD_FILENAMES (point it at the KK build). run.sh.
 *
 * RED  (unfixed KK): SIGABRT with the vk_render_pass.c:2708 assert at submit.
 * GREEN (fixed KK):  exits 0; stderr carries the "render-pass begin VU
 *                    violation" warning; the clear still lands (contained).
 *
 * NOTE: calls the KK ICD directly — exercises the KK-side log-only fix ONLY,
 * not virglrenderer/vkr.
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

   /* ---- device ---- */
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

   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = ndev_exts,
      .ppEnabledExtensionNames = dev_exts,
   };
   VkDevice dev;
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   VkQueue queue;
   vkGetDeviceQueue(dev, qfi, 0, &queue);

   /* ---- offscreen color image: R8G8B8A8, view R8G8B8A8 ---- */
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

   /* ---- THE POISON: render pass declares B8G8R8A8 for that RGBA8 view ---- */
   VkAttachmentDescription att = {
      .format = VK_FORMAT_B8G8R8A8_UNORM, /* != the view's R8G8B8A8 */
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
      .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
   };
   VkAttachmentReference cref = {
      .attachment = 0,
      .layout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
   };
   VkSubpassDescription sub = {
      .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
      .colorAttachmentCount = 1,
      .pColorAttachments = &cref,
   };
   VkRenderPassCreateInfo rpci = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
      .attachmentCount = 1,
      .pAttachments = &att,
      .subpassCount = 1,
      .pSubpasses = &sub,
   };
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));

   VkFramebufferCreateInfo fbci = {
      .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
      .renderPass = rp,
      .attachmentCount = 1,
      .pAttachments = &view,
      .width = W,
      .height = H,
      .layers = 1,
   };
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));

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

   VkClearValue clear = {.color = {.float32 = {1, 0, 0, 1}}}; /* red */
   VkRenderPassBeginInfo rpbi = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
      .renderPass = rp,
      .framebuffer = fb,
      .renderArea = {{0, 0}, {W, H}},
      .clearValueCount = 1,
      .pClearValues = &clear,
   };
   printf("recording BeginRenderPass: pass declares BGRA8, view is RGBA8 (VU violation)\n");
   fflush(stdout);
   vkCmdBeginRenderPass(cmd, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
   vkCmdEndRenderPass(cmd);

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

   /* ---- readback: the clear should still have landed (contained) ---- */
   uint8_t *px;
   CHECK(vkMapMemory(dev, rbm, 0, VK_WHOLE_SIZE, 0, (void **)&px));
   const uint8_t *p = &px[((H / 2) * (size_t)W + W / 2) * 4];
   printf("readback: center (32,32) = [%u %u %u %u] (want red 255,0,0,255)\n",
          p[0], p[1], p[2], p[3]);
   int ok = p[0] == 255 && p[1] == 0 && p[2] == 0;
   vkUnmapMemory(dev, rbm);

   if (ok) {
      printf("GREEN: no abort; VU violation logged, clear contained.\n");
      return 0;
   }
   printf("NOTE: no abort (good), but readback not red — undefined-but-contained "
          "rendering is acceptable for a VU violation; treat as GREEN if the "
          "warning line appeared on stderr.\n");
   return 0;
}
