// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* kktexel — raw-Vulkan replica of the zink PBO upload loop against KK:
 * per frame, write a pattern into a host-visible buffer, create a FRESH
 * VkBufferView + descriptor (zink's view cache misses every frame), render a
 * full-screen quad into image X whose fragment shader texelFetches the
 * buffer, then sample X into Y and read back. Content must equal the
 * frame's pattern.
 *
 *   KKT_REUSE_VIEW=1   create the buffer view + descriptor once (control)
 *   KKT_FRAMES=N       default 3
 *
 * Build: cc -O0 -g -o kktexel kktexel.c -lvulkan (VK_ICD_FILENAMES -> KK)
 * Needs cm.vert.spv, cm_texel.frag.spv, cm_tex.frag.spv (make spv).
 */

#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(x)                                                                \
   do {                                                                         \
      VkResult r_ = (x);                                                        \
      if (r_ != VK_SUCCESS) {                                                   \
         fprintf(stderr, "FAIL %s = %d @ %d\n", #x, r_, __LINE__);              \
         exit(1);                                                               \
      }                                                                         \
   } while (0)

#define DIM 64
#define BYTES (DIM * DIM * 4)

static VkDevice dev;
static VkPhysicalDevice phys;
static VkQueue queue;
static uint32_t qfi;
static VkCommandPool cpool;

static uint32_t
mem_type(uint32_t bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(phys, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   exit(3);
}

static void *
read_file(const char *path, size_t *size)
{
   FILE *f = fopen(path, "rb");
   if (!f) {
      fprintf(stderr, "missing %s (make spv)\n", path);
      exit(1);
   }
   fseek(f, 0, SEEK_END);
   *size = ftell(f);
   fseek(f, 0, SEEK_SET);
   void *b = malloc(*size);
   if (fread(b, 1, *size, f) != *size)
      exit(1);
   fclose(f);
   return b;
}

static VkShaderModule
module(const char *path)
{
   size_t sz;
   void *spv = read_file(path, &sz);
   VkShaderModuleCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                   .codeSize = sz,
                                   .pCode = spv };
   VkShaderModule m;
   CHECK(vkCreateShaderModule(dev, &ci, NULL, &m));
   return m;
}

static void
fill(uint32_t *p, int frame)
{
   for (int i = 0; i < DIM * DIM; i++)
      p[i] = 0x9e3779b9u * (uint32_t)(frame + 1) + (uint32_t)i * 2654435761u;
}

static VkCommandBuffer
begin_cmd(void)
{
   VkCommandBufferAllocateInfo ai = { .sType =
                                         VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                      .commandPool = cpool,
                                      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                      .commandBufferCount = 1 };
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &ai, &cmd));
   VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                   .flags =
                                      VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   CHECK(vkBeginCommandBuffer(cmd, &bi));
   return cmd;
}

static void
submit_wait(VkCommandBuffer cmd)
{
   CHECK(vkEndCommandBuffer(cmd));
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                       .commandBufferCount = 1,
                       .pCommandBuffers = &cmd };
   CHECK(vkQueueSubmit(queue, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
   vkDestroyFence(dev, fence, NULL);
   vkFreeCommandBuffers(dev, cpool, 1, &cmd);
}

/* KKT_ONE_SUBMIT: submit without any CPU-side wait (zink's shape — cross-
 * submit ordering rests on the driver's fence chain alone). */
static void
submit_nowait(VkCommandBuffer cmd)
{
   CHECK(vkEndCommandBuffer(cmd));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                       .commandBufferCount = 1,
                       .pCommandBuffers = &cmd };
   CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
}

static void
img_barrier(VkCommandBuffer cmd, VkImage img, VkImageLayout from, VkImageLayout to)
{
   VkImageMemoryBarrier b = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                              .srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
                              .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT |
                                               VK_ACCESS_MEMORY_WRITE_BIT,
                              .oldLayout = from,
                              .newLayout = to,
                              .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                              .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                              .image = img,
                              .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0,
                                                    1 } };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                        VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 0, NULL, 0, NULL, 1,
                        &b);
}

int
main(void)
{
   int frames = getenv("KKT_FRAMES") ? atoi(getenv("KKT_FRAMES")) : 3;
   int reuse_view = getenv("KKT_REUSE_VIEW") != NULL;

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "kktexel",
                             .apiVersion = VK_API_VERSION_1_1 };
   VkInstanceCreateInfo ii = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                               .pApplicationInfo = &app };
   VkInstance inst;
   CHECK(vkCreateInstance(&ii, NULL, &inst));
   uint32_t n = 1;
   vkEnumeratePhysicalDevices(inst, &n, &phys);
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   fprintf(stderr, "device: %s variant: %s\n", props.deviceName,
           reuse_view ? "reuse-view" : "fresh-view-per-frame");

   uint32_t nqf = 16;
   VkQueueFamilyProperties qf[16];
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &nqf, qf);
   for (qfi = 0; qfi < nqf; qfi++)
      if (qf[qfi].queueFlags & VK_QUEUE_GRAPHICS_BIT)
         break;
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                   .queueFamilyIndex = qfi,
                                   .queueCount = 1,
                                   .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .queueCreateInfoCount = 1,
                              .pQueueCreateInfos = &qci };
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, qfi, 0, &queue);
   VkCommandPoolCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .queueFamilyIndex = qfi };
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cpool));

   /* the "PBO": host-visible uniform texel buffer, persistently mapped */
   VkBuffer pbo;
   VkDeviceMemory pbo_mem;
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                              .size = BYTES,
                              .usage = VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &pbo));
   VkMemoryRequirements breq;
   vkGetBufferMemoryRequirements(dev, pbo, &breq);
   VkMemoryAllocateInfo bmai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                 .allocationSize = breq.size,
                                 .memoryTypeIndex = mem_type(
                                    breq.memoryTypeBits,
                                    VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                       VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
   CHECK(vkAllocateMemory(dev, &bmai, NULL, &pbo_mem));
   CHECK(vkBindBufferMemory(dev, pbo, pbo_mem, 0));
   uint32_t *pbo_ptr;
   CHECK(vkMapMemory(dev, pbo_mem, 0, BYTES, 0, (void **)&pbo_ptr));

   /* images X (rendered+sampled) and Y (target+copy src) */
   VkImage X, Y;
   VkDeviceMemory Xm, Ym;
   VkImageView Xv, Yv;
   for (int i = 0; i < 2; i++) {
      VkImageCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                                .imageType = VK_IMAGE_TYPE_2D,
                                .format = VK_FORMAT_R8G8B8A8_UNORM,
                                .extent = { DIM, DIM, 1 },
                                .mipLevels = 1,
                                .arrayLayers = 1,
                                .samples = VK_SAMPLE_COUNT_1_BIT,
                                .tiling = VK_IMAGE_TILING_OPTIMAL,
                                .usage = i == 0
                                            ? VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                                                 VK_IMAGE_USAGE_SAMPLED_BIT
                                            : VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                                                 VK_IMAGE_USAGE_TRANSFER_SRC_BIT };
      VkImage *img = i == 0 ? &X : &Y;
      VkDeviceMemory *mem = i == 0 ? &Xm : &Ym;
      VkImageView *view = i == 0 ? &Xv : &Yv;
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
      VkImageViewCreateInfo vci = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
         .image = *img,
         .viewType = VK_IMAGE_VIEW_TYPE_2D,
         .format = VK_FORMAT_R8G8B8A8_UNORM,
         .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 }
      };
      CHECK(vkCreateImageView(dev, &vci, NULL, view));
   }

   /* render passes (legacy is fine — the fetch is what's under test) */
   VkAttachmentDescription att = { .format = VK_FORMAT_R8G8B8A8_UNORM,
                                   .samples = VK_SAMPLE_COUNT_1_BIT,
                                   .loadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                   .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
                                   .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                   .finalLayout =
                                      VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
   VkAttachmentReference ref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
   VkSubpassDescription sub = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
                                .colorAttachmentCount = 1,
                                .pColorAttachments = &ref };
   VkRenderPassCreateInfo rpci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
                                   .attachmentCount = 1,
                                   .pAttachments = &att,
                                   .subpassCount = 1,
                                   .pSubpasses = &sub };
   VkRenderPass rp_upload;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp_upload));
   att.finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
   VkRenderPass rp_target;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp_target));

   VkFramebufferCreateInfo fbci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                                    .attachmentCount = 1,
                                    .width = DIM,
                                    .height = DIM,
                                    .layers = 1 };
   VkFramebuffer fb_X, fb_Y;
   fbci.renderPass = rp_upload;
   fbci.pAttachments = &Xv;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb_X));
   fbci.renderPass = rp_target;
   fbci.pAttachments = &Yv;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb_Y));

   /* descriptor set layouts: texel buffer (upload), sampler (sample) */
   VkDescriptorSetLayoutBinding tb_bind = { .binding = 0,
                                            .descriptorType =
                                               VK_DESCRIPTOR_TYPE_UNIFORM_TEXEL_BUFFER,
                                            .descriptorCount = 1,
                                            .stageFlags =
                                               VK_SHADER_STAGE_FRAGMENT_BIT };
   VkDescriptorSetLayoutCreateInfo dslci = {
      .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
      .bindingCount = 1,
      .pBindings = &tb_bind
   };
   VkDescriptorSetLayout dsl_tb;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl_tb));
   VkDescriptorSetLayoutBinding smp_bind = { .binding = 0,
                                             .descriptorType =
                                                VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
                                             .descriptorCount = 1,
                                             .stageFlags =
                                                VK_SHADER_STAGE_FRAGMENT_BIT };
   dslci.pBindings = &smp_bind;
   VkDescriptorSetLayout dsl_smp;
   CHECK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl_smp));

   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT,
                               0, 32 };
   VkPipelineLayoutCreateInfo plci = { .sType =
                                          VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                       .setLayoutCount = 1,
                                       .pSetLayouts = &dsl_tb,
                                       .pushConstantRangeCount = 1,
                                       .pPushConstantRanges = &pcr };
   VkPipelineLayout lay_upload;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &lay_upload));
   plci.pSetLayouts = &dsl_smp;
   VkPipelineLayout lay_sample;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &lay_sample));

   VkShaderModule vs = module("cm.vert.spv");
   VkShaderModule fs_texel = module("cm_texel.frag.spv");
   VkShaderModule fs_tex = module("cm_tex.frag.spv");

   /* pipeline helper (viewport DIM fixed) */
   VkPipelineShaderStageCreateInfo st[2] = {
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_VERTEX_BIT,
        .module = vs,
        .pName = "main" },
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
        .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
        .pName = "main" },
   };
   VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST
   };
   VkViewport vp = { 0, 0, DIM, DIM, 0, 1 };
   VkRect2D sc = { { 0, 0 }, { DIM, DIM } };
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
   VkGraphicsPipelineCreateInfo gci = { .sType =
                                           VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
                                        .stageCount = 2,
                                        .pStages = st,
                                        .pVertexInputState = &vin,
                                        .pInputAssemblyState = &ia,
                                        .pViewportState = &vps,
                                        .pRasterizationState = &rs,
                                        .pMultisampleState = &ms,
                                        .pColorBlendState = &cb,
                                        .subpass = 0 };
   st[1].module = fs_texel;
   gci.layout = lay_upload;
   gci.renderPass = rp_upload;
   VkPipeline p_upload;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gci, NULL, &p_upload));
   st[1].module = fs_tex;
   gci.layout = lay_sample;
   gci.renderPass = rp_target;
   VkPipeline p_sample;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gci, NULL, &p_sample));

   /* sampler descriptor (X), fixed */
   VkSamplerCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
                               .magFilter = VK_FILTER_NEAREST,
                               .minFilter = VK_FILTER_NEAREST };
   VkSampler sampler;
   CHECK(vkCreateSampler(dev, &sci, NULL, &sampler));
   VkDescriptorPoolSize sizes[2] = {
      { VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1 },
      { VK_DESCRIPTOR_TYPE_UNIFORM_TEXEL_BUFFER, 64 },
   };
   VkDescriptorPoolCreateInfo dpci = { .sType =
                                          VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
                                       .maxSets = 65,
                                       .poolSizeCount = 2,
                                       .pPoolSizes = sizes };
   VkDescriptorPool dpool;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dpool));

   VkDescriptorSetAllocateInfo dsai = { .sType =
                                           VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
                                        .descriptorPool = dpool,
                                        .descriptorSetCount = 1,
                                        .pSetLayouts = &dsl_smp };
   VkDescriptorSet dset_smp;
   CHECK(vkAllocateDescriptorSets(dev, &dsai, &dset_smp));
   VkDescriptorImageInfo dii = { sampler, Xv, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
   VkWriteDescriptorSet wds = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                                .dstSet = dset_smp,
                                .dstBinding = 0,
                                .descriptorCount = 1,
                                .descriptorType =
                                   VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
                                .pImageInfo = &dii };
   vkUpdateDescriptorSets(dev, 1, &wds, 0, NULL);

   /* readback buffer */
   VkBuffer rb_buf;
   VkDeviceMemory rb_mem;
   bci.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &rb_buf));
   vkGetBufferMemoryRequirements(dev, rb_buf, &breq);
   bmai.allocationSize = breq.size;
   bmai.memoryTypeIndex = mem_type(breq.memoryTypeBits,
                                   VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                      VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
   CHECK(vkAllocateMemory(dev, &bmai, NULL, &rb_mem));
   CHECK(vkBindBufferMemory(dev, rb_buf, rb_mem, 0));
   uint32_t *rb;
   CHECK(vkMapMemory(dev, rb_mem, 0, BYTES, 0, (void **)&rb));

   VkBufferView reused_view = VK_NULL_HANDLE;
   VkDescriptorSet reused_set = VK_NULL_HANDLE;
   uint32_t *expected = malloc(BYTES);
   int failures = 0;

   VkClearValue clear = { 0 };
   /* KKT_FRESH_MEM: allocate a brand-new VkDeviceMemory + VkBuffer for the
    * texel data every frame — probes whether allocations made AFTER the
    * device's first submits ever become GPU-visible (residency-set commit
    * propagation). Deliberately leaks; the run is a few frames. */
   int fresh_mem = getenv("KKT_FRESH_MEM") != NULL;

   for (int frame = 0; frame < frames; frame++) {
      fill(expected, frame);

      VkBuffer frame_pbo = pbo;
      if (fresh_mem) {
         VkBufferCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                                    .size = BYTES,
                                    .usage =
                                       VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT };
         CHECK(vkCreateBuffer(dev, &fci, NULL, &frame_pbo));
         VkMemoryRequirements freq;
         vkGetBufferMemoryRequirements(dev, frame_pbo, &freq);
         VkMemoryAllocateInfo fmai = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            .allocationSize = freq.size,
            .memoryTypeIndex =
               mem_type(freq.memoryTypeBits,
                        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                           VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)
         };
         VkDeviceMemory fmem;
         CHECK(vkAllocateMemory(dev, &fmai, NULL, &fmem));
         CHECK(vkBindBufferMemory(dev, frame_pbo, fmem, 0));
         uint32_t *fptr;
         CHECK(vkMapMemory(dev, fmem, 0, BYTES, 0, (void **)&fptr));
         memcpy(fptr, expected, BYTES);
      } else {
         memcpy(pbo_ptr, expected, BYTES);
      }

      VkBufferView view;
      VkDescriptorSet dset_tb;
      if (reuse_view && reused_view) {
         view = reused_view;
         dset_tb = reused_set;
      } else {
         VkBufferViewCreateInfo bvci = {
            .sType = VK_STRUCTURE_TYPE_BUFFER_VIEW_CREATE_INFO,
            .buffer = frame_pbo,
            .format = VK_FORMAT_R8G8B8A8_UNORM,
            .range = BYTES
         };
         CHECK(vkCreateBufferView(dev, &bvci, NULL, &view));
         dsai.pSetLayouts = &dsl_tb;
         /* KKT_FRESH_POOL: brand-new VkDescriptorPool each frame, like zink's
          * per-batch pools — the set's backing BO is then a fresh allocation. */
         if (getenv("KKT_FRESH_POOL")) {
            VkDescriptorPool fp;
            CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &fp));
            dsai.descriptorPool = fp;
         }
         CHECK(vkAllocateDescriptorSets(dev, &dsai, &dset_tb));
         VkWriteDescriptorSet w = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                                    .dstSet = dset_tb,
                                    .dstBinding = 0,
                                    .descriptorCount = 1,
                                    .descriptorType =
                                       VK_DESCRIPTOR_TYPE_UNIFORM_TEXEL_BUFFER,
                                    .pTexelBufferView = &view };
         vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
         if (reuse_view) {
            reused_view = view;
            reused_set = dset_tb;
         }
      }

      /* upload draw: texel-fetch the buffer into X */
      int one_submit = getenv("KKT_ONE_SUBMIT") != NULL;
      VkCommandBuffer cmd = begin_cmd();
      VkRenderPassBeginInfo rbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                                    .renderPass = rp_upload,
                                    .framebuffer = fb_X,
                                    .renderArea = { { 0, 0 }, { DIM, DIM } },
                                    .clearValueCount = 1,
                                    .pClearValues = &clear };
      vkCmdBeginRenderPass(cmd, &rbi, VK_SUBPASS_CONTENTS_INLINE);
      vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_upload);
      vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, lay_upload, 0, 1,
                              &dset_tb, 0, NULL);
      float pc[8] = { -1, -1, 2, 2, DIM, 0, 0, 0 };
      vkCmdPushConstants(cmd, lay_upload,
                         VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0,
                         32, pc);
      vkCmdDraw(cmd, 6, 1, 0, 0);
      vkCmdEndRenderPass(cmd);
      if (one_submit)
         submit_nowait(cmd);
      else
         submit_wait(cmd);

      /* sample X into Y, copy out, compare */
      cmd = begin_cmd();
      rbi.renderPass = rp_target;
      rbi.framebuffer = fb_Y;
      vkCmdBeginRenderPass(cmd, &rbi, VK_SUBPASS_CONTENTS_INLINE);
      vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_sample);
      vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, lay_sample, 0, 1,
                              &dset_smp, 0, NULL);
      float pc2[8] = { -1, -1, 2, 2, 1, 1, 1, 1 };
      vkCmdPushConstants(cmd, lay_sample,
                         VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0,
                         32, pc2);
      vkCmdDraw(cmd, 6, 1, 0, 0);
      vkCmdEndRenderPass(cmd);
      VkBufferImageCopy bic = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0,
                                                      1 },
                                .imageExtent = { DIM, DIM, 1 } };
      vkCmdCopyImageToBuffer(cmd, Y, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, rb_buf, 1,
                             &bic);
      submit_wait(cmd);

      if (memcmp(rb, expected, BYTES)) {
         int zeros = 0;
         for (int i = 0; i < DIM * DIM; i++)
            zeros += rb[i] == 0;
         printf("frame %d: STALE (zeros=%d/%d rb[0]=%08x want %08x)\n", frame, zeros,
                DIM * DIM, rb[0], expected[0]);
         failures++;
      } else {
         printf("frame %d: ok\n", frame);
      }
   }
   printf("kktexel: %s (%d failures / %d frames)\n", failures ? "FAIL" : "PASS",
          failures, frames);
   vkDeviceWaitIdle(dev);
   return failures ? 1 : 0;
}
