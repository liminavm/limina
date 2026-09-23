/* Begin a 2-attachment render pass the ways a guest can get wrong, then end
 * it and the command buffer.
 *
 *   rp_attach_count imageless   imageless framebuffer, VkRenderPassAttachmentBeginInfo
 *                               carrying 1 view for a 2-attachment pass
 *   rp_attach_count fb          regular framebuffer created with 1 view for a
 *                               2-attachment pass
 *   rp_attach_count nobegin     imageless framebuffer begun WITHOUT
 *                               VkRenderPassAttachmentBeginInfo (0 views)
 *   rp_attach_count nullrp      renderPass = VK_NULL_HANDLE
 *   rp_attach_count nullfb      framebuffer = VK_NULL_HANDLE
 *   rp_attach_count nullfb-il   framebuffer = VK_NULL_HANDLE, but 2 views
 *                               supplied by VkRenderPassAttachmentBeginInfo
 *   rp_attach_count nullview    imageless, 2 views supplied, the second NULL
 *   rp_attach_count nullfbview  regular framebuffer created with 2 views,
 *                               the second NULL
 *   rp_attach_count draw        the "imageless" begin, then a draw
 *   rp_attach_count drawnopass  a draw with no render pass begun at all
 *   rp_attach_count ok          valid control: 2 views, 2 attachments
 *   rp_attach_count drawok      valid control, with a draw
 *
 * The draw modes bind a pipeline built from tri.vert.spv / tri.frag.spv (in
 * the working directory) and exit 3 if it cannot be built, so a missing
 * pipeline never reads as a survived draw.
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

static VkShaderModule
load_shader(VkDevice dev, const char *path)
{
   FILE *f = fopen(path, "rb");
   if (!f) {
      fprintf(stderr, "cannot open %s\n", path);
      exit(3);
   }
   static uint32_t code[4096];
   size_t n = fread(code, 1, sizeof(code), f);
   fclose(f);
   VkShaderModuleCreateInfo ci = {
      .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
      .codeSize = n,
      .pCode = code,
   };
   VkShaderModule m;
   if (vkCreateShaderModule(dev, &ci, NULL, &m) != VK_SUCCESS) {
      fprintf(stderr, "vkCreateShaderModule(%s) failed\n", path);
      exit(3);
   }
   return m;
}

/* A full-screen triangle writing both colour attachments of subpass 0. */
static VkPipeline
make_pipeline(VkDevice dev, VkRenderPass rp)
{
   VkPipelineShaderStageCreateInfo stages[2] = {
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_VERTEX_BIT,
       .module = load_shader(dev, "tri.vert.spv"),
       .pName = "main"},
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
       .module = load_shader(dev, "tri.frag.spv"),
       .pName = "main"},
   };
   VkPipelineVertexInputStateCreateInfo vi = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
   };
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
   };
   VkViewport vp = {0, 0, 64, 64, 0, 1};
   VkRect2D sc = {{0, 0}, {64, 64}};
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .pViewports = &vp,
      .scissorCount = 1,
      .pScissors = &sc,
   };
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE,
      .lineWidth = 1.0f,
   };
   VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
   };
   VkPipelineColorBlendAttachmentState cba[2] = {
      {.colorWriteMask = 0xf},
      {.colorWriteMask = 0xf},
   };
   VkPipelineColorBlendStateCreateInfo cb = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 2,
      .pAttachments = cba,
   };
   VkPipelineLayoutCreateInfo plci = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
   };
   VkPipelineLayout layout;
   if (vkCreatePipelineLayout(dev, &plci, NULL, &layout) != VK_SUCCESS) {
      fprintf(stderr, "vkCreatePipelineLayout failed\n");
      exit(3);
   }
   VkGraphicsPipelineCreateInfo gci = {
      .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .stageCount = 2,
      .pStages = stages,
      .pVertexInputState = &vi,
      .pInputAssemblyState = &ia,
      .pViewportState = &vps,
      .pRasterizationState = &rs,
      .pMultisampleState = &ms,
      .pColorBlendState = &cb,
      .layout = layout,
      .renderPass = rp,
      .subpass = 0,
   };
   VkPipeline p;
   VkResult r = vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gci, NULL, &p);
   if (r != VK_SUCCESS) {
      fprintf(stderr, "vkCreateGraphicsPipelines failed: %d\n", r);
      exit(3);
   }
   return p;
}

int
main(int argc, char **argv)
{
   const char *mode = argc > 1 ? argv[1] : "imageless";
#define IS(m) (!strcmp(mode, m))
   int nobegin = IS("nobegin");
   int drawok = IS("drawok");
   int drawnopass = IS("drawnopass");
   int draw = IS("draw") || drawok || drawnopass;
   int nullrp = IS("nullrp");
   int nullfb = IS("nullfb") || IS("nullfb-il");
   int nullview = IS("nullview");
   int nullfbview = IS("nullfbview");
   int imageless = IS("imageless") || IS("draw") || nobegin || IS("nullfb-il") || nullview;
   int valid = !(IS("imageless") || IS("fb") || IS("draw") || nobegin);

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
   VkPipeline pipe = draw ? make_pipeline(dev, rp) : VK_NULL_HANDLE;

   /* The views the framebuffer or the begin info hands over. */
   VkImageView given[2] = {view[0], (nullview || nullfbview) ? VK_NULL_HANDLE : view[1]};

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
      .pAttachments = imageless ? NULL : given,
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
      .pAttachments = given,
   };
   VkClearValue clears[2] = {0};
   VkRenderPassBeginInfo rpbi = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
      .pNext = imageless && !nobegin ? &rabi : NULL,
      .renderPass = nullrp ? VK_NULL_HANDLE : rp,
      .framebuffer = nullfb ? VK_NULL_HANDLE : fb,
      .renderArea = {{0, 0}, {64, 64}},
      .clearValueCount = 2,
      .pClearValues = clears,
   };
   if (!drawnopass) {
      printf("mode=%s: beginning a 2-attachment render pass with %u view(s)\n",
             mode, supplied);
      fflush(stdout);
      vkCmdBeginRenderPass(cb, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
      printf("begin returned\n");
      fflush(stdout);
   } else {
      printf("mode=%s: no render pass begun\n", mode);
   }
   if (draw) {
      vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
      vkCmdDraw(cb, 3, 1, 0, 0);
      printf("draw returned\n");
      fflush(stdout);
   }
   if (!drawnopass) {
      vkCmdEndRenderPass(cb);
      printf("end render pass returned\n");
      fflush(stdout);
   }
   VkResult r = vkEndCommandBuffer(cb);
   printf("RESULT: vkEndCommandBuffer = %d\n", r);
   if (r == VK_SUCCESS) {
      /* A recorded buffer can still fault when it runs; submit what ended. */
      VkQueue q;
      vkGetDeviceQueue(dev, 0, 0, &q);
      VkSubmitInfo si = {
         .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
         .commandBufferCount = 1,
         .pCommandBuffers = &cb,
      };
      VkResult sr = vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE);
      VkResult wr = sr == VK_SUCCESS ? vkQueueWaitIdle(q) : sr;
      printf("RESULT: submit = %d, wait = %d\n", sr, wr);
   }

   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyFramebuffer(dev, fb, &scribble);
   vkDestroyRenderPass(dev, rp, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return 0;
}
