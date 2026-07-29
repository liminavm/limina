// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* crossmark Vulkan backend — see crossmark.h for the scene contract.
 * Derived from spikes/venus-cmdstream-probe/drawstorm.c.
 * -p/-F (HAVE_WAYLAND builds): render to a Wayland swapchain, uncapped
 * (mailbox > immediate > fifo), timing the acquire+present separately. */

#include <vulkan/vulkan.h>
#ifdef HAVE_WAYLAND
#include <vulkan/vulkan_wayland.h>
#include "cmwin.h"
#endif
#include "crossmark.h"

#define CHECK(x)                                                                \
   do {                                                                         \
      VkResult r_ = (x);                                                        \
      if (r_ != VK_SUCCESS) {                                                   \
         fprintf(stderr, "FAIL %s = %d @ %s:%d\n", #x, r_, __FILE__, __LINE__); \
         exit(1);                                                               \
      }                                                                         \
   } while (0)

#define MAX_SWAP_IMAGES 8

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

static VkImageView
make_view(VkImage img, VkFormat format)
{
   VkImageViewCreateInfo ivci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = format,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 }
   };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &ivci, NULL, &view));
   return view;
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
#ifndef HAVE_WAYLAND
   if (o.present) {
      fprintf(stderr, "-p/-F need a wayland-enabled build (Linux guest)\n");
      return 1;
   }
#endif

   const char *inst_exts[2];
   uint32_t n_inst_exts = 0;
   if (o.present) {
      inst_exts[n_inst_exts++] = "VK_KHR_surface";
      inst_exts[n_inst_exts++] = "VK_KHR_wayland_surface";
   }
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "crossmark",
                             .apiVersion = VK_API_VERSION_1_1 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app,
                                .enabledExtensionCount = n_inst_exts,
                                .ppEnabledExtensionNames = inst_exts };
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
   const char *dev_exts[1] = { "VK_KHR_swapchain" };
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                   .queueFamilyIndex = qfi,
                                   .queueCount = 1,
                                   .pQueuePriorities = &prio };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .queueCreateInfoCount = 1,
                              .pQueueCreateInfos = &qci,
                              .enabledExtensionCount = o.present ? 1u : 0u,
                              .ppEnabledExtensionNames = dev_exts };
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
   VkQueue queue;
   vkGetDeviceQueue(dev, qfi, 0, &queue);

   /* ---- render target(s): offscreen image, or a Wayland swapchain ---- */
   uint32_t width = CM_WIDTH, height = CM_HEIGHT;
   VkFormat color_format = VK_FORMAT_R8G8B8A8_UNORM;
   VkImage target = VK_NULL_HANDLE;
   VkDeviceMemory target_mem;
   VkSwapchainKHR swapchain = VK_NULL_HANDLE;
   (void)swapchain; /* referenced only in HAVE_WAYLAND blocks */
   VkImage swap_images[MAX_SWAP_IMAGES];
   uint32_t n_swap = 0;
#ifdef HAVE_WAYLAND
   struct cm_win *win = NULL;
   VkSurfaceKHR surface = VK_NULL_HANDLE;
   if (o.present) {
      win = cm_win_create(CM_WIDTH, CM_HEIGHT, o.fullscreen, "crossmark");
      if (!win)
         return 1;
      width = win->width;
      height = win->height;
      VkWaylandSurfaceCreateInfoKHR wsci = {
         .sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
         .display = win->dpy,
         .surface = win->surface
      };
      CHECK(vkCreateWaylandSurfaceKHR(inst, &wsci, NULL, &surface));
      VkBool32 supported = VK_FALSE;
      vkGetPhysicalDeviceSurfaceSupportKHR(phys, qfi, surface, &supported);
      if (!supported) {
         fprintf(stderr, "queue family has no present support\n");
         return 1;
      }

      VkSurfaceFormatKHR fmts[32];
      uint32_t nfmt = 32;
      vkGetPhysicalDeviceSurfaceFormatsKHR(phys, surface, &nfmt, fmts);
      color_format = fmts[0].format;
      VkColorSpaceKHR color_space = fmts[0].colorSpace;
      for (uint32_t i = 0; i < nfmt; i++)
         if (fmts[i].format == VK_FORMAT_B8G8R8A8_UNORM ||
             fmts[i].format == VK_FORMAT_R8G8B8A8_UNORM) {
            color_format = fmts[i].format;
            color_space = fmts[i].colorSpace;
            break;
         }

      VkPresentModeKHR modes[8];
      uint32_t nmode = 8;
      vkGetPhysicalDeviceSurfacePresentModesKHR(phys, surface, &nmode, modes);
      VkPresentModeKHR want = VK_PRESENT_MODE_FIFO_KHR;
      for (uint32_t i = 0; i < nmode; i++)
         if (modes[i] == VK_PRESENT_MODE_IMMEDIATE_KHR &&
             want != VK_PRESENT_MODE_MAILBOX_KHR)
            want = VK_PRESENT_MODE_IMMEDIATE_KHR;
         else if (modes[i] == VK_PRESENT_MODE_MAILBOX_KHR)
            want = VK_PRESENT_MODE_MAILBOX_KHR;
      fprintf(stderr, "present mode: %s\n",
              want == VK_PRESENT_MODE_MAILBOX_KHR     ? "mailbox"
              : want == VK_PRESENT_MODE_IMMEDIATE_KHR ? "immediate"
                                                      : "fifo (VSYNC-CAPPED)");

      VkSurfaceCapabilitiesKHR caps;
      vkGetPhysicalDeviceSurfaceCapabilitiesKHR(phys, surface, &caps);
      if (caps.currentExtent.width != 0xffffffffu) {
         width = caps.currentExtent.width;
         height = caps.currentExtent.height;
      }
      uint32_t min_images = caps.minImageCount + 1;
      if (caps.maxImageCount && min_images > caps.maxImageCount)
         min_images = caps.maxImageCount;
      VkSwapchainCreateInfoKHR scci = {
         .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
         .surface = surface,
         .minImageCount = min_images,
         .imageFormat = color_format,
         .imageColorSpace = color_space,
         .imageExtent = { width, height },
         .imageArrayLayers = 1,
         .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
         .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
         .preTransform = caps.currentTransform,
         .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
         .presentMode = want,
         .clipped = VK_TRUE
      };
      CHECK(vkCreateSwapchainKHR(dev, &scci, NULL, &swapchain));
      n_swap = MAX_SWAP_IMAGES;
      CHECK(vkGetSwapchainImagesKHR(dev, swapchain, &n_swap, swap_images));
      fprintf(stderr, "swapchain: %ux%u x%u images\n", width, height, n_swap);
   }
#endif
   if (!o.present) {
      make_image(width, height,
                 VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                 &target, &target_mem);
      swap_images[0] = target;
      n_swap = 1;
   }

   VkAttachmentDescription att = { .format = color_format,
                                   .samples = VK_SAMPLE_COUNT_1_BIT,
                                   .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
                                   .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                   .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                   .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
                                   .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                   .finalLayout =
                                      o.present
                                         ? VK_IMAGE_LAYOUT_PRESENT_SRC_KHR
                                         : VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL };
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

   VkImageView fb_views[MAX_SWAP_IMAGES];
   VkFramebuffer fbs[MAX_SWAP_IMAGES];
   for (uint32_t i = 0; i < n_swap; i++) {
      fb_views[i] = make_view(swap_images[i], color_format);
      VkFramebufferCreateInfo fbci = { .sType =
                                          VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                                       .renderPass = rp,
                                       .attachmentCount = 1,
                                       .pAttachments = &fb_views[i],
                                       .width = width,
                                       .height = height,
                                       .layers = 1 };
      CHECK(vkCreateFramebuffer(dev, &fbci, NULL, &fbs[i]));
   }

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

   VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST
   };
   /* dynamic viewport/scissor so the same pipelines serve the offscreen and
    * arbitrary-size present paths */
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .scissorCount = 1
   };
   VkDynamicState dyn_states[2] = { VK_DYNAMIC_STATE_VIEWPORT,
                                    VK_DYNAMIC_STATE_SCISSOR };
   VkPipelineDynamicStateCreateInfo dyn = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
      .dynamicStateCount = 2,
      .pDynamicStates = dyn_states
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
         .pDynamicState = &dyn,
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
         .pDynamicState = &dyn,
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
      tex_view[i] = make_view(tex_img[i], VK_FORMAT_R8G8B8A8_UNORM);
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

   const VkDeviceSize stream_size = CM_UPLOAD_TEX * CM_UPLOAD_TEX * 4;
   VkBuffer staging;
   VkDeviceMemory staging_mem;
   make_buffer(stream_size, VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
               VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
               &staging, &staging_mem);
   void *staging_ptr;
   CHECK(vkMapMemory(dev, staging_mem, 0, stream_size, 0, &staging_ptr));

   const VkDeviceSize rb_size = (VkDeviceSize)width * height * 4;
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
   VkSemaphore acq_sem = VK_NULL_HANDLE, done_sem = VK_NULL_HANDLE;
   if (o.present) {
      VkSemaphoreCreateInfo semci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
      CHECK(vkCreateSemaphore(dev, &semci, NULL, &acq_sem));
      CHECK(vkCreateSemaphore(dev, &semci, NULL, &done_sem));
   }

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

      uint32_t img_idx = 0;
      double t_acq = 0;
      if (o.present) {
#ifdef HAVE_WAYLAND
         double a0 = cm_now_ms();
         VkResult ar = vkAcquireNextImageKHR(dev, swapchain, UINT64_MAX, acq_sem,
                                             VK_NULL_HANDLE, &img_idx);
         if (ar == VK_ERROR_OUT_OF_DATE_KHR) {
            fprintf(stderr, "swapchain out of date (resize?) — rerun; v1 does "
                            "not recreate\n");
            exit(2);
         }
         t_acq = cm_now_ms() - a0;
         t0 = cm_now_ms(); /* draw section starts after acquire */
#endif
      }

      VkCommandBufferBeginInfo cbbi = {
         .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
         .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT
      };
      CHECK(vkBeginCommandBuffer(cmd, &cbbi));

      if (o.shape == CM_SHAPE_UPLOAD) {
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
                                     .framebuffer = fbs[img_idx],
                                     .renderArea = { { 0, 0 }, { width, height } },
                                     .clearValueCount = 1,
                                     .pClearValues = &clear };
      vkCmdBeginRenderPass(cmd, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
      VkViewport vp = { 0, 0, (float)width, (float)height, 0, 1 };
      VkRect2D sc = { { 0, 0 }, { width, height } };
      vkCmdSetViewport(cmd, 0, 1, &vp);
      vkCmdSetScissor(cmd, 0, 1, &sc);

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

      if (last && o.hash && !o.present) {
         VkBufferImageCopy bic = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0,
                                                         0, 1 },
                                   .imageExtent = { width, height, 1 } };
         vkCmdCopyImageToBuffer(cmd, target, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                                rb_buf, 1, &bic);
      }
      CHECK(vkEndCommandBuffer(cmd));
      double t1 = cm_now_ms();

      VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                          .waitSemaphoreCount = o.present ? 1u : 0u,
                          .pWaitSemaphores = &acq_sem,
                          .pWaitDstStageMask = &wait_stage,
                          .commandBufferCount = 1,
                          .pCommandBuffers = &cmd,
                          .signalSemaphoreCount = o.present ? 1u : 0u,
                          .pSignalSemaphores = &done_sem };
      CHECK(vkQueueSubmit(queue, 1, &si, fence));
      double t2 = cm_now_ms();

      CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
      CHECK(vkResetFences(dev, 1, &fence));
      double t3 = cm_now_ms();

      double t_present = t_acq;
      if (o.present) {
#ifdef HAVE_WAYLAND
         double p0 = cm_now_ms();
         VkPresentInfoKHR pi = { .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
                                 .waitSemaphoreCount = 1,
                                 .pWaitSemaphores = &done_sem,
                                 .swapchainCount = 1,
                                 .pSwapchains = &swapchain,
                                 .pImageIndices = &img_idx };
         VkResult pr = vkQueuePresentKHR(queue, &pi);
         if (pr == VK_ERROR_OUT_OF_DATE_KHR) {
            fprintf(stderr, "swapchain out of date at present — rerun\n");
            exit(2);
         }
         cm_win_pump(win);
         t_present += cm_now_ms() - p0;
#endif
      }

      if (frame >= o.warmup) {
         t.draw += t1 - t0;
         t.flush += t2 - t1;
         t.sync += t3 - t2;
         t.present += t_present;
         t.total += (t3 - t0) + t_present;
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
