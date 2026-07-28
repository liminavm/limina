// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* drawstorm — native Vulkan command-stream overhead probe.
 *
 * Renders N tiny triangles per frame into an offscreen 1024x1024 target,
 * one push-constant update + one draw per triangle, re-recording the command
 * buffer every frame (the realistic app pattern). Reports the per-frame time
 * split the app can see:
 *   record = the vkCmd* loop            (guest: venus encode; host: KK enqueue)
 *   submit = vkQueueSubmit              (guest: ring flush/kick; host: Metal replay+encode)
 *   fence  = vkWaitForFences            (everything downstream + GPU)
 *
 * Portable: builds on the Linux guest (venus) and on macOS against KK directly
 * (VK_ICD_FILENAMES -> kosmickrisp json), so guest-vs-host is a direct A/B of
 * the virtualization tax on the same workload.
 *
 * Usage: drawstorm [-n draws] [-f frames] [-w warmup-frames] [-P] [-s]
 *   -P  disable per-draw push constants (draws only)
 *   -s  single draw call per frame (fixed-cost floor)
 */

#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define CHECK(x)                                                                \
   do {                                                                         \
      VkResult r_ = (x);                                                        \
      if (r_ != VK_SUCCESS) {                                                   \
         fprintf(stderr, "FAIL %s = %d @ %s:%d\n", #x, r_, __FILE__, __LINE__); \
         exit(1);                                                               \
      }                                                                         \
   } while (0)

static double
now_ms(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static void *
read_file(const char *path, size_t *size)
{
   FILE *f = fopen(path, "rb");
   if (!f) {
      fprintf(stderr, "cannot open %s (run from the probe dir; make spv first)\n", path);
      exit(1);
   }
   fseek(f, 0, SEEK_END);
   *size = ftell(f);
   fseek(f, 0, SEEK_SET);
   void *buf = malloc(*size);
   if (fread(buf, 1, *size, f) != *size)
      exit(1);
   fclose(f);
   return buf;
}

int
main(int argc, char **argv)
{
   int ndraws = 1000, nframes = 300, warmup = 30, push = 1, single = 0;
   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "-n"))
         ndraws = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-f"))
         nframes = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-w"))
         warmup = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-P"))
         push = 0;
      else if (!strcmp(argv[i], "-s"))
         single = 1;
   }
   if (single)
      ndraws = 1;

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "drawstorm",
                             .apiVersion = VK_API_VERSION_1_1 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t ndev = 1;
   VkPhysicalDevice phys;
   vkEnumeratePhysicalDevices(inst, &ndev, &phys);
   if (!ndev) {
      fprintf(stderr, "no Vulkan device\n");
      exit(1);
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   fprintf(stderr, "device: %s\n", props.deviceName);

   uint32_t nqf = 0, qfi = ~0u;
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &nqf, NULL);
   VkQueueFamilyProperties qf[16];
   if (nqf > 16)
      nqf = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &nqf, qf);
   for (uint32_t i = 0; i < nqf; i++)
      if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
         qfi = i;
         break;
      }

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                   .queueFamilyIndex = qfi,
                                   .queueCount = 1,
                                   .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .queueCreateInfoCount = 1,
                              .pQueueCreateInfos = &qci };
   VkDevice dev;
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
   VkQueue queue;
   vkGetDeviceQueue(dev, qfi, 0, &queue);

   /* offscreen color target */
   const uint32_t W = 1024, H = 1024;
   VkImageCreateInfo imgci = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                               .imageType = VK_IMAGE_TYPE_2D,
                               .format = VK_FORMAT_R8G8B8A8_UNORM,
                               .extent = { W, H, 1 },
                               .mipLevels = 1,
                               .arrayLayers = 1,
                               .samples = VK_SAMPLE_COUNT_1_BIT,
                               .tiling = VK_IMAGE_TILING_OPTIMAL,
                               .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                                        VK_IMAGE_USAGE_TRANSFER_SRC_BIT };
   VkImage img;
   CHECK(vkCreateImage(dev, &imgci, NULL, &img));
   VkMemoryRequirements mreq;
   vkGetImageMemoryRequirements(dev, img, &mreq);
   VkPhysicalDeviceMemoryProperties mprops;
   vkGetPhysicalDeviceMemoryProperties(phys, &mprops);
   uint32_t mti = ~0u;
   for (uint32_t i = 0; i < mprops.memoryTypeCount; i++)
      if ((mreq.memoryTypeBits & (1u << i)) &&
          (mprops.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) {
         mti = i;
         break;
      }
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                .allocationSize = mreq.size,
                                .memoryTypeIndex = mti };
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
   CHECK(vkBindImageMemory(dev, img, mem, 0));

   VkImageViewCreateInfo ivci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = imgci.format,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 }
   };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &ivci, NULL, &view));

   VkAttachmentDescription att = { .format = imgci.format,
                                   .samples = VK_SAMPLE_COUNT_1_BIT,
                                   .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
                                   .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                   .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
                                   .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                   .finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
   VkAttachmentReference attref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
   VkSubpassDescription sub = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
                                .colorAttachmentCount = 1,
                                .pColorAttachments = &attref };
   VkRenderPassCreateInfo rpci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
                                   .attachmentCount = 1,
                                   .pAttachments = &att,
                                   .subpassCount = 1,
                                   .pSubpasses = &sub };
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));

   VkFramebufferCreateInfo fbci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                                    .renderPass = rp,
                                    .attachmentCount = 1,
                                    .pAttachments = &view,
                                    .width = W,
                                    .height = H,
                                    .layers = 1 };
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));

   size_t vsz, fsz;
   void *vspv = read_file("vert.spv", &vsz);
   void *fspv = read_file("frag.spv", &fsz);
   VkShaderModuleCreateInfo smci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                     .codeSize = vsz,
                                     .pCode = vspv };
   VkShaderModule vs, fs;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &vs));
   smci.codeSize = fsz;
   smci.pCode = fspv;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &fs));

   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0,
                               16 };
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                       .pushConstantRangeCount = 1,
                                       .pPushConstantRanges = &pcr };
   VkPipelineLayout layout;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &layout));

   VkPipelineShaderStageCreateInfo stages[2] = {
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_VERTEX_BIT,
        .module = vs,
        .pName = "main" },
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
        .module = fs,
        .pName = "main" },
   };
   VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST
   };
   VkViewport vp = { 0, 0, W, H, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { W, H } };
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .pViewports = &vp,
      .scissorCount = 1,
      .pScissors = &sc
   };
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE,
      .lineWidth = 1.0f
   };
   VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT
   };
   VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = 0xf };
   VkPipelineColorBlendStateCreateInfo cb = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1,
      .pAttachments = &cba
   };
   VkGraphicsPipelineCreateInfo gpci = { .sType =
                                            VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
                                         .stageCount = 2,
                                         .pStages = stages,
                                         .pVertexInputState = &vin,
                                         .pInputAssemblyState = &ia,
                                         .pViewportState = &vps,
                                         .pRasterizationState = &rs,
                                         .pMultisampleState = &ms,
                                         .pColorBlendState = &cb,
                                         .layout = layout,
                                         .renderPass = rp,
                                         .subpass = 0 };
   VkPipeline pipe;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

   VkCommandPoolCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .flags =
                                       VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                                    .queueFamilyIndex = qfi };
   VkCommandPool cpool;
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cpool));
   VkCommandBufferAllocateInfo cbai = { .sType =
                                           VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                        .commandPool = cpool,
                                        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                        .commandBufferCount = 1 };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cmd));

   VkFenceCreateInfo fenci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fenci, NULL, &fence));

   double t_record = 0, t_submit = 0, t_fence = 0, t_total = 0;
   int measured = 0;

   for (int frame = 0; frame < nframes + warmup; frame++) {
      double t0 = now_ms();

      VkCommandBufferBeginInfo cbbi = {
         .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
         .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT
      };
      CHECK(vkBeginCommandBuffer(cmd, &cbbi));
      VkClearValue clear = { .color = { { 0.05f, 0.05f, 0.1f, 1.0f } } };
      VkRenderPassBeginInfo rpbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                                     .renderPass = rp,
                                     .framebuffer = fb,
                                     .renderArea = { { 0, 0 }, { W, H } },
                                     .clearValueCount = 1,
                                     .pClearValues = &clear };
      vkCmdBeginRenderPass(cmd, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
      vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
      float pc[4] = { 0, 0, 0, 1 };
      for (int i = 0; i < ndraws; i++) {
         if (push) {
            pc[0] = (float)(i & 1023) / 1024.0f;
            pc[1] = (float)((i >> 4) & 1023) / 1024.0f;
            pc[2] = (float)frame / 1000.0f;
            vkCmdPushConstants(cmd, layout,
                               VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0,
                               16, pc);
         }
         vkCmdDraw(cmd, 3, 1, 0, 0);
      }
      vkCmdEndRenderPass(cmd);
      CHECK(vkEndCommandBuffer(cmd));
      double t1 = now_ms();

      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                          .commandBufferCount = 1,
                          .pCommandBuffers = &cmd };
      CHECK(vkQueueSubmit(queue, 1, &si, fence));
      double t2 = now_ms();

      CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
      CHECK(vkResetFences(dev, 1, &fence));
      double t3 = now_ms();

      if (frame >= warmup) {
         t_record += t1 - t0;
         t_submit += t2 - t1;
         t_fence += t3 - t2;
         t_total += t3 - t0;
         measured++;
      }
   }

   double per = measured ? 1.0 / measured : 0;
   printf("drawstorm n=%d push=%d frames=%d device=\"%s\"\n", ndraws, push, measured,
          props.deviceName);
   printf("per-frame ms: record=%.3f submit=%.3f fence=%.3f total=%.3f (%.1f fps)\n",
          t_record * per, t_submit * per, t_fence * per, t_total * per,
          measured / (t_total / 1e3));
   printf("per-draw us: record=%.3f submit=%.3f fence=%.3f total=%.3f\n",
          t_record * per / ndraws * 1e3, t_submit * per / ndraws * 1e3,
          t_fence * per / ndraws * 1e3, t_total * per / ndraws * 1e3);

   vkDeviceWaitIdle(dev);
   return 0;
}
