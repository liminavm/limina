// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* crossmark Vulkan backend — see crossmark.h for the scene contract.
 * Derived from spikes/venus-cmdstream-probe/drawstorm.c. */

#include <vulkan/vulkan.h>
#include "crossmark.h"

#define CHECK(x)                                                                \
   do {                                                                         \
      VkResult r_ = (x);                                                        \
      if (r_ != VK_SUCCESS) {                                                   \
         fprintf(stderr, "FAIL %s = %d @ %s:%d\n", #x, r_, __FILE__, __LINE__); \
         exit(1);                                                               \
      }                                                                         \
   } while (0)

static void *
read_file(const char *path, size_t *size)
{
   FILE *f = fopen(path, "rb");
   if (!f) {
      fprintf(stderr, "cannot open %s (run from the crossmark dir; make first)\n", path);
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

static VkDevice dev;
static VkPhysicalDevice phys;

static uint32_t
mem_type(uint32_t bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(phys, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type for 0x%x\n", want);
   exit(1);
}

static void
make_buffer(VkDeviceSize size, VkBufferUsageFlags usage, VkMemoryPropertyFlags props,
            VkBuffer *buf, VkDeviceMemory *mem)
{
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                              .size = size,
                              .usage = usage };
   CHECK(vkCreateBuffer(dev, &bci, NULL, buf));
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, *buf, &req);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                .allocationSize = req.size,
                                .memoryTypeIndex = mem_type(req.memoryTypeBits, props) };
   CHECK(vkAllocateMemory(dev, &mai, NULL, mem));
   CHECK(vkBindBufferMemory(dev, *buf, *mem, 0));
}

static void
make_image(uint32_t w, uint32_t h, VkImageUsageFlags usage, VkImage *img,
           VkDeviceMemory *mem)
{
   VkImageCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                             .imageType = VK_IMAGE_TYPE_2D,
                             .format = VK_FORMAT_R8G8B8A8_UNORM,
                             .extent = { w, h, 1 },
                             .mipLevels = 1,
                             .arrayLayers = 1,
                             .samples = VK_SAMPLE_COUNT_1_BIT,
                             .tiling = VK_IMAGE_TILING_OPTIMAL,
                             .usage = usage };
   CHECK(vkCreateImage(dev, &ici, NULL, img));
   VkMemoryRequirements req;
   vkGetImageMemoryRequirements(dev, *img, &req);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                .allocationSize = req.size,
                                .memoryTypeIndex = mem_type(
                                   req.memoryTypeBits,
                                   VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   CHECK(vkAllocateMemory(dev, &mai, NULL, mem));
   CHECK(vkBindImageMemory(dev, *img, *mem, 0));
}

static void
image_barrier(VkCommandBuffer cmd, VkImage img, VkImageLayout from, VkImageLayout to,
              VkAccessFlags src, VkAccessFlags dst, VkPipelineStageFlags srcStage,
              VkPipelineStageFlags dstStage)
{
   VkImageMemoryBarrier b = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                              .srcAccessMask = src,
                              .dstAccessMask = dst,
                              .oldLayout = from,
                              .newLayout = to,
                              .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                              .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                              .image = img,
                              .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0,
                                                    1 } };
   vkCmdPipelineBarrier(cmd, srcStage, dstStage, 0, 0, NULL, 0, NULL, 1, &b);
}

int
main(int argc, char **argv)
{
   struct cm_opts o;
   if (cm_parse_args(&o, argc, argv))
      return 1;

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "crossmark",
                             .apiVersion = VK_API_VERSION_1_1 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t ndev = 1;
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
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
   VkQueue queue;
   vkGetDeviceQueue(dev, qfi, 0, &queue);

   /* offscreen color target */
   VkImage target;
   VkDeviceMemory target_mem;
   make_image(CM_WIDTH, CM_HEIGHT,
              VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
              &target, &target_mem);
   VkImageViewCreateInfo ivci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = target,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 }
   };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &ivci, NULL, &view));

   VkAttachmentDescription att = { .format = VK_FORMAT_R8G8B8A8_UNORM,
                                   .samples = VK_SAMPLE_COUNT_1_BIT,
                                   .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
                                   .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                   .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
                                   .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                   .finalLayout =
                                      VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL };
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
                                    .width = CM_WIDTH,
                                    .height = CM_HEIGHT,
                                    .layers = 1 };
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb));

   size_t vsz, ffsz, tfsz;
   void *vspv = read_file("cm.vert.spv", &vsz);
   void *ffspv = read_file("cm_flat.frag.spv", &ffsz);
   void *tfspv = read_file("cm_tex.frag.spv", &tfsz);
   VkShaderModuleCreateInfo smci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                     .codeSize = vsz,
                                     .pCode = vspv };
   VkShaderModule vs, flat_fs, tex_fs;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &vs));
   smci.codeSize = ffsz;
   smci.pCode = ffspv;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &flat_fs));
   smci.codeSize = tfsz;
   smci.pCode = tfspv;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &tex_fs));

   /* descriptor set layout: one combined image sampler (tex pipeline) */
   VkDescriptorSetLayoutBinding dslb = { .binding = 0,
                                         .descriptorType =
                                            VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
                                         .descriptorCount = 1,
                                         .stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT };
   VkDescriptorSetLayoutCreateInfo dslci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 1,
      .pBindings = &dslb
   };
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));

   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT,
                               0, 32 };
   VkPipelineLayoutCreateInfo plci = { .sType =
                                          VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                       .pushConstantRangeCount = 1,
                                       .pPushConstantRanges = &pcr };
   VkPipelineLayout flat_layout;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &flat_layout));
   plci.setLayoutCount = 1;
   plci.pSetLayouts = &dsl;
   VkPipelineLayout tex_layout;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &tex_layout));

   /* pipelines: CM_NVARIANTS flat variants (spec-constant factor) + tex */
   VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST
   };
   VkViewport vp = { 0, 0, CM_WIDTH, CM_HEIGHT, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { CM_WIDTH, CM_HEIGHT } };
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

   VkPipeline flat_pipes[CM_NVARIANTS], tex_pipe;
   for (int v = 0; v < CM_NVARIANTS; v++) {
      float factor = (float)(v + 1) / (float)CM_NVARIANTS;
      VkSpecializationMapEntry sme = { 0, 0, 4 };
      VkSpecializationInfo si = { 1, &sme, 4, &factor };
      VkPipelineShaderStageCreateInfo stages[2] = {
         { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
           .stage = VK_SHADER_STAGE_VERTEX_BIT,
           .module = vs,
           .pName = "main" },
         { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
           .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
           .module = flat_fs,
           .pName = "main",
           .pSpecializationInfo = &si },
      };
      VkGraphicsPipelineCreateInfo gpci = {
         .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
         .stageCount = 2,
         .pStages = stages,
         .pVertexInputState = &vin,
         .pInputAssemblyState = &ia,
         .pViewportState = &vps,
         .pRasterizationState = &rs,
         .pMultisampleState = &ms,
         .pColorBlendState = &cb,
         .layout = flat_layout,
         .renderPass = rp,
         .subpass = 0
      };
      CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL,
                                      &flat_pipes[v]));
   }
   {
      VkPipelineShaderStageCreateInfo stages[2] = {
         { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
           .stage = VK_SHADER_STAGE_VERTEX_BIT,
           .module = vs,
           .pName = "main" },
         { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
           .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
           .module = tex_fs,
           .pName = "main" },
      };
      VkGraphicsPipelineCreateInfo gpci = {
         .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
         .stageCount = 2,
         .pStages = stages,
         .pVertexInputState = &vin,
         .pInputAssemblyState = &ia,
         .pViewportState = &vps,
         .pRasterizationState = &rs,
         .pMultisampleState = &ms,
         .pColorBlendState = &cb,
         .layout = tex_layout,
         .renderPass = rp,
         .subpass = 0
      };
      CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &tex_pipe));
   }

   /* textures: 2 static + 1 streaming, NEAREST/CLAMP for determinism */
   VkImage tex_img[3];
   VkDeviceMemory tex_mem[3];
   VkImageView tex_view[3];
   for (int i = 0; i < 3; i++) {
      uint32_t d = CM_UPLOAD_TEX;
      make_image(d, d, VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
                 &tex_img[i], &tex_mem[i]);
      VkImageViewCreateInfo tvci = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
         .image = tex_img[i],
         .viewType = VK_IMAGE_VIEW_TYPE_2D,
         .format = VK_FORMAT_R8G8B8A8_UNORM,
         .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 }
      };
      CHECK(vkCreateImageView(dev, &tvci, NULL, &tex_view[i]));
   }
   VkSamplerCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
                               .magFilter = VK_FILTER_NEAREST,
                               .minFilter = VK_FILTER_NEAREST,
                               .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
                               .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
                               .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE };
   VkSampler sampler;
   CHECK(vkCreateSampler(dev, &sci, NULL, &sampler));

   VkDescriptorPoolSize dps = { VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 3 };
   VkDescriptorPoolCreateInfo dpci = { .sType =
                                          VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
                                       .maxSets = 3,
                                       .poolSizeCount = 1,
                                       .pPoolSizes = &dps };
   VkDescriptorPool dpool;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dpool));
   VkDescriptorSet dsets[3];
   for (int i = 0; i < 3; i++) {
      VkDescriptorSetAllocateInfo dsai = {
         .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
         .descriptorPool = dpool,
         .descriptorSetCount = 1,
         .pSetLayouts = &dsl
      };
      CHECK(vkAllocateDescriptorSets(dev, &dsai, &dsets[i]));
      VkDescriptorImageInfo dii = { sampler, tex_view[i],
                                    VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
      VkWriteDescriptorSet wds = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                                   .dstSet = dsets[i],
                                   .dstBinding = 0,
                                   .descriptorCount = 1,
                                   .descriptorType =
                                      VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
                                   .pImageInfo = &dii };
      vkUpdateDescriptorSets(dev, 1, &wds, 0, NULL);
   }

   /* staging buffer: streaming texel source, persistently mapped */
   const VkDeviceSize stream_size = CM_UPLOAD_TEX * CM_UPLOAD_TEX * 4;
   VkBuffer staging;
   VkDeviceMemory staging_mem;
   make_buffer(stream_size, VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
               VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
               &staging, &staging_mem);
   void *staging_ptr;
   CHECK(vkMapMemory(dev, staging_mem, 0, stream_size, 0, &staging_ptr));

   /* readback buffer for the pixel hash */
   const VkDeviceSize rb_size = CM_WIDTH * CM_HEIGHT * 4;
   VkBuffer rb_buf = VK_NULL_HANDLE;
   VkDeviceMemory rb_mem = VK_NULL_HANDLE;
   void *rb_ptr = NULL;
   if (o.hash) {
      make_buffer(rb_size, VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                  VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                     VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
                  &rb_buf, &rb_mem);
      CHECK(vkMapMemory(dev, rb_mem, 0, rb_size, 0, &rb_ptr));
   }

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

   /* one-time init upload: fill the two static textures */
   {
      uint8_t *texels = malloc(stream_size);
      VkCommandBufferBeginInfo cbbi = {
         .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
         .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT
      };
      for (int i = 0; i < 2; i++) {
         cm_fill_texture(texels, CM_UPLOAD_TEX, CM_UPLOAD_TEX, i);
         memcpy(staging_ptr, texels, stream_size);
         CHECK(vkBeginCommandBuffer(cmd, &cbbi));
         image_barrier(cmd, tex_img[i], VK_IMAGE_LAYOUT_UNDEFINED,
                       VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 0,
                       VK_ACCESS_TRANSFER_WRITE_BIT,
                       VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                       VK_PIPELINE_STAGE_TRANSFER_BIT);
         VkBufferImageCopy bic = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0,
                                                         0, 1 },
                                   .imageExtent = { CM_UPLOAD_TEX, CM_UPLOAD_TEX, 1 } };
         vkCmdCopyBufferToImage(cmd, staging, tex_img[i],
                                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &bic);
         image_barrier(cmd, tex_img[i], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                       VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                       VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT,
                       VK_PIPELINE_STAGE_TRANSFER_BIT,
                       VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT);
         CHECK(vkEndCommandBuffer(cmd));
         VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                             .commandBufferCount = 1,
                             .pCommandBuffers = &cmd };
         CHECK(vkQueueSubmit(queue, 1, &si, fence));
         CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
         CHECK(vkResetFences(dev, 1, &fence));
      }
      free(texels);
   }

   struct cm_times t = { 0 };
   uint8_t *stream_src = malloc(stream_size);
   int streaming_initialized = 0;

   for (int frame = 0; frame < o.nframes + o.warmup; frame++) {
      int last = frame == o.nframes + o.warmup - 1;
      double t0 = cm_now_ms();

      VkCommandBufferBeginInfo cbbi = {
         .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
         .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT
      };
      CHECK(vkBeginCommandBuffer(cmd, &cbbi));

      if (o.shape == CM_SHAPE_UPLOAD) {
         /* stream 1 MiB of texels through the staging buffer into tex 2 */
         cm_fill_stream(stream_src, frame);
         memcpy(staging_ptr, stream_src, stream_size);
         image_barrier(cmd, tex_img[2],
                       streaming_initialized ? VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL
                                             : VK_IMAGE_LAYOUT_UNDEFINED,
                       VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                       streaming_initialized ? VK_ACCESS_SHADER_READ_BIT : 0,
                       VK_ACCESS_TRANSFER_WRITE_BIT,
                       streaming_initialized ? VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT
                                             : VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                       VK_PIPELINE_STAGE_TRANSFER_BIT);
         VkBufferImageCopy bic = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0,
                                                         0, 1 },
                                   .imageExtent = { CM_UPLOAD_TEX, CM_UPLOAD_TEX, 1 } };
         vkCmdCopyBufferToImage(cmd, staging, tex_img[2],
                                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &bic);
         image_barrier(cmd, tex_img[2], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                       VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                       VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT,
                       VK_PIPELINE_STAGE_TRANSFER_BIT,
                       VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT);
         streaming_initialized = 1;
      }

      VkClearValue clear = { .color = { { 0.05f, 0.05f, 0.1f, 1.0f } } };
      VkRenderPassBeginInfo rpbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                                     .renderPass = rp,
                                     .framebuffer = fb,
                                     .renderArea = { { 0, 0 },
                                                     { CM_WIDTH, CM_HEIGHT } },
                                     .clearValueCount = 1,
                                     .pClearValues = &clear };
      vkCmdBeginRenderPass(cmd, &rpbi, VK_SUBPASS_CONTENTS_INLINE);

      int tex_shape = o.shape == CM_SHAPE_UPLOAD || o.shape == CM_SHAPE_DESKTOP;
      VkPipelineLayout layout = tex_shape ? tex_layout : flat_layout;
      int bound_tex = -1, bound_variant = -1;
      if (tex_shape)
         vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, tex_pipe);
      else if (o.shape == CM_SHAPE_DRAWS) {
         vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS,
                           flat_pipes[CM_NVARIANTS - 1]);
         bound_variant = CM_NVARIANTS - 1;
      }

      float pc[8];
      for (int i = 0; i < o.ndraws; i++) {
         cm_draw_params(o.shape, frame, i, pc);
         if (o.shape == CM_SHAPE_STATE) {
            int v = i % CM_NVARIANTS;
            if (v != bound_variant) {
               vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, flat_pipes[v]);
               bound_variant = v;
            }
         } else if (tex_shape) {
            int ti = cm_draw_texture(o.shape, i);
            if (ti != bound_tex) {
               vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, tex_layout,
                                       0, 1, &dsets[ti], 0, NULL);
               bound_tex = ti;
            }
         }
         vkCmdPushConstants(cmd, layout,
                            VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0,
                            32, pc);
         vkCmdDraw(cmd, tex_shape ? 6 : 3, 1, 0, 0);
      }
      vkCmdEndRenderPass(cmd);

      if (last && o.hash) {
         VkBufferImageCopy bic = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0,
                                                         0, 1 },
                                   .imageExtent = { CM_WIDTH, CM_HEIGHT, 1 } };
         vkCmdCopyImageToBuffer(cmd, target, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                                rb_buf, 1, &bic);
      }
      CHECK(vkEndCommandBuffer(cmd));
      double t1 = cm_now_ms();

      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                          .commandBufferCount = 1,
                          .pCommandBuffers = &cmd };
      CHECK(vkQueueSubmit(queue, 1, &si, fence));
      double t2 = cm_now_ms();

      CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
      CHECK(vkResetFences(dev, 1, &fence));
      double t3 = cm_now_ms();

      if (frame >= o.warmup) {
         t.draw += t1 - t0;
         t.flush += t2 - t1;
         t.sync += t3 - t2;
         t.total += t3 - t0;
         t.frames++;
      }
   }

   uint64_t hash = 0;
   if (o.hash)
      hash = cm_hash_pixels(rb_ptr, rb_size);

   cm_report("vk", &o, props.deviceName, &t, hash);
   vkDeviceWaitIdle(dev);
   return 0;
}
