/* Begin a render pass with fewer attachment image views than the pass
 * declares, then end it and the command buffer.
 *
 *   rp_attach_count imageless   imageless framebuffer, VkRenderPassAttachmentBeginInfo
 *                               carrying 1 view for a 2-attachment pass
 *   rp_attach_count fb          regular framebuffer created with 1 view for a
 *                               2-attachment pass
 *   rp_attach_count nobegin     imageless framebuffer begun WITHOUT
 *                               VkRenderPassAttachmentBeginInfo (0 views)
 *   rp_attach_count ok          valid control: 2 views, 2 attachments
 *
 * Run against a chosen ICD with VK_ICD_FILENAMES=<icd.json>.  Exit 0 and a
 * "RESULT" line means the process survived; the EndCommandBuffer result is
 * printed.  Build: cc -o rp_attach_count rp_attach_count.c
 *                  -I/opt/homebrew/include -L/opt/homebrew/lib -lvulkan
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define CHECK(x)                                                              \
   do {                                                                       \
      VkResult _r = (x);                                                      \
      if (_r != VK_SUCCESS) {                                                 \
         fprintf(stderr, "%s failed: %d\n", #x, _r);                          \
         exit(2);                                                             \
      }                                                                       \
   } while (0)

/* Framebuffer allocator that hands out 0xAA-filled memory, so a field the
 * driver forgets to initialize reads as garbage rather than as the zero a
 * fresh heap page usually happens to hold. */
static VKAPI_ATTR void *VKAPI_CALL
scribble_alloc(void *ud, size_t size, size_t align, VkSystemAllocationScope s)
{
   void *p = NULL;
   if (posix_memalign(&p, align < sizeof(void *) ? sizeof(void *) : align, size))
      return NULL;
   memset(p, 0xAA, size);
   return p;
}
static VKAPI_ATTR void *VKAPI_CALL
scribble_realloc(void *ud, void *o, size_t size, size_t align,
                 VkSystemAllocationScope s)
{
   return realloc(o, size);
}
static VKAPI_ATTR void VKAPI_CALL
scribble_free(void *ud, void *p)
{
   free(p);
}
static const VkAllocationCallbacks scribble = {
   .pfnAllocation = scribble_alloc,
   .pfnReallocation = scribble_realloc,
   .pfnFree = scribble_free,
};

static uint32_t
find_mem(VkPhysicalDevice pd, uint32_t bits)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if (bits & (1u << i))
         return i;
   return 0;
}

int
main(int argc, char **argv)
{
   const char *mode = argc > 1 ? argv[1] : "imageless";
   int nobegin = !strcmp(mode, "nobegin");
   int imageless = !strcmp(mode, "imageless") || nobegin;
   int valid = !strcmp(mode, "ok");

   const char *inst_ext[] = {VK_KHR_PORTABILITY_ENUMERATION_EXTENSION_NAME};
   VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                            .apiVersion = VK_API_VERSION_1_2};
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
      .pApplicationInfo = &app,
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = inst_ext,
   };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t n = 1;
   VkPhysicalDevice pd;
   VkResult er = vkEnumeratePhysicalDevices(inst, &n, &pd);
   if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || n == 0) {
      fprintf(stderr, "no physical device\n");
      return 2;
   }
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
   VkPhysicalDeviceVulkan12Features f12 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
      .imagelessFramebuffer = VK_TRUE,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &f12,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));

   const VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;
   const VkImageUsageFlags usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;

   VkImageCreateInfo imci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = fmt,
      .extent = {64, 64, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = usage,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage img[2];
   VkImageView view[2];
   for (int i = 0; i < 2; i++) {
      CHECK(vkCreateImage(dev, &imci, NULL, &img[i]));
      VkMemoryRequirements mr;
      vkGetImageMemoryRequirements(dev, img[i], &mr);
      VkMemoryAllocateInfo mai = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
         .allocationSize = mr.size,
         .memoryTypeIndex = find_mem(pd, mr.memoryTypeBits),
      };
      VkDeviceMemory mem;
      CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
      CHECK(vkBindImageMemory(dev, img[i], mem, 0));
      VkImageViewCreateInfo vci = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
         .image = img[i],
         .viewType = VK_IMAGE_VIEW_TYPE_2D,
         .format = fmt,
         .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
      };
      CHECK(vkCreateImageView(dev, &vci, NULL, &view[i]));
   }

   VkAttachmentDescription att[2];
   for (int i = 0; i < 2; i++) {
      att[i] = (VkAttachmentDescription){
         .format = fmt,
         .samples = VK_SAMPLE_COUNT_1_BIT,
         .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
         .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
         .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
         .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
         .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
         .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
      };
   }
   VkAttachmentReference refs[2] = {
      {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL},
      {1, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL},
   };
   VkSubpassDescription sp = {
      .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
      .colorAttachmentCount = 2,
      .pColorAttachments = refs,
   };
   VkRenderPassCreateInfo rpci = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
      .attachmentCount = 2,
      .pAttachments = att,
      .subpassCount = 1,
      .pSubpasses = &sp,
   };
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));

   /* The framebuffer carries 1 attachment (invalid) unless mode "ok". */
   uint32_t supplied = valid ? 2 : nobegin ? 0 : 1;
   VkFramebufferAttachmentImageInfo aii[2];
   for (int i = 0; i < 2; i++) {
      aii[i] = (VkFramebufferAttachmentImageInfo){
         .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_ATTACHMENT_IMAGE_INFO,
         .usage = usage,
         .width = 64,
         .height = 64,
         .layerCount = 1,
         .viewFormatCount = 1,
         .pViewFormats = &fmt,
      };
   }
   VkFramebufferAttachmentsCreateInfo faci = {
      .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_ATTACHMENTS_CREATE_INFO,
      .attachmentImageInfoCount = 2,
      .pAttachmentImageInfos = aii,
   };
   VkFramebufferCreateInfo fbci = {
      .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
      .pNext = imageless ? &faci : NULL,
      .flags = imageless ? VK_FRAMEBUFFER_CREATE_IMAGELESS_BIT : 0,
      .renderPass = rp,
      .attachmentCount = imageless ? 2 : supplied,
      .pAttachments = imageless ? NULL : view,
      .width = 64,
      .height = 64,
      .layers = 1,
   };
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fbci, &scribble, &fb));

   VkCommandPoolCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = 0,
   };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
   VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));
   VkCommandBufferBeginInfo cbbi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
   };
   CHECK(vkBeginCommandBuffer(cb, &cbbi));

   VkRenderPassAttachmentBeginInfo rabi = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_ATTACHMENT_BEGIN_INFO,
      .attachmentCount = supplied,
      .pAttachments = view,
   };
   VkClearValue clears[2] = {0};
   VkRenderPassBeginInfo rpbi = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
      .pNext = imageless && !nobegin ? &rabi : NULL,
      .renderPass = rp,
      .framebuffer = fb,
      .renderArea = {{0, 0}, {64, 64}},
      .clearValueCount = 2,
      .pClearValues = clears,
   };
   printf("mode=%s: beginning a 2-attachment render pass with %u view(s)\n",
          mode, supplied);
   fflush(stdout);
   vkCmdBeginRenderPass(cb, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
   printf("begin returned\n");
   fflush(stdout);
   vkCmdEndRenderPass(cb);
   printf("end render pass returned\n");
   fflush(stdout);
   VkResult r = vkEndCommandBuffer(cb);
   printf("RESULT: vkEndCommandBuffer = %d\n", r);

   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyFramebuffer(dev, fb, &scribble);
   vkDestroyRenderPass(dev, rp, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return 0;
}
