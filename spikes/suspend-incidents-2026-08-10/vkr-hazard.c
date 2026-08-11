/*
 * vkr-hazard: deterministic reproducer for the suspend-replay SIGSEGV
 * (issue 2 of the 2026-08-10 dogfood incidents).
 *
 * Records a command buffer whose vkCmdBeginRenderPass references a
 * framebuffer, then destroys the framebuffer and sleeps forever. The venus
 * journal prunes the framebuffer's CREATE entry at destroy, but the
 * RECORDING entries (keyed by the still-live command buffer) survive and do
 * not pin what they reference. Every suspend/resume replay then dispatches
 * CmdBeginRenderPass against a dead framebuffer handle:
 *
 *   - unpatched vkr: the failed Begin is FATAL-recovered and skipped, the
 *     following CmdEndRenderPass still reaches the host driver, and
 *     KosmicKrisp derefs cmd_buffer->render_pass == NULL in end_subpass
 *     (vk_render_pass.c) -> worker SIGSEGV at null+0x60. RED.
 *   - patched vkr: the failure poisons the cmd_buf for the rest of the
 *     replay ("replay: poisoning cmd_buf" in the worker log) and the resume
 *     completes cleanly. GREEN.
 *
 * Build (in the guest):  gcc -O1 -o vkr-hazard vkr-hazard.c -lvulkan
 * Run (venus ICD env):   set -a; . /etc/environment.d/90-limina-zink.conf; set +a
 *                        ./vkr-hazard &
 * then suspend/resume the VM once per verdict.
 */
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHECK(expr)                                                         \
   do {                                                                     \
      VkResult check_res_ = (expr);                                         \
      if (check_res_ != VK_SUCCESS) {                                       \
         fprintf(stderr, "FAIL %s = %d (%s:%d)\n", #expr, check_res_,       \
                 __FILE__, __LINE__);                                       \
         exit(1);                                                           \
      }                                                                     \
   } while (0)

int
main(void)
{
   VkInstance inst;
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "vkr-hazard",
      .apiVersion = VK_API_VERSION_1_1,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t ndev = 1;
   VkPhysicalDevice phys;
   VkResult r = vkEnumeratePhysicalDevices(inst, &ndev, &phys);
   if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || ndev == 0) {
      fprintf(stderr, "no physical device (venus ICD env missing?)\n");
      return 1;
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   printf("device: %s\n", props.deviceName);

   uint32_t nqf = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &nqf, NULL);
   VkQueueFamilyProperties *qf = calloc(nqf, sizeof(*qf));
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &nqf, qf);
   uint32_t qfi = 0;
   for (uint32_t i = 0; i < nqf; i++) {
      if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
         qfi = i;
         break;
      }
   }
   free(qf);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfi,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   VkDevice dev;
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));

   /* A 64x64 color attachment to hang the framebuffer on. */
   VkImageCreateInfo imgci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .extent = { 64, 64, 1 },
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage img;
   CHECK(vkCreateImage(dev, &imgci, NULL, &img));

   VkMemoryRequirements mreq;
   vkGetImageMemoryRequirements(dev, img, &mreq);
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(phys, &mp);
   uint32_t mti = UINT32_MAX;
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
      if ((mreq.memoryTypeBits & (1u << i)) &&
          (mp.memoryTypes[i].propertyFlags &
           VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) {
         mti = i;
         break;
      }
   }
   if (mti == UINT32_MAX)
      mti = __builtin_ctz(mreq.memoryTypeBits);
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mreq.size,
      .memoryTypeIndex = mti,
   };
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
   CHECK(vkBindImageMemory(dev, img, mem, 0));

   VkImageViewCreateInfo ivci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = imgci.format,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
   };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &ivci, NULL, &view));

   VkAttachmentDescription att = {
      .format = imgci.format,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
      .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
   };
   VkAttachmentReference ref = {
      .attachment = 0,
      .layout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
   };
   VkSubpassDescription sub = {
      .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
      .colorAttachmentCount = 1,
      .pColorAttachments = &ref,
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
      .width = 64,
      .height = 64,
      .layers = 1,
   };
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));

   VkCommandPoolCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qfi,
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
   VkClearValue clear = { .color = { .float32 = { 0, 0, 0, 1 } } };
   VkRenderPassBeginInfo rpbi = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
      .renderPass = rp,
      .framebuffer = fb,
      .renderArea = { { 0, 0 }, { 64, 64 } },
      .clearValueCount = 1,
      .pClearValues = &clear,
   };
   vkCmdBeginRenderPass(cb, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
   vkCmdEndRenderPass(cb);
   CHECK(vkEndCommandBuffer(cb));

   /* The hazard: kill the framebuffer, keep the recorded cmd_buf alive. */
   vkDestroyFramebuffer(dev, fb, NULL);

   printf("armed: recorded cmd_buf alive, framebuffer destroyed; "
          "suspend/resume now\n");
   fflush(stdout);
   for (;;)
      pause();
}
