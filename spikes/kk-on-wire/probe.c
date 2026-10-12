/*
 * kk-probe: the smallest Vulkan programs that prove a driver computes and draws, checked on the
 * CPU. Written for KosmicKrisp on limina's Metal-level protocol in a Linux guest, but plain
 * Vulkan 1.0: any driver runs it.
 *
 *   compute   one dispatch writes data[i] = 3 * i + 1 into a host-visible buffer
 *   triangle  a 64x64 clear to blue plus one green triangle, copied to a buffer; the green and
 *             blue pixel counts are checked and the image is written to kk-probe.ppm
 *   allocs    6000 separately allocated, mapped 4 KiB buffers, each written and checked through
 *             its own mapping, then a dispatch into the last one
 *   share     a buffer exported as a dma-buf (VK_KHR_external_memory_fd): imported back into the
 *             same device and dispatched into, then imported by a child process (this program,
 *             exec'd as `share-child <fd>`) that checks what the parent wrote and dispatches
 *             into it; the parent checks each result through its own mapping. Not part of the
 *             default run.
 *   signal    a submit that only signals a timeline semaphore, then the counter polled with
 *             vkGetSemaphoreCounterValue, which pushes nothing: the signal must reach the host on
 *             its own. Not part of the default run.
 *
 * Exit status 0 only if every part ran and checked out.
 */
#include <fcntl.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#include "fill.comp.h"
#include "tri.frag.h"
#include "tri.vert.h"

#define CHECK(x)                                                                              \
   do {                                                                                       \
      VkResult r_ = (x);                                                                      \
      if (r_ != VK_SUCCESS) {                                                                 \
         fprintf(stderr, "%s:%d: %s = %d\n", __FILE__, __LINE__, #x, r_);                     \
         exit(2);                                                                             \
      }                                                                                       \
   } while (0)

static VkDevice dev;
static VkPhysicalDevice pdev;
static VkQueue queue;
static uint32_t qfam;
static VkCommandPool pool;

static uint32_t
memory_type(uint32_t bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties p;
   vkGetPhysicalDeviceMemoryProperties(pdev, &p);
   for (uint32_t i = 0; i < p.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (p.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type for bits 0x%x flags 0x%x\n", bits, want);
   exit(2);
}

struct buf {
   VkBuffer b;
   VkDeviceMemory m;
   void *map;
};

static struct buf
host_buffer(VkDeviceSize size, VkBufferUsageFlags usage)
{
   struct buf r;
   VkBufferCreateInfo bi = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                            .size = size,
                            .usage = usage};
   CHECK(vkCreateBuffer(dev, &bi, NULL, &r.b));
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, r.b, &req);
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = memory_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                            VK_MEMORY_PROPERTY_HOST_COHERENT_BIT),
   };
   CHECK(vkAllocateMemory(dev, &ai, NULL, &r.m));
   CHECK(vkBindBufferMemory(dev, r.b, r.m, 0));
   CHECK(vkMapMemory(dev, r.m, 0, VK_WHOLE_SIZE, 0, &r.map));
   return r;
}

static void
free_buf(struct buf b)
{
   vkUnmapMemory(dev, b.m);
   vkDestroyBuffer(dev, b.b, NULL);
   vkFreeMemory(dev, b.m, NULL);
}

static VkShaderModule
shader(const uint32_t *code, size_t size)
{
   VkShaderModuleCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                  .codeSize = size,
                                  .pCode = code};
   VkShaderModule m;
   CHECK(vkCreateShaderModule(dev, &ci, NULL, &m));
   return m;
}

static VkCommandBuffer
begin(void)
{
   VkCommandBufferAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                     .commandPool = pool,
                                     .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                     .commandBufferCount = 1};
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &ai, &cb));
   VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                  .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
   CHECK(vkBeginCommandBuffer(cb, &bi));
   return cb;
}

static void
submit_and_wait(VkCommandBuffer cb)
{
   CHECK(vkEndCommandBuffer(cb));
   VkFenceCreateInfo fi = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
   VkFence f;
   CHECK(vkCreateFence(dev, &fi, NULL, &f));
   VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                      .commandBufferCount = 1,
                      .pCommandBuffers = &cb};
   CHECK(vkQueueSubmit(queue, 1, &si, f));
   CHECK(vkWaitForFences(dev, 1, &f, VK_TRUE, 10ull * 1000 * 1000 * 1000));
   vkDestroyFence(dev, f, NULL);
   vkFreeCommandBuffers(dev, pool, 1, &cb);
}

static int
compute_into(struct buf out)
{
   const uint32_t n = 256;
   memset(out.map, 0xff, n * 4);

   VkDescriptorSetLayoutBinding b = {.binding = 0,
                                     .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                                     .descriptorCount = 1,
                                     .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT};
   VkDescriptorSetLayoutCreateInfo li = {.sType =
                                            VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                                         .bindingCount = 1,
                                         .pBindings = &b};
   VkDescriptorSetLayout dsl;
   CHECK(vkCreateDescriptorSetLayout(dev, &li, NULL, &dsl));
   VkPipelineLayoutCreateInfo pli = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                     .setLayoutCount = 1,
                                     .pSetLayouts = &dsl};
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &pli, NULL, &pl));
   VkComputePipelineCreateInfo ci = {
      .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
      .stage = {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                .module = shader(fill_comp, sizeof(fill_comp)),
                .pName = "main"},
      .layout = pl,
   };
   VkPipeline p;
   CHECK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &ci, NULL, &p));

   VkDescriptorPoolSize ps = {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1};
   VkDescriptorPoolCreateInfo dpi = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
                                     .maxSets = 1,
                                     .poolSizeCount = 1,
                                     .pPoolSizes = &ps};
   VkDescriptorPool dp;
   CHECK(vkCreateDescriptorPool(dev, &dpi, NULL, &dp));
   VkDescriptorSetAllocateInfo dai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
                                      .descriptorPool = dp,
                                      .descriptorSetCount = 1,
                                      .pSetLayouts = &dsl};
   VkDescriptorSet ds;
   CHECK(vkAllocateDescriptorSets(dev, &dai, &ds));
   VkDescriptorBufferInfo dbi = {out.b, 0, VK_WHOLE_SIZE};
   VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                             .dstSet = ds,
                             .descriptorCount = 1,
                             .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                             .pBufferInfo = &dbi};
   vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);

   VkCommandBuffer cb = begin();
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, p);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
   vkCmdDispatch(cb, n / 64, 1, 1);
   VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                         .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
                         .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0,
                        1, &mb, 0, NULL, 0, NULL);
   submit_and_wait(cb);

   const uint32_t *d = out.map;
   uint32_t good = 0;
   for (uint32_t i = 0; i < n; i++)
      good += d[i] == 3 * i + 1;
   printf("compute: %u/%u words right (data[0]=%u data[255]=%u)\n", good, n, d[0], d[n - 1]);
   vkDestroyPipeline(dev, p, NULL);
   vkDestroyDescriptorPool(dev, dp, NULL);
   vkDestroyPipelineLayout(dev, pl, NULL);
   vkDestroyDescriptorSetLayout(dev, dsl, NULL);
   return good == n ? 0 : 1;
}

static int
compute(void)
{
   struct buf out = host_buffer(256 * 4, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT);
   int fails = compute_into(out);
   free_buf(out);
   return fails;
}

/* More separately allocated, mapped buffers than a guest context has arenas, each written
 * through its own mapping and checked, then a dispatch into the last one. */
static int
allocs(void)
{
   enum { N = 6000 };
   static struct buf b[N];
   for (uint32_t i = 0; i < N; i++) {
      b[i] = host_buffer(4096, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT);
      memset(b[i].map, 0, 4096);
      ((uint32_t *)b[i].map)[i % 1024] = i;
   }
   uint32_t good = 0;
   for (uint32_t i = 0; i < N; i++) {
      const uint32_t *w = b[i].map;
      uint32_t nonzero = 0;
      for (uint32_t j = 0; j < 1024; j++)
         nonzero += w[j] != 0;
      good += w[i % 1024] == i && nonzero == (i ? 1u : 0u);
   }
   printf("allocs: %u/%u buffers kept their own contents\n", good, N);
   int fails = good == N ? 0 : 1;
   fails += compute_into(b[N - 1]);
   for (uint32_t i = 0; i < N; i++)
      free_buf(b[i]);
   return fails;
}

static int
triangle(void)
{
   const uint32_t W = 64, H = 64;
   const VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;

   VkImageCreateInfo ii = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = fmt,
      .extent = {W, H, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
   };
   VkImage img;
   CHECK(vkCreateImage(dev, &ii, NULL, &img));
   VkMemoryRequirements req;
   vkGetImageMemoryRequirements(dev, img, &req);
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = memory_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
   };
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &ai, NULL, &mem));
   CHECK(vkBindImageMemory(dev, img, mem, 0));
   VkImageViewCreateInfo vi = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = fmt,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
   };
   VkImageView view;
   CHECK(vkCreateImageView(dev, &vi, NULL, &view));

   VkAttachmentDescription att = {
      .format = fmt,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
      .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
   };
   VkAttachmentReference ref = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
   VkSubpassDescription sub = {.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
                               .colorAttachmentCount = 1,
                               .pColorAttachments = &ref};
   VkSubpassDependency dep = {
      .srcSubpass = 0,
      .dstSubpass = VK_SUBPASS_EXTERNAL,
      .srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
      .dstStageMask = VK_PIPELINE_STAGE_TRANSFER_BIT,
      .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
   };
   VkRenderPassCreateInfo rpi = {.sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
                                 .attachmentCount = 1,
                                 .pAttachments = &att,
                                 .subpassCount = 1,
                                 .pSubpasses = &sub,
                                 .dependencyCount = 1,
                                 .pDependencies = &dep};
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &rpi, NULL, &rp));
   VkFramebufferCreateInfo fbi = {.sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                                  .renderPass = rp,
                                  .attachmentCount = 1,
                                  .pAttachments = &view,
                                  .width = W,
                                  .height = H,
                                  .layers = 1};
   VkFramebuffer fb;
   CHECK(vkCreateFramebuffer(dev, &fbi, NULL, &fb));

   VkPipelineLayoutCreateInfo pli = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &pli, NULL, &pl));
   VkPipelineShaderStageCreateInfo stages[2] = {
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_VERTEX_BIT,
       .module = shader(tri_vert, sizeof(tri_vert)),
       .pName = "main"},
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
       .module = shader(tri_frag, sizeof(tri_frag)),
       .pName = "main"},
   };
   VkPipelineVertexInputStateCreateInfo vin = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
   VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
   VkViewport vp = {0, 0, (float)W, (float)H, 0, 1};
   VkRect2D sc = {{0, 0}, {W, H}};
   VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .pViewports = &vp,
      .scissorCount = 1,
      .pScissors = &sc};
   VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE,
      .lineWidth = 1};
   VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
   VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
   VkPipelineColorBlendStateCreateInfo cb_state = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1,
      .pAttachments = &cba};
   VkGraphicsPipelineCreateInfo gpi = {
      .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .stageCount = 2,
      .pStages = stages,
      .pVertexInputState = &vin,
      .pInputAssemblyState = &ia,
      .pViewportState = &vps,
      .pRasterizationState = &rs,
      .pMultisampleState = &ms,
      .pColorBlendState = &cb_state,
      .layout = pl,
      .renderPass = rp,
   };
   VkPipeline p;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpi, NULL, &p));

   struct buf out = host_buffer(W * H * 4, VK_BUFFER_USAGE_TRANSFER_DST_BIT);
   memset(out.map, 0, W * H * 4);

   VkCommandBuffer cb = begin();
   VkClearValue clear = {.color = {.float32 = {0, 0, 1, 1}}};
   VkRenderPassBeginInfo rbi = {.sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                                .renderPass = rp,
                                .framebuffer = fb,
                                .renderArea = sc,
                                .clearValueCount = 1,
                                .pClearValues = &clear};
   vkCmdBeginRenderPass(cb, &rbi, VK_SUBPASS_CONTENTS_INLINE);
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, p);
   vkCmdDraw(cb, 3, 1, 0, 0);
   vkCmdEndRenderPass(cb);
   VkBufferImageCopy copy = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
                             .imageExtent = {W, H, 1}};
   vkCmdCopyImageToBuffer(cb, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, out.b, 1, &copy);
   VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                         .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                         .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1,
                        &mb, 0, NULL, 0, NULL);
   submit_and_wait(cb);

   const uint8_t *px = out.map;
   uint32_t green = 0, blue = 0, other = 0;
   for (uint32_t i = 0; i < W * H; i++) {
      const uint8_t *c = px + 4 * i;
      if (c[0] == 0 && c[1] == 255 && c[2] == 0)
         green++;
      else if (c[0] == 0 && c[1] == 0 && c[2] == 255)
         blue++;
      else
         other++;
   }
   FILE *f = fopen("kk-probe.ppm", "wb");
   if (f) {
      fprintf(f, "P6\n%u %u\n255\n", W, H);
      for (uint32_t i = 0; i < W * H; i++)
         fwrite(px + 4 * i, 1, 3, f);
      fclose(f);
   }
   /* The triangle covers half the image: (-1,-1), (1,-1), (-1,1) in NDC. */
   printf("triangle: green %u blue %u other %u (green expected ~%u)\n", green, blue, other,
          W * H / 2);
   free_buf(out);
   vkDestroyPipeline(dev, p, NULL);
   vkDestroyPipelineLayout(dev, pl, NULL);
   vkDestroyFramebuffer(dev, fb, NULL);
   vkDestroyRenderPass(dev, rp, NULL);
   vkDestroyImageView(dev, view, NULL);
   vkDestroyImage(dev, img, NULL);
   vkFreeMemory(dev, mem, NULL);
   return green > W * H * 2 / 5 && blue > W * H * 2 / 5 && green + blue == W * H ? 0 : 1;
}

/* -- share -- */

#define SHARE_SIZE (64 * 1024)
#define SHARE_MARK 0x5eed1234u
#define SHARE_MARK_WORD 300

static PFN_vkGetMemoryFdKHR get_memory_fd;
static PFN_vkGetMemoryFdPropertiesKHR get_memory_fd_properties;

static VkBuffer
external_buffer(void)
{
   VkExternalMemoryBufferCreateInfo ext = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
   VkBufferCreateInfo bi = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                            .pNext = &ext,
                            .size = SHARE_SIZE,
                            .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT};
   VkBuffer b;
   CHECK(vkCreateBuffer(dev, &bi, NULL, &b));
   return b;
}

static struct buf
exported_buffer(void)
{
   struct buf r = {.b = external_buffer()};
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, r.b, &req);
   VkExportMemoryAllocateInfo ex = {.sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
                                    .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &ex,
      .allocationSize = req.size,
      .memoryTypeIndex = memory_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                            VK_MEMORY_PROPERTY_HOST_COHERENT_BIT),
   };
   CHECK(vkAllocateMemory(dev, &ai, NULL, &r.m));
   CHECK(vkBindBufferMemory(dev, r.b, r.m, 0));
   CHECK(vkMapMemory(dev, r.m, 0, VK_WHOLE_SIZE, 0, &r.map));
   return r;
}

static int
export_fd(VkDeviceMemory m)
{
   VkMemoryGetFdInfoKHR gi = {.sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
                              .memory = m,
                              .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
   int fd = -1;
   CHECK(get_memory_fd(dev, &gi, &fd));
   return fd;
}

/* Takes `fd`. */
static struct buf
imported_buffer(int fd)
{
   struct buf r = {.b = external_buffer()};
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, r.b, &req);
   VkMemoryFdPropertiesKHR fp = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
   CHECK(get_memory_fd_properties(dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fp));
   VkImportMemoryFdInfoKHR im = {.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                                 .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                 .fd = fd};
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &im,
      .allocationSize = req.size,
      .memoryTypeIndex =
         memory_type(req.memoryTypeBits & fp.memoryTypeBits,
                     VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT),
   };
   CHECK(vkAllocateMemory(dev, &ai, NULL, &r.m));
   CHECK(vkBindBufferMemory(dev, r.b, r.m, 0));
   CHECK(vkMapMemory(dev, r.m, 0, VK_WHOLE_SIZE, 0, &r.map));
   return r;
}

/* Whether the parent's view shows what compute_into wrote through another one. */
static int
check_filled(const char *who, const struct buf *b)
{
   const uint32_t *d = b->map;
   uint32_t good = 0;
   for (uint32_t i = 0; i < 256; i++)
      good += d[i] == 3 * i + 1;
   printf("share: %s: the exporter's mapping shows %u/256 words right\n", who, good);
   return good == 256 ? 0 : 1;
}

static int
share_child(int fd)
{
   struct buf b = imported_buffer(fd);
   uint32_t mark = ((uint32_t *)b.map)[SHARE_MARK_WORD];
   printf("share-child: imported; the parent's mark reads 0x%x\n", mark);
   int fails = mark != SHARE_MARK;
   fails += compute_into(b);
   free_buf(b);
   return fails;
}

static int
share(const char *self)
{
   struct buf ex = exported_buffer();
   ((uint32_t *)ex.map)[SHARE_MARK_WORD] = SHARE_MARK;

   /* Imported back into the device that exported it: the same GEM object twice. */
   struct buf again = imported_buffer(export_fd(ex.m));
   int fails = compute_into(again);
   free_buf(again);
   fails += check_filled("self-import", &ex);
   memset(ex.map, 0, 256 * 4);

   /* An fd without close-on-exec for the child, which gets its own device. */
   int fd = export_fd(ex.m);
   int inherit = fcntl(fd, F_DUPFD, 100);
   close(fd);
   pid_t pid = fork();
   if (pid == 0) {
      char arg[16];
      snprintf(arg, sizeof(arg), "%d", inherit);
      execl(self, self, "share-child", arg, (char *)NULL);
      _exit(127);
   }
   close(inherit);
   int status = 0;
   waitpid(pid, &status, 0);
   if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
      printf("share: child failed (status 0x%x)\n", status);
      fails++;
   }
   fails += check_filled("child", &ex);
   free_buf(ex);
   return fails;
}

/* -- signal -- */

static int
signal_reaches_host(void)
{
   VkSemaphoreTypeCreateInfo ti = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
                                   .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE};
   VkSemaphoreCreateInfo si = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &ti};
   VkSemaphore sem;
   CHECK(vkCreateSemaphore(dev, &si, NULL, &sem));
   uint64_t one = 1;
   VkTimelineSemaphoreSubmitInfo tsi = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
                                        .signalSemaphoreValueCount = 1,
                                        .pSignalSemaphoreValues = &one};
   VkSubmitInfo sub = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                       .pNext = &tsi,
                       .signalSemaphoreCount = 1,
                       .pSignalSemaphores = &sem};
   CHECK(vkQueueSubmit(queue, 1, &sub, VK_NULL_HANDLE));
   uint64_t v = 0;
   struct timespec start, now;
   clock_gettime(CLOCK_MONOTONIC, &start);
   double s = 0;
   while (v < 1 && s < 3.0) {
      CHECK(vkGetSemaphoreCounterValue(dev, sem, &v));
      clock_gettime(CLOCK_MONOTONIC, &now);
      s = (now.tv_sec - start.tv_sec) + (now.tv_nsec - start.tv_nsec) / 1e9;
   }
   printf("signal: the counter reads %llu after %.3f s\n", (unsigned long long)v, s);
   vkDestroySemaphore(dev, sem, NULL);
   return v >= 1 ? 0 : 1;
}

int
main(int argc, char **argv)
{
   VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                            .pApplicationName = "kk-probe",
                            .apiVersion = VK_API_VERSION_1_2};
   VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                               .pApplicationInfo = &app};
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t n = 1;
   VkResult r = vkEnumeratePhysicalDevices(inst, &n, &pdev);
   if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || n == 0) {
      fprintf(stderr, "no physical device\n");
      return 2;
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pdev, &props);
   printf("device: %s\n", props.deviceName);

   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, NULL);
   VkQueueFamilyProperties qf[8];
   if (nq > 8)
      nq = 8;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   for (qfam = 0; qfam < nq; qfam++)
      if ((qf[qfam].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) ==
          (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT))
         break;
   float prio = 1;
   VkDeviceQueueCreateInfo qi = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                 .queueFamilyIndex = qfam,
                                 .queueCount = 1,
                                 .pQueuePriorities = &prio};
   const char *only = argc > 1 ? argv[1] : NULL;
   bool sharing = only && !strncmp(only, "share", 5);
   const char *exts[] = {"VK_KHR_external_memory", "VK_KHR_external_memory_fd",
                         "VK_EXT_external_memory_dma_buf"};
   VkPhysicalDeviceVulkan12Features f12 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
      .timelineSemaphore = only && !strcmp(only, "signal")};
   VkDeviceCreateInfo di = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                            .pNext = &f12,
                            .queueCreateInfoCount = 1,
                            .pQueueCreateInfos = &qi,
                            .enabledExtensionCount = sharing ? 3 : 0,
                            .ppEnabledExtensionNames = exts};
   CHECK(vkCreateDevice(pdev, &di, NULL, &dev));
   if (sharing) {
      get_memory_fd = (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(dev, "vkGetMemoryFdKHR");
      get_memory_fd_properties = (PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(
         dev, "vkGetMemoryFdPropertiesKHR");
   }
   vkGetDeviceQueue(dev, qfam, 0, &queue);
   VkCommandPoolCreateInfo pi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                 .queueFamilyIndex = qfam};
   CHECK(vkCreateCommandPool(dev, &pi, NULL, &pool));

   int fails = 0;
   if (only && !strcmp(only, "signal"))
      fails += signal_reaches_host();
   else if (only && !strcmp(only, "share"))
      fails += share(argv[0]);
   else if (only && !strcmp(only, "share-child") && argc > 2)
      fails += share_child(atoi(argv[2]));
   if (!only || !strcmp(only, "compute"))
      fails += compute();
   if (!only || !strcmp(only, "triangle"))
      fails += triangle();
   if (!only || !strcmp(only, "allocs"))
      fails += allocs();

   vkDeviceWaitIdle(dev);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   printf("%s\n", fails ? "FAIL" : "PASS");
   return fails ? 1 : 0;
}
