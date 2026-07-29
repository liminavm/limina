// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* kkload — pure-Vulkan repro for the KK LOAD_OP_LOAD content loss found via
 * the zink PBO upload path (pbotest, 2026-07-28).
 *
 * Sequence, all on one queue with a fence wait between submits:
 *   1. RP1 -> image X (DONT_CARE): draw full-screen solid RED.
 *   2. sample X into target Y, copy Y to a buffer, check: all red.
 *   3. RP2 -> image X (LOAD): draw HALF-screen (bottom) solid GREEN.
 *   4. sample X into Y again, check: bottom green, top STILL RED.
 * If step 4's top is not red, LOAD_OP_LOAD lost the image contents.
 *
 * Each render pass is recorded in its own command buffer + submit, matching
 * the zink shape (upload RP and sampling draw in separate batches).
 * Build: cc -O0 -g -o kkload kkload.c -lvulkan  (VK_ICD_FILENAMES -> KK)
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

/* glslangValidator-compiled at build time (see Makefile): quad from
 * gl_VertexIndex covering rect in push constants; solid-color frag; and a
 * sampling frag. */

static VkDevice dev;
static VkPhysicalDevice phys;
static VkQueue queue;
static uint32_t qfi;

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
      fprintf(stderr, "missing %s (make kkload-shaders)\n", path);
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

struct img {
   VkImage img;
   VkDeviceMemory mem;
   VkImageView view;
};

static struct img
make_image(VkImageUsageFlags usage)
{
   struct img r;
   VkImageCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                             .imageType = VK_IMAGE_TYPE_2D,
                             .format = VK_FORMAT_R8G8B8A8_UNORM,
                             .extent = { DIM, DIM, 1 },
                             .mipLevels = 1,
                             .arrayLayers = 1,
                             .samples = VK_SAMPLE_COUNT_1_BIT,
                             .tiling = VK_IMAGE_TILING_OPTIMAL,
                             .usage = usage };
   CHECK(vkCreateImage(dev, &ici, NULL, &r.img));
   VkMemoryRequirements req;
   vkGetImageMemoryRequirements(dev, r.img, &req);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                .allocationSize = req.size,
                                .memoryTypeIndex =
                                   mem_type(req.memoryTypeBits,
                                            VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
   CHECK(vkAllocateMemory(dev, &mai, NULL, &r.mem));
   CHECK(vkBindImageMemory(dev, r.img, r.mem, 0));
   VkImageViewCreateInfo vci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = r.img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = VK_FORMAT_R8G8B8A8_UNORM,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 }
   };
   CHECK(vkCreateImageView(dev, &vci, NULL, &r.view));
   return r;
}

static VkRenderPass
make_rp(VkAttachmentLoadOp load, VkImageLayout initial, VkImageLayout final)
{
   VkAttachmentDescription att = { .format = VK_FORMAT_R8G8B8A8_UNORM,
                                   .samples = VK_SAMPLE_COUNT_1_BIT,
                                   .loadOp = load,
                                   .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                   .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
                                   .initialLayout = initial,
                                   .finalLayout = final };
   VkAttachmentReference ref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
   VkSubpassDescription sub = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
                                .colorAttachmentCount = 1,
                                .pColorAttachments = &ref };
   VkRenderPassCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
                                 .attachmentCount = 1,
                                 .pAttachments = &att,
                                 .subpassCount = 1,
                                 .pSubpasses = &sub };
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &ci, NULL, &rp));
   return rp;
}

static VkPipeline
make_pipe(VkRenderPass rp, VkPipelineLayout layout, VkShaderModule vs,
          VkShaderModule fs)
{
   VkPipelineShaderStageCreateInfo st[2] = {
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
   VkFormat cfmt = VK_FORMAT_R8G8B8A8_UNORM;
   VkPipelineRenderingCreateInfo prci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
      .colorAttachmentCount = 1,
      .pColorAttachmentFormats = &cfmt
   };
   VkGraphicsPipelineCreateInfo ci = { .sType =
                                          VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
                                       .pNext = rp == VK_NULL_HANDLE ? &prci : NULL,
                                       .stageCount = 2,
                                       .pStages = st,
                                       .pVertexInputState = &vin,
                                       .pInputAssemblyState = &ia,
                                       .pViewportState = &vps,
                                       .pRasterizationState = &rs,
                                       .pMultisampleState = &ms,
                                       .pColorBlendState = &cb,
                                       .layout = layout,
                                       .renderPass = rp,
                                       .subpass = 0 };
   VkPipeline p;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &ci, NULL, &p));
   return p;
}

static VkCommandPool cpool;

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

static int dyn;     /* KL_DYN=1: use dynamic rendering (the zink shape) */
static int general; /* KL_GENERAL=1: keep the image in GENERAL, no barriers
                     * after the initial transition (the unified-image-layouts
                     * zink shape; implies KL_DYN) */

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

static void
begin_dyn(VkCommandBuffer cmd, VkImageView view, VkAttachmentLoadOp load)
{
   VkRenderingAttachmentInfo att = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = view,
      .imageLayout = general ? VK_IMAGE_LAYOUT_GENERAL
                             : VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = load,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE
   };
   VkRenderingInfo ri = { .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
                          .renderArea = { { 0, 0 }, { DIM, DIM } },
                          .layerCount = 1,
                          .colorAttachmentCount = 1,
                          .pColorAttachments = &att };
   vkCmdBeginRendering(cmd, &ri);
}

int
main(void)
{
   general = getenv("KL_GENERAL") != NULL;
   dyn = general || getenv("KL_DYN") != NULL;
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "kkload",
                             .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ii = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                               .pApplicationInfo = &app };
   VkInstance inst;
   CHECK(vkCreateInstance(&ii, NULL, &inst));
   uint32_t n = 1;
   vkEnumeratePhysicalDevices(inst, &n, &phys);
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   fprintf(stderr, "device: %s mode: %s%s\n", props.deviceName,
           dyn ? "dynamic-rendering" : "legacy-renderpass",
           general ? "+general-layout" : "");

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
   VkPhysicalDeviceDynamicRenderingFeatures drf = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DYNAMIC_RENDERING_FEATURES,
      .dynamicRendering = VK_TRUE
   };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .pNext = dyn ? &drf : NULL,
                              .queueCreateInfoCount = 1,
                              .pQueueCreateInfos = &qci };
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, qfi, 0, &queue);
   VkCommandPoolCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .queueFamilyIndex = qfi };
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cpool));

   struct img X = make_image(VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                             VK_IMAGE_USAGE_SAMPLED_BIT);
   struct img Y = make_image(VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                             VK_IMAGE_USAGE_TRANSFER_SRC_BIT);

   VkRenderPass rp_first = make_rp(VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   VK_IMAGE_LAYOUT_UNDEFINED,
                                   VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
   VkRenderPass rp_load = make_rp(VK_ATTACHMENT_LOAD_OP_LOAD,
                                  VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                                  VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
   VkRenderPass rp_target = make_rp(VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                    VK_IMAGE_LAYOUT_UNDEFINED,
                                    VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL);

   VkFramebufferCreateInfo fbci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                                    .attachmentCount = 1,
                                    .width = DIM,
                                    .height = DIM,
                                    .layers = 1 };
   VkFramebuffer fb_X_first, fb_X_load, fb_Y;
   fbci.renderPass = rp_first;
   fbci.pAttachments = &X.view;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb_X_first));
   fbci.renderPass = rp_load;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb_X_load));
   fbci.renderPass = rp_target;
   fbci.pAttachments = &Y.view;
   CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fb_Y));

   /* layouts: solid (push rect+color), sample (set0=sampler2D) */
   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT,
                               0, 32 };
   VkPipelineLayoutCreateInfo plci = { .sType =
                                          VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                       .pushConstantRangeCount = 1,
                                       .pPushConstantRanges = &pcr };
   VkPipelineLayout solid_layout;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &solid_layout));

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
   plci.setLayoutCount = 1;
   plci.pSetLayouts = &dsl;
   VkPipelineLayout tex_layout;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &tex_layout));

   VkShaderModule vs = module("cm.vert.spv");
   VkShaderModule fs_flat = module("cm_flat.frag.spv");
   VkShaderModule fs_tex = module("cm_tex.frag.spv");
   VkPipeline p_first = make_pipe(dyn ? VK_NULL_HANDLE : rp_first, solid_layout, vs,
                                  fs_flat);
   VkPipeline p_load = dyn ? p_first : make_pipe(rp_load, solid_layout, vs, fs_flat);
   VkPipeline p_sample = make_pipe(dyn ? VK_NULL_HANDLE : rp_target, tex_layout, vs,
                                   fs_tex);

   VkSamplerCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
                               .magFilter = VK_FILTER_NEAREST,
                               .minFilter = VK_FILTER_NEAREST };
   VkSampler sampler;
   CHECK(vkCreateSampler(dev, &sci, NULL, &sampler));
   VkDescriptorPoolSize dps = { VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1 };
   VkDescriptorPoolCreateInfo dpci = { .sType =
                                          VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
                                       .maxSets = 1,
                                       .poolSizeCount = 1,
                                       .pPoolSizes = &dps };
   VkDescriptorPool dpool;
   CHECK(vkCreateDescriptorPool(dev, &dpci, NULL, &dpool));
   VkDescriptorSetAllocateInfo dsai = { .sType =
                                           VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
                                        .descriptorPool = dpool,
                                        .descriptorSetCount = 1,
                                        .pSetLayouts = &dsl };
   VkDescriptorSet dset;
   CHECK(vkAllocateDescriptorSets(dev, &dsai, &dset));
   VkDescriptorImageInfo dii = { sampler, X.view,
                                 general ? VK_IMAGE_LAYOUT_GENERAL
                                         : VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
   VkWriteDescriptorSet wds = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                                .dstSet = dset,
                                .dstBinding = 0,
                                .descriptorCount = 1,
                                .descriptorType =
                                   VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
                                .pImageInfo = &dii };
   vkUpdateDescriptorSets(dev, 1, &wds, 0, NULL);

   VkDeviceSize rb_size = DIM * DIM * 4;
   VkBuffer rb_buf;
   VkDeviceMemory rb_mem;
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                              .size = rb_size,
                              .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &rb_buf));
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, rb_buf, &req);
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                .allocationSize = req.size,
                                .memoryTypeIndex = mem_type(
                                   req.memoryTypeBits,
                                   VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                      VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
   CHECK(vkAllocateMemory(dev, &mai, NULL, &rb_mem));
   CHECK(vkBindBufferMemory(dev, rb_buf, rb_mem, 0));
   uint32_t *rb;
   CHECK(vkMapMemory(dev, rb_mem, 0, rb_size, 0, (void **)&rb));

   float red[8] = { -1, -1, 2, 2, 1, 0, 0, 1 };   /* full quad, red */
   float green[8] = { -1, -1, 2, 1, 0, 1, 0, 1 }; /* bottom half, green */

   VkClearValue clear = { 0 };
   VkRenderPassBeginInfo rbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                                 .renderArea = { { 0, 0 }, { DIM, DIM } },
                                 .clearValueCount = 1,
                                 .pClearValues = &clear };

   /* --- 1: RP1 draw red into X (separate submit) --- */
   VkCommandBuffer cmd = begin_cmd();
   if (dyn) {
      img_barrier(cmd, X.img, VK_IMAGE_LAYOUT_UNDEFINED,
                  general ? VK_IMAGE_LAYOUT_GENERAL
                          : VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL);
      begin_dyn(cmd, X.view, VK_ATTACHMENT_LOAD_OP_DONT_CARE);
   } else {
      rbi.renderPass = rp_first;
      rbi.framebuffer = fb_X_first;
      vkCmdBeginRenderPass(cmd, &rbi, VK_SUBPASS_CONTENTS_INLINE);
   }
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_first);
   vkCmdPushConstants(cmd, solid_layout,
                      VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, 32,
                      red);
   vkCmdDraw(cmd, 6, 1, 0, 0);
   if (dyn) {
      vkCmdEndRendering(cmd);
      if (!general)
         img_barrier(cmd, X.img, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                     VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
   } else
      vkCmdEndRenderPass(cmd);
   submit_wait(cmd);

   /* --- 2: sample X -> Y -> buffer, check all red --- */
   int fail = 0;
   for (int pass = 0; pass < 2; pass++) {
      cmd = begin_cmd();
      if (pass == 1) {
         /* RP2: LOAD + bottom-half green into X */
         if (dyn) {
            if (!general)
               img_barrier(cmd, X.img, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                           VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL);
            begin_dyn(cmd, X.view, VK_ATTACHMENT_LOAD_OP_LOAD);
         } else {
            rbi.renderPass = rp_load;
            rbi.framebuffer = fb_X_load;
            vkCmdBeginRenderPass(cmd, &rbi, VK_SUBPASS_CONTENTS_INLINE);
         }
         vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_load);
         vkCmdPushConstants(cmd, solid_layout,
                            VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT,
                            0, 32, green);
         vkCmdDraw(cmd, 6, 1, 0, 0);
         if (dyn) {
            vkCmdEndRendering(cmd);
            if (!general)
               img_barrier(cmd, X.img, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                           VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
         } else
            vkCmdEndRenderPass(cmd);
         submit_wait(cmd);
         cmd = begin_cmd();
      }
      if (dyn) {
         img_barrier(cmd, Y.img, VK_IMAGE_LAYOUT_UNDEFINED,
                     VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL);
         begin_dyn(cmd, Y.view, VK_ATTACHMENT_LOAD_OP_DONT_CARE);
      } else {
         rbi.renderPass = rp_target;
         rbi.framebuffer = fb_Y;
         vkCmdBeginRenderPass(cmd, &rbi, VK_SUBPASS_CONTENTS_INLINE);
      }
      vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, p_sample);
      vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, tex_layout, 0, 1,
                              &dset, 0, NULL);
      float full[8] = { -1, -1, 2, 2, 1, 1, 1, 1 };
      vkCmdPushConstants(cmd, tex_layout,
                         VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0,
                         32, full);
      vkCmdDraw(cmd, 6, 1, 0, 0);
      if (dyn) {
         vkCmdEndRendering(cmd);
         img_barrier(cmd, Y.img, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                     VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL);
      } else
         vkCmdEndRenderPass(cmd);
      VkBufferImageCopy bic = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0,
                                                      1 },
                                .imageExtent = { DIM, DIM, 1 } };
      vkCmdCopyImageToBuffer(cmd, Y.img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, rb_buf,
                             1, &bic);
      submit_wait(cmd);

      uint32_t top = rb[(DIM - 1) * DIM + DIM / 2]; /* top row (y max) */
      uint32_t bot = rb[0 * DIM + DIM / 2];         /* bottom row (y min) */
      const uint32_t RED = 0xff0000ffu, GREEN = 0xff00ff00u;
      if (pass == 0) {
         int ok = top == RED && bot == RED;
         printf("pass 1 (after first render): top=%08x bot=%08x %s\n", top, bot,
                ok ? "OK (all red)" : "WRONG");
         fail |= !ok;
      } else {
         int ok = top == RED && bot == GREEN;
         printf("pass 2 (after LOAD+half-green): top=%08x bot=%08x %s\n", top, bot,
                ok ? "OK (top red kept, bottom green)"
                   : "WRONG (LOAD_OP_LOAD lost contents?)");
         fail |= !ok;
      }
   }
   printf("kkload: %s\n", fail ? "FAIL" : "PASS");
   vkDeviceWaitIdle(dev);
   return fail;
}
