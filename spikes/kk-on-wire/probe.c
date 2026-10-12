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
 *   import    a linear XRGB8888 scanout buffer another driver made (gbm on the render node, stock
 *             virgl in a limina guest) and wrote a pattern into, imported as a dedicated image
 *             with the LINEAR modifier at the producer's pitch, copied to a buffer: every pixel
 *             is checked. Not part of the default run.
 *   export    the other way: an image made for dma-buf export with the LINEAR modifier, filled
 *             with a pattern by a buffer copy, exported and sampled by the render node's GL
 *             driver (EGL on gbm: stock virgl in a limina guest) into a texture of its own, read
 *             back from there: every pixel is checked. Not part of the default run.
 *   nv12      a 4:2:0 video surface from the render node's VA driver (stock virgl), written with
 *             a Y and a CbCr pattern through vaPutImage and exported as a dma-buf
 *             (vaExportSurfaceHandle), imported as a 2-plane image at VA's plane layouts; each
 *             plane is copied to a buffer and checked. Not part of the default run.
 *   nv12-plain  the same, the surface made as ffmpeg and GStreamer make theirs (no DRM_PRIME_2
 *             memory type), so its planes are not shared until exported.
 *   vadecode  real decoded frames, as a player gets them: ffmpeg's VA hwaccel on the render node
 *             (stock virgl), each frame mapped to DRM PRIME (av_hwframe_map), imported as a
 *             2- or 3-plane image and copied out, against the same frame read back by ffmpeg
 *             (av_hwframe_transfer_data). The clip is $KK_CLIP (default ~/vab/clip.webm). Not
 *             part of the default run.
 *   timestamps the queue's timestamps as an app sees them (libplacebo times every frame): with
 *             timestampValidBits, two writes around a submit read back in order; without, a
 *             timestamp pool is refused or made, never an abort.
 *   syncfd    a submit held by a wait on a timeline value nobody has signaled, signaling a
 *             binary semaphore: its exported sync_file (VK_KHR_external_semaphore_fd) is a real
 *             fence, pending until the CPU signals the timeline and signaled after. Not part of
 *             the default run.
 *
 * Exit status 0 only if every part ran and checked out.
 */
#include <drm_fourcc.h>
#include <fcntl.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <xf86drm.h>
#include <virtgpu_drm.h>
#include <linux/dma-buf.h>
#include <poll.h>
#include <sys/ioctl.h>
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

/* -- import -- */

static bool
fd_ready(int fd, int ms)
{
   struct pollfd p = {.fd = fd, .events = POLLIN};
   return poll(&p, 1, ms) == 1;
}

#define IMPORT_W 64
#define IMPORT_H 16

static uint32_t
import_pixel(uint32_t x, uint32_t y)
{
   return 0xff000000u | (x * 4) << 16 | (y * 16) << 8 | 0x5a;
}

static int
import_foreign(void)
{
   int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
   struct gbm_device *gbm = drm >= 0 ? gbm_create_device(drm) : NULL;
   struct gbm_bo *bo =
      gbm ? gbm_bo_create(gbm, IMPORT_W, IMPORT_H, GBM_FORMAT_XRGB8888,
                          GBM_BO_USE_SCANOUT | GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING)
          : NULL;
   if (!bo) {
      fprintf(stderr, "import: gbm made no buffer\n");
      return 1;
   }
   printf("import: gbm backend %s\n", gbm_device_get_backend_name(gbm));
   uint32_t map_stride = 0;
   void *map_data = NULL;
   uint8_t *px = gbm_bo_map(bo, 0, 0, IMPORT_W, IMPORT_H, GBM_BO_TRANSFER_WRITE, &map_stride,
                            &map_data);
   if (!px) {
      fprintf(stderr, "import: gbm would not map the buffer\n");
      return 1;
   }
   for (uint32_t y = 0; y < IMPORT_H; y++)
      for (uint32_t x = 0; x < IMPORT_W; x++)
         ((uint32_t *)(px + y * map_stride))[x] = import_pixel(x, y);
   gbm_bo_unmap(bo, map_data);
   int fd = gbm_bo_get_fd(bo);
   uint32_t stride = gbm_bo_get_stride(bo);
   /* The producer's writes are fenced on the buffer: wait for them, as an importer must. */
   struct dma_buf_export_sync_file ex = {.flags = DMA_BUF_SYNC_READ, .fd = -1};
   if (ioctl(fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &ex) == 0) {
      fd_ready(ex.fd, 3000);
      close(ex.fd);
   }

   VkSubresourceLayout plane = {.rowPitch = stride};
   VkImageDrmFormatModifierExplicitCreateInfoEXT mod = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
      .drmFormatModifier = DRM_FORMAT_MOD_LINEAR,
      .drmFormatModifierPlaneCount = 1,
      .pPlaneLayouts = &plane};
   VkExternalMemoryImageCreateInfo ext = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .pNext = &mod,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
   VkImageCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                            .pNext = &ext,
                            .imageType = VK_IMAGE_TYPE_2D,
                            .format = VK_FORMAT_B8G8R8A8_UNORM,
                            .extent = {IMPORT_W, IMPORT_H, 1},
                            .mipLevels = 1,
                            .arrayLayers = 1,
                            .samples = VK_SAMPLE_COUNT_1_BIT,
                            .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
                            .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
                                     VK_IMAGE_USAGE_SAMPLED_BIT};
   VkImage img;
   CHECK(vkCreateImage(dev, &ici, NULL, &img));
   VkMemoryRequirements req;
   vkGetImageMemoryRequirements(dev, img, &req);
   VkMemoryFdPropertiesKHR fp = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
   CHECK(get_memory_fd_properties(dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fp));
   VkMemoryDedicatedAllocateInfo ded = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                                        .image = img};
   VkImportMemoryFdInfoKHR im = {.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                                 .pNext = &ded,
                                 .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                 .fd = fd};
   VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                              .pNext = &im,
                              .allocationSize = req.size,
                              .memoryTypeIndex =
                                 memory_type(req.memoryTypeBits & fp.memoryTypeBits, 0)};
   VkDeviceMemory mem;
   VkResult r = vkAllocateMemory(dev, &ai, NULL, &mem);
   if (r != VK_SUCCESS) {
      fprintf(stderr, "import: the %ux%u buffer (pitch %u) was not imported: %d\n", IMPORT_W,
              IMPORT_H, stride, r);
      close(fd);
      return 1;
   }
   CHECK(vkBindImageMemory(dev, img, mem, 0));

   struct buf out = host_buffer(IMPORT_W * IMPORT_H * 4, VK_BUFFER_USAGE_TRANSFER_DST_BIT);
   memset(out.map, 0, IMPORT_W * IMPORT_H * 4);
   VkCommandBuffer cb = begin();
   VkImageMemoryBarrier acquire = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                                   .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                                   .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
                                   .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                                   .srcQueueFamilyIndex = VK_QUEUE_FAMILY_EXTERNAL,
                                   .dstQueueFamilyIndex = qfam,
                                   .image = img,
                                   .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0,
                        0, NULL, 0, NULL, 1, &acquire);
   VkBufferImageCopy copy = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
                             .imageExtent = {IMPORT_W, IMPORT_H, 1}};
   vkCmdCopyImageToBuffer(cb, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, out.b, 1, &copy);
   submit_and_wait(cb);

   uint32_t good = 0;
   for (uint32_t y = 0; y < IMPORT_H; y++)
      for (uint32_t x = 0; x < IMPORT_W; x++)
         good += ((uint32_t *)out.map)[y * IMPORT_W + x] == import_pixel(x, y);
   printf("import: %ux%u at pitch %u: %u/%u pixels as the producer wrote them (first 0x%08x)\n",
          IMPORT_W, IMPORT_H, stride, good, IMPORT_W * IMPORT_H, ((uint32_t *)out.map)[0]);
   free_buf(out);
   vkDestroyImage(dev, img, NULL);
   vkFreeMemory(dev, mem, NULL);
   gbm_bo_destroy(bo);
   gbm_device_destroy(gbm);
   close(drm);
   return good == IMPORT_W * IMPORT_H ? 0 : 1;
}

/* Samples dma-buf `fd` (IMPORT_W x IMPORT_H ARGB8888, rows `pitch` apart) texel for texel into a
 * texture of the render node's own GL driver, as a compositor draws a client buffer, and counts
 * the pixels read back from that texture that match the pattern. */
static uint32_t
sample_foreign(int fd, uint32_t pitch)
{
   int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
   struct gbm_device *gbm = drm >= 0 ? gbm_create_device(drm) : NULL;
   PFNEGLGETPLATFORMDISPLAYEXTPROC get_display =
      (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
   EGLDisplay dpy = gbm && get_display ? get_display(EGL_PLATFORM_GBM_KHR, gbm, NULL)
                                       : EGL_NO_DISPLAY;
   if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL) ||
       !eglBindAPI(EGL_OPENGL_ES_API)) {
      fprintf(stderr, "export: no EGL on the render node\n");
      return 0;
   }
   const EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
   EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
   if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
      fprintf(stderr, "export: no surfaceless GLES context\n");
      return 0;
   }
   printf("export: consumer GL: %s\n", (const char *)glGetString(GL_RENDERER));
   PFNEGLCREATEIMAGEKHRPROC create_image =
      (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
   PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target =
      (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
   const EGLint iattr[] = {EGL_WIDTH, IMPORT_W,
                           EGL_HEIGHT, IMPORT_H,
                           EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_ARGB8888,
                           EGL_DMA_BUF_PLANE0_FD_EXT, fd,
                           EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
                           EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)pitch,
                           EGL_NONE};
   EGLImageKHR image = create_image(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, iattr);
   if (image == EGL_NO_IMAGE_KHR) {
      fprintf(stderr, "export: the GL driver did not import the buffer (0x%x)\n", eglGetError());
      return 0;
   }
   GLuint src, dst, fb;
   glGenTextures(1, &src);
   glBindTexture(GL_TEXTURE_2D, src);
   image_target(GL_TEXTURE_2D, image);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
   glGenTextures(1, &dst);
   glBindTexture(GL_TEXTURE_2D, dst);
   glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, IMPORT_W, IMPORT_H, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
   glGenFramebuffers(1, &fb);
   glBindFramebuffer(GL_FRAMEBUFFER, fb);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, dst, 0);

   static const char *vs = "attribute vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }";
   static const char *fs = "precision highp float; uniform sampler2D t; uniform vec2 size;\n"
                           "void main() { gl_FragColor = texture2D(t, gl_FragCoord.xy / size); }";
   GLuint prog = glCreateProgram();
   const char *srcs[2] = {vs, fs};
   GLenum kinds[2] = {GL_VERTEX_SHADER, GL_FRAGMENT_SHADER};
   for (int i = 0; i < 2; i++) {
      GLuint sh = glCreateShader(kinds[i]);
      glShaderSource(sh, 1, &srcs[i], NULL);
      glCompileShader(sh);
      glAttachShader(prog, sh);
   }
   glBindAttribLocation(prog, 0, "p");
   glLinkProgram(prog);
   glUseProgram(prog);
   glActiveTexture(GL_TEXTURE0);
   glBindTexture(GL_TEXTURE_2D, src);
   glUniform1i(glGetUniformLocation(prog, "t"), 0);
   glUniform2f(glGetUniformLocation(prog, "size"), IMPORT_W, IMPORT_H);
   static const float quad[] = {-1, -1, 1, -1, -1, 1, 1, 1};
   glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, quad);
   glEnableVertexAttribArray(0);
   glViewport(0, 0, IMPORT_W, IMPORT_H);
   glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

   static uint8_t rgba[IMPORT_W * IMPORT_H * 4];
   glReadPixels(0, 0, IMPORT_W, IMPORT_H, GL_RGBA, GL_UNSIGNED_BYTE, rgba);
   uint32_t good = 0;
   for (uint32_t y = 0; y < IMPORT_H; y++)
      for (uint32_t x = 0; x < IMPORT_W; x++) {
         const uint8_t *q = &rgba[(y * IMPORT_W + x) * 4];
         uint32_t want = import_pixel(x, y);
         good += q[0] == ((want >> 16) & 0xff) && q[1] == ((want >> 8) & 0xff) &&
                 q[2] == (want & 0xff) && q[3] == 0xff;
      }
   if (good != IMPORT_W * IMPORT_H)
      fprintf(stderr, "export: first texel read back %02x %02x %02x %02x\n", rgba[0], rgba[1],
              rgba[2], rgba[3]);
   eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
   eglDestroyContext(dpy, ctx);
   eglTerminate(dpy);
   gbm_device_destroy(gbm);
   close(drm);
   return good;
}

static int
export_to_foreign(void)
{
   PFN_vkGetImageDrmFormatModifierPropertiesEXT get_modifier =
      (PFN_vkGetImageDrmFormatModifierPropertiesEXT)vkGetDeviceProcAddr(
         dev, "vkGetImageDrmFormatModifierPropertiesEXT");
   uint64_t linear = DRM_FORMAT_MOD_LINEAR;
   VkImageDrmFormatModifierListCreateInfoEXT mods = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
      .drmFormatModifierCount = 1,
      .pDrmFormatModifiers = &linear};
   VkExternalMemoryImageCreateInfo ext = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .pNext = &mods,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
   VkImageCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                            .pNext = &ext,
                            .imageType = VK_IMAGE_TYPE_2D,
                            .format = VK_FORMAT_B8G8R8A8_UNORM,
                            .extent = {IMPORT_W, IMPORT_H, 1},
                            .mipLevels = 1,
                            .arrayLayers = 1,
                            .samples = VK_SAMPLE_COUNT_1_BIT,
                            .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
                            .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                                     VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT};
   VkImage img;
   CHECK(vkCreateImage(dev, &ici, NULL, &img));
   VkMemoryDedicatedRequirements dreq = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS};
   VkMemoryRequirements2 req = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2, .pNext = &dreq};
   VkImageMemoryRequirementsInfo2 rinfo = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2, .image = img};
   vkGetImageMemoryRequirements2(dev, &rinfo, &req);
   VkMemoryDedicatedAllocateInfo ded = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                                        .image = img};
   VkExportMemoryAllocateInfo ex = {.sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
                                    .pNext = &ded,
                                    .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &ex,
      .allocationSize = req.memoryRequirements.size,
      .memoryTypeIndex = memory_type(req.memoryRequirements.memoryTypeBits, 0)};
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &ai, NULL, &mem));
   CHECK(vkBindImageMemory(dev, img, mem, 0));
   VkImageDrmFormatModifierPropertiesEXT mp = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT};
   CHECK(get_modifier(dev, img, &mp));
   VkImageSubresource sub = {.aspectMask = VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT};
   VkSubresourceLayout lay;
   vkGetImageSubresourceLayout(dev, img, &sub, &lay);

   struct buf src = host_buffer(IMPORT_W * IMPORT_H * 4, VK_BUFFER_USAGE_TRANSFER_SRC_BIT);
   for (uint32_t y = 0; y < IMPORT_H; y++)
      for (uint32_t x = 0; x < IMPORT_W; x++)
         ((uint32_t *)src.map)[y * IMPORT_W + x] = import_pixel(x, y);
   VkCommandBuffer cb = begin();
   VkImageMemoryBarrier to_dst = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                                  .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                                  .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                  .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                                  .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                  .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                  .image = img,
                                  .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0,
                        0, NULL, 0, NULL, 1, &to_dst);
   VkBufferImageCopy copy = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
                             .imageExtent = {IMPORT_W, IMPORT_H, 1}};
   vkCmdCopyBufferToImage(cb, src.b, img, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &copy);
   VkImageMemoryBarrier release = to_dst;
   release.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
   release.dstAccessMask = 0;
   release.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
   release.newLayout = VK_IMAGE_LAYOUT_GENERAL;
   release.srcQueueFamilyIndex = qfam;
   release.dstQueueFamilyIndex = VK_QUEUE_FAMILY_EXTERNAL;
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                        0, 0, NULL, 0, NULL, 1, &release);
   submit_and_wait(cb);
   free_buf(src);
   int fd = export_fd(mem);

   uint32_t good = sample_foreign(fd, (uint32_t)lay.rowPitch);
   close(fd);
   printf("export: %ux%u at pitch %llu, sampled by the render node's GL: %u/%u pixels as KK "
          "wrote them\n",
          IMPORT_W, IMPORT_H, (unsigned long long)lay.rowPitch, good, IMPORT_W * IMPORT_H);
   vkDestroyImage(dev, img, NULL);
   vkFreeMemory(dev, mem, NULL);
   return good == IMPORT_W * IMPORT_H ? 0 : 1;
}

/* -- nv12 -- */

#define NV12_W 100
#define NV12_H 16

static uint8_t
nv12_y(uint32_t x, uint32_t y)
{
   return (uint8_t)(x * 3 + y * 7);
}

static uint8_t
nv12_c(uint32_t x, uint32_t y, uint32_t c)
{
   return c ? (uint8_t)(0xc0 - x - y) : (uint8_t)(0x40 + x + y);
}

/* Imports a 4:2:0 picture of `nplanes` planes -- 2: NV12, luma and a CbCr pair; 3: I420, luma, Cb
 * and Cr -- plane p at offsets[p], rows pitches[p] apart, in dma-buf fds[p], as a LINEAR image
 * (disjoint when the planes are in `nobj` > 1 dma-bufs, one each), and copies each plane out
 * tight into out[p]: w*h luma, then ceil(w/2)*ceil(h/2) samples of 2 (NV12) or 1 (I420) bytes.
 * Nonzero when any step fails; the fds stay the caller's. */
static int
yuv_import(uint32_t w, uint32_t h, uint32_t nplanes, uint32_t nobj, const int *fds,
           const uint32_t *offsets, const uint32_t *pitches, uint8_t **out)
{
   const uint32_t cw = (w + 1) / 2, ch = (h + 1) / 2;
   const uint32_t sample = nplanes == 2 ? 2 : 1;
   const bool disjoint = nobj > 1;
   if (nplanes < 2 || nplanes > 3 || (disjoint && nobj != nplanes)) {
      fprintf(stderr, "yuv: %u planes in %u dma-bufs is no layout this imports\n", nplanes, nobj);
      return 1;
   }
   VkSubresourceLayout layouts[3];
   for (uint32_t p = 0; p < nplanes; p++)
      layouts[p] = (VkSubresourceLayout){.offset = offsets[p], .rowPitch = pitches[p]};
   VkImageDrmFormatModifierExplicitCreateInfoEXT mod = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
      .drmFormatModifier = DRM_FORMAT_MOD_LINEAR,
      .drmFormatModifierPlaneCount = nplanes,
      .pPlaneLayouts = layouts};
   VkExternalMemoryImageCreateInfo ext = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .pNext = &mod,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
   VkImageCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                            .pNext = &ext,
                            .flags = disjoint ? VK_IMAGE_CREATE_DISJOINT_BIT : 0,
                            .imageType = VK_IMAGE_TYPE_2D,
                            .format = nplanes == 2 ? VK_FORMAT_G8_B8R8_2PLANE_420_UNORM
                                                   : VK_FORMAT_G8_B8_R8_3PLANE_420_UNORM,
                            .extent = {w, h, 1},
                            .mipLevels = 1,
                            .arrayLayers = 1,
                            .samples = VK_SAMPLE_COUNT_1_BIT,
                            .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
                            .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
                                     VK_IMAGE_USAGE_SAMPLED_BIT};
   VkImage img;
   VkResult r = vkCreateImage(dev, &ici, NULL, &img);
   if (r != VK_SUCCESS) {
      fprintf(stderr, "yuv: no %u-plane image at the producer's layout: %d\n", nplanes, r);
      return 1;
   }
   static const VkImageAspectFlagBits aspect[3] = {
      VK_IMAGE_ASPECT_PLANE_0_BIT, VK_IMAGE_ASPECT_PLANE_1_BIT, VK_IMAGE_ASPECT_PLANE_2_BIT};
   VkDeviceMemory mem[3] = {VK_NULL_HANDLE, VK_NULL_HANDLE, VK_NULL_HANDLE};
   VkBindImagePlaneMemoryInfo plane_bind[3];
   VkBindImageMemoryInfo binds[3];
   for (uint32_t o = 0; o < nobj; o++) {
      VkImagePlaneMemoryRequirementsInfo preq = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_PLANE_MEMORY_REQUIREMENTS_INFO,
         .planeAspect = aspect[o]};
      VkImageMemoryRequirementsInfo2 rinfo = {
         .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,
         .pNext = disjoint ? &preq : NULL,
         .image = img};
      VkMemoryRequirements2 req = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2};
      vkGetImageMemoryRequirements2(dev, &rinfo, &req);
      int fd = dup(fds[o]);
      VkMemoryFdPropertiesKHR fp = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
      CHECK(get_memory_fd_properties(dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd,
                                     &fp));
      VkMemoryDedicatedAllocateInfo ded = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .image = img};
      VkImportMemoryFdInfoKHR imp = {.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                                     .pNext = &ded,
                                     .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                     .fd = fd};
      VkMemoryAllocateInfo ai = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
         .pNext = &imp,
         .allocationSize = req.memoryRequirements.size,
         .memoryTypeIndex =
            memory_type(req.memoryRequirements.memoryTypeBits & fp.memoryTypeBits, 0)};
      r = vkAllocateMemory(dev, &ai, NULL, &mem[o]);
      if (r != VK_SUCCESS) {
         fprintf(stderr, "yuv: dma-buf %u was not imported: %d\n", o, r);
         close(fd);
         return 1;
      }
      /* A type that says host-visible promises the memory can be mapped, and players take the
       * promise (libplacebo maps every host-visible import). Mapping a plane whose pixels are a
       * host surface could show nothing true, so the import must not say it. */
      VkPhysicalDeviceMemoryProperties mp;
      vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
      if (mp.memoryTypes[ai.memoryTypeIndex].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) {
         void *map = NULL;
         r = vkMapMemory(dev, mem[o], 0, VK_WHOLE_SIZE, 0, &map);
         fprintf(stderr, "yuv: dma-buf %u was imported host-visible (mapping it: %d)\n", o, r);
         if (r == VK_SUCCESS)
            vkUnmapMemory(dev, mem[o]);
         return 1;
      }
      plane_bind[o] = (VkBindImagePlaneMemoryInfo){
         .sType = VK_STRUCTURE_TYPE_BIND_IMAGE_PLANE_MEMORY_INFO, .planeAspect = aspect[o]};
      binds[o] = (VkBindImageMemoryInfo){.sType = VK_STRUCTURE_TYPE_BIND_IMAGE_MEMORY_INFO,
                                         .pNext = disjoint ? &plane_bind[o] : NULL,
                                         .image = img,
                                         .memory = mem[o]};
   }
   r = vkBindImageMemory2(dev, nobj, binds);
   if (r != VK_SUCCESS) {
      fprintf(stderr, "yuv: the planes were not bound: %d\n", r);
      return 1;
   }

   struct buf bufs[3];
   uint32_t bytes[3];
   VkCommandBuffer cb = begin();
   VkImageMemoryBarrier acquire = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                                   .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                                   .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
                                   .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                                   .srcQueueFamilyIndex = VK_QUEUE_FAMILY_EXTERNAL,
                                   .dstQueueFamilyIndex = qfam,
                                   .image = img,
                                   .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
   vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0,
                        0, NULL, 0, NULL, 1, &acquire);
   for (uint32_t p = 0; p < nplanes; p++) {
      bytes[p] = p ? cw * ch * sample : w * h;
      bufs[p] = host_buffer(bytes[p], VK_BUFFER_USAGE_TRANSFER_DST_BIT);
      memset(bufs[p].map, 0, bytes[p]);
      VkBufferImageCopy c = {.imageSubresource = {aspect[p], 0, 0, 1},
                             .imageExtent = {p ? cw : w, p ? ch : h, 1}};
      vkCmdCopyImageToBuffer(cb, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, bufs[p].b, 1, &c);
   }
   submit_and_wait(cb);
   for (uint32_t p = 0; p < nplanes; p++) {
      memcpy(out[p], bufs[p].map, bytes[p]);
      free_buf(bufs[p]);
   }
   vkDestroyImage(dev, img, NULL);
   for (uint32_t o = 0; o < nobj; o++)
      vkFreeMemory(dev, mem[o], NULL);
   return 0;
}

static int
nv12_from_va(bool exportable)
{
   int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
   VADisplay va = vaGetDisplayDRM(drm);
   int major, minor;
   if (!va || vaInitialize(va, &major, &minor) != VA_STATUS_SUCCESS) {
      fprintf(stderr, "nv12: no VA display\n");
      return 1;
   }
   VASurfaceID surf;
   /* With the DRM_PRIME_2 memory type the planes are made shared, for export; without it, as
    * ffmpeg and GStreamer make theirs, they are plain textures until something exports them. */
   VASurfaceAttrib attr = {.type = VASurfaceAttribMemoryType,
                           .flags = VA_SURFACE_ATTRIB_SETTABLE,
                           .value = {.type = VAGenericValueTypeInteger,
                                     .value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2}};
   if (vaCreateSurfaces(va, VA_RT_FORMAT_YUV420, NV12_W, NV12_H, &surf, 1, &attr,
                        exportable ? 1 : 0) !=
       VA_STATUS_SUCCESS) {
      fprintf(stderr, "nv12: VA made no surface\n");
      return 1;
   }
   /* Written through an image put onto the surface: virgl's VA driver derives none. */
   VAImageFormat nv12_fmt = {.fourcc = VA_FOURCC_NV12, .byte_order = VA_LSB_FIRST,
                             .bits_per_pixel = 12};
   VAImage im;
   uint8_t *p = NULL;
   if (vaCreateImage(va, &nv12_fmt, NV12_W, NV12_H, &im) != VA_STATUS_SUCCESS ||
       vaMapBuffer(va, im.buf, (void **)&p) != VA_STATUS_SUCCESS) {
      fprintf(stderr, "nv12: VA made no image to write\n");
      return 1;
   }
   for (uint32_t y = 0; y < NV12_H; y++)
      for (uint32_t x = 0; x < NV12_W; x++)
         p[im.offsets[0] + y * im.pitches[0] + x] = nv12_y(x, y);
   for (uint32_t y = 0; y < NV12_H / 2; y++)
      for (uint32_t x = 0; x < NV12_W / 2; x++)
         for (uint32_t c = 0; c < 2; c++)
            p[im.offsets[1] + y * im.pitches[1] + x * 2 + c] = nv12_c(x, y, c);
   vaUnmapBuffer(va, im.buf);
   if (vaPutImage(va, surf, im.image_id, 0, 0, NV12_W, NV12_H, 0, 0, NV12_W, NV12_H) !=
       VA_STATUS_SUCCESS) {
      fprintf(stderr, "nv12: VA would not put the image onto the surface\n");
      return 1;
   }
   vaDestroyImage(va, im.image_id);
   vaSyncSurface(va, surf);

   VADRMPRIMESurfaceDescriptor d;
   if (vaExportSurfaceHandle(va, surf, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                             VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                             &d) != VA_STATUS_SUCCESS) {
      fprintf(stderr, "nv12: VA would not export the surface\n");
      return 1;
   }
   printf("nv12: VA exported fourcc %.4s, %u object(s), %u layer(s)\n", (char *)&d.fourcc,
          d.num_objects, d.num_layers);
   for (uint32_t o = 0; o < d.num_objects; o++) {
      uint32_t h = 0;
      struct drm_virtgpu_resource_info ri = {0};
      if (drmPrimeFDToHandle(drm, d.objects[o].fd, &h) == 0) {
         ri.bo_handle = h;
         ioctl(drm, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &ri);
      }
      printf("nv12:  object %u: size %u, modifier 0x%llx, resource %u, blob_mem %u\n", o,
             d.objects[o].size, (unsigned long long)d.objects[o].drm_format_modifier,
             ri.res_handle, ri.blob_mem);
   }
   for (uint32_t pl = 0; pl < d.layers[0].num_planes; pl++)
      printf("nv12:  plane %u: object %u offset %u pitch %u\n", pl,
             d.layers[0].object_index[pl], d.layers[0].offset[pl], d.layers[0].pitch[pl]);
   if (d.layers[0].num_planes != 2 || d.num_objects > 2) {
      fprintf(stderr, "nv12: not two planes in at most two objects\n");
      return 1;
   }
   int fds[2];
   uint32_t offsets[2], pitches[2];
   for (uint32_t pl = 0; pl < 2; pl++) {
      fds[pl] = d.objects[d.layers[0].object_index[pl]].fd;
      offsets[pl] = d.layers[0].offset[pl];
      pitches[pl] = d.layers[0].pitch[pl];
   }
   static uint8_t luma[NV12_W * NV12_H], chroma[NV12_W * NV12_H / 2];
   uint8_t *out[2] = {luma, chroma};
   int failed = yuv_import(NV12_W, NV12_H, 2, d.num_objects, fds, offsets, pitches, out);
   uint32_t ygood = 0, cgood = 0;
   for (uint32_t yy = 0; !failed && yy < NV12_H; yy++)
      for (uint32_t x = 0; x < NV12_W; x++)
         ygood += luma[yy * NV12_W + x] == nv12_y(x, yy);
   for (uint32_t yy = 0; !failed && yy < NV12_H / 2; yy++)
      for (uint32_t x = 0; x < NV12_W / 2; x++)
         for (uint32_t k = 0; k < 2; k++)
            cgood += chroma[(yy * NV12_W / 2 + x) * 2 + k] == nv12_c(x, yy, k);
   if (!failed)
      printf("nv12: luma %u/%u, chroma %u/%u bytes as VA wrote them\n", ygood, NV12_W * NV12_H,
             cgood, NV12_W * NV12_H / 2);
   for (uint32_t o = 0; o < d.num_objects; o++)
      close(d.objects[o].fd);
   vaDestroySurfaces(va, &surf, 1);
   vaTerminate(va);
   close(drm);
   return ygood == NV12_W * NV12_H && cgood == NV12_W * NV12_H / 2 ? 0 : 1;
}

/* -- vadecode -- */

#define VADECODE_FRAMES 60

static enum AVPixelFormat
vadecode_format(AVCodecContext *c, const enum AVPixelFormat *offered)
{
   (void)c;
   for (; *offered != AV_PIX_FMT_NONE; offered++) {
      if (*offered == AV_PIX_FMT_VAAPI)
         return *offered;
   }
   return AV_PIX_FMT_NONE;
}

/* One decoded frame, imported plane by plane and compared with ffmpeg's own read of it: the
 * number of rows that differ, or -1 when it could not be imported or read. */
static int
vadecode_frame(AVFrame *f, uint32_t index)
{
   AVFrame *drm = av_frame_alloc(), *ref = av_frame_alloc();
   drm->format = AV_PIX_FMT_DRM_PRIME;
   int wrong = -1;
   if (av_hwframe_map(drm, f, AV_HWFRAME_MAP_READ) < 0) {
      fprintf(stderr, "vadecode: frame %u would not map to DRM PRIME\n", index);
      goto out;
   }
   const AVDRMFrameDescriptor *d = (const AVDRMFrameDescriptor *)drm->data[0];
   int fds[3] = {-1, -1, -1};
   uint32_t offsets[3] = {0}, pitches[3] = {0}, n = 0;
   for (int l = 0; l < d->nb_layers; l++) {
      for (int p = 0; p < d->layers[l].nb_planes && n < 3; p++, n++) {
         fds[n] = d->objects[d->layers[l].planes[p].object_index].fd;
         offsets[n] = (uint32_t)d->layers[l].planes[p].offset;
         pitches[n] = (uint32_t)d->layers[l].planes[p].pitch;
      }
   }
   const uint32_t w = f->width, h = f->height, cw = (w + 1) / 2, ch = (h + 1) / 2;
   const uint32_t sample = n == 2 ? 2 : 1;
   if (index == 0)
      printf("vadecode: %ux%u, %u planes in %d dma-buf(s), pitches %u/%u/%u\n", w, h, n,
             d->nb_objects, pitches[0], pitches[1], n > 2 ? pitches[2] : 0);
   uint8_t *planes[3] = {malloc(w * h), malloc(cw * ch * 2), malloc(cw * ch)};
   const enum AVPixelFormat want = n == 2 ? AV_PIX_FMT_NV12 : AV_PIX_FMT_YUV420P;
   if (yuv_import(w, h, n, d->nb_objects, fds, offsets, pitches, planes)) {
      fprintf(stderr, "vadecode: frame %u was not imported\n", index);
   } else if (av_hwframe_transfer_data(ref, f, 0) < 0 || ref->format != want) {
      fprintf(stderr, "vadecode: frame %u was not read back in its own layout\n", index);
   } else {
      wrong = 0;
      for (uint32_t p = 0; p < n; p++) {
         const uint32_t rows = p ? ch : h, row = p ? cw * sample : w;
         for (uint32_t y = 0; y < rows; y++)
            wrong += memcmp(planes[p] + y * row, ref->data[p] + y * ref->linesize[p], row) != 0;
      }
   }
   for (uint32_t p = 0; p < 3; p++)
      free(planes[p]);
out:
   av_frame_free(&drm);
   av_frame_free(&ref);
   return wrong;
}

static int
timestamps(void)
{
   uint32_t n = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &n, NULL);
   VkQueueFamilyProperties *families = calloc(n, sizeof(*families));
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &n, families);
   const uint32_t bits = families[qfam].timestampValidBits;
   free(families);
   VkQueryPoolCreateInfo qi = {.sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
                               .queryType = VK_QUERY_TYPE_TIMESTAMP,
                               .queryCount = 2};
   VkQueryPool qp;
   VkResult r = vkCreateQueryPool(dev, &qi, NULL, &qp);
   if (!bits) {
      printf("timestamps: none advertised; a timestamp pool: %d\n", r);
      if (r == VK_SUCCESS)
         vkDestroyQueryPool(dev, qp, NULL);
      return 0;
   }
   if (r != VK_SUCCESS) {
      fprintf(stderr, "timestamps: %u valid bits advertised, but no pool: %d\n", bits, r);
      return 1;
   }
   VkCommandBuffer cb = begin();
   vkCmdResetQueryPool(cb, qp, 0, 2);
   vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, qp, 0);
   vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qp, 1);
   submit_and_wait(cb);
   uint64_t t[2] = {0, 0};
   r = vkGetQueryPoolResults(dev, qp, 0, 2, sizeof(t), t, sizeof(t[0]),
                             VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT);
   vkDestroyQueryPool(dev, qp, NULL);
   printf("timestamps: %u valid bits, read %d: %" PRIu64 " then %" PRIu64 "\n", bits, r, t[0],
          t[1]);
   return r != VK_SUCCESS || !t[1] || t[1] < t[0];
}

static int
vadecode(void)
{
   char clip[512];
   const char *env = getenv("KK_CLIP");
   if (env)
      snprintf(clip, sizeof(clip), "%s", env);
   else
      snprintf(clip, sizeof(clip), "%s/vab/clip.webm", getenv("HOME"));
   AVFormatContext *fmt = NULL;
   const AVCodec *dec = NULL;
   AVBufferRef *hw = NULL;
   if (avformat_open_input(&fmt, clip, NULL, NULL) < 0 || avformat_find_stream_info(fmt, NULL) < 0) {
      fprintf(stderr, "vadecode: cannot read %s\n", clip);
      return 1;
   }
   int stream = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, &dec, 0);
   if (stream < 0 ||
       av_hwdevice_ctx_create(&hw, AV_HWDEVICE_TYPE_VAAPI, "/dev/dri/renderD128", NULL, 0) < 0) {
      fprintf(stderr, "vadecode: no video stream or no VA device\n");
      return 1;
   }
   AVCodecContext *cc = avcodec_alloc_context3(dec);
   avcodec_parameters_to_context(cc, fmt->streams[stream]->codecpar);
   cc->hw_device_ctx = av_buffer_ref(hw);
   cc->get_format = vadecode_format;
   if (avcodec_open2(cc, dec, NULL) < 0) {
      fprintf(stderr, "vadecode: the decoder would not open\n");
      return 1;
   }
   AVPacket *pkt = av_packet_alloc();
   AVFrame *f = av_frame_alloc();
   uint32_t frames = 0, exact = 0;
   bool broken = false;
   while (frames < VADECODE_FRAMES && !broken && av_read_frame(fmt, pkt) >= 0) {
      if (pkt->stream_index == stream)
         avcodec_send_packet(cc, pkt);
      av_packet_unref(pkt);
      while (frames < VADECODE_FRAMES && avcodec_receive_frame(cc, f) == 0) {
         if (f->format != AV_PIX_FMT_VAAPI) {
            fprintf(stderr, "vadecode: frame %u was not decoded in hardware\n", frames);
            broken = true;
            break;
         }
         int wrong = vadecode_frame(f, frames);
         if (wrong < 0) {
            broken = true;
            break;
         }
         if (wrong)
            fprintf(stderr, "vadecode: frame %u: %d rows differ from ffmpeg's read\n", frames,
                    wrong);
         exact += wrong == 0;
         frames++;
         av_frame_unref(f);
      }
   }
   printf("vadecode: %u/%u frames imported as ffmpeg reads them\n", exact, frames);
   av_frame_free(&f);
   av_packet_free(&pkt);
   avcodec_free_context(&cc);
   av_buffer_unref(&hw);
   avformat_close_input(&fmt);
   return !broken && frames == VADECODE_FRAMES && exact == frames ? 0 : 1;
}

/* -- syncfd -- */

static int
sync_fd_follows_work(void)
{
   PFN_vkGetSemaphoreFdKHR get_semaphore_fd =
      (PFN_vkGetSemaphoreFdKHR)vkGetDeviceProcAddr(dev, "vkGetSemaphoreFdKHR");
   VkSemaphoreTypeCreateInfo ti = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
                                   .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE};
   VkSemaphoreCreateInfo tci = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &ti};
   VkSemaphore gate;
   CHECK(vkCreateSemaphore(dev, &tci, NULL, &gate));
   VkExportSemaphoreCreateInfo ex = {.sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
                                     .handleTypes =
                                        VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT};
   VkSemaphoreCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &ex};
   VkSemaphore done;
   CHECK(vkCreateSemaphore(dev, &bci, NULL, &done));

   struct buf b = host_buffer(4096, VK_BUFFER_USAGE_TRANSFER_DST_BIT);
   memset(b.map, 0, 4096);
   VkCommandBuffer cb = begin();
   vkCmdFillBuffer(cb, b.b, 0, 4096, 0xabcdabcd);
   CHECK(vkEndCommandBuffer(cb));

   uint64_t one = 1, unused = 0;
   VkTimelineSemaphoreSubmitInfo tsi = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
                                        .waitSemaphoreValueCount = 1,
                                        .pWaitSemaphoreValues = &one,
                                        .signalSemaphoreValueCount = 1,
                                        .pSignalSemaphoreValues = &unused};
   VkPipelineStageFlags stage = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
   VkSubmitInfo sub = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                       .pNext = &tsi,
                       .waitSemaphoreCount = 1,
                       .pWaitSemaphores = &gate,
                       .pWaitDstStageMask = &stage,
                       .commandBufferCount = 1,
                       .pCommandBuffers = &cb,
                       .signalSemaphoreCount = 1,
                       .pSignalSemaphores = &done};
   CHECK(vkQueueSubmit(queue, 1, &sub, VK_NULL_HANDLE));

   VkSemaphoreGetFdInfoKHR gi = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
                                 .semaphore = done,
                                 .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT};
   int fd = -1;
   CHECK(get_semaphore_fd(dev, &gi, &fd));
   bool early = fd >= 0 && fd_ready(fd, 300);

   VkSemaphoreSignalInfo si = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO,
                               .semaphore = gate,
                               .value = 1};
   CHECK(vkSignalSemaphore(dev, &si));
   bool after = fd >= 0 && fd_ready(fd, 3000);
   vkQueueWaitIdle(queue);
   uint32_t word = ((uint32_t *)b.map)[1023];
   printf("syncfd: fd %d, %s before the gate opened, %s after; the fill wrote 0x%x\n", fd,
          early ? "signaled" : "pending", after ? "signaled" : "pending", word);
   if (fd >= 0)
      close(fd);
   vkFreeCommandBuffers(dev, pool, 1, &cb);
   free_buf(b);
   vkDestroySemaphore(dev, done, NULL);
   vkDestroySemaphore(dev, gate, NULL);
   return fd >= 0 && !early && after && word == 0xabcdabcd ? 0 : 1;
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
   bool exporting = only && !strcmp(only, "export");
   bool nv12 = only && (!strcmp(only, "nv12") || !strcmp(only, "nv12-plain") ||
                        !strcmp(only, "vadecode"));
   bool importing = exporting || nv12 || (only && !strcmp(only, "import"));
   bool sharing = importing || (only && !strncmp(only, "share", 5));
   bool syncfd = only && !strcmp(only, "syncfd");
   const char *exts[] = {"VK_KHR_external_memory", "VK_KHR_external_memory_fd",
                         "VK_EXT_external_memory_dma_buf", "VK_EXT_image_drm_format_modifier"};
   const char *sem_exts[] = {"VK_KHR_external_semaphore", "VK_KHR_external_semaphore_fd"};
   VkPhysicalDeviceVulkan11Features f11 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES,
      .samplerYcbcrConversion = nv12};
   VkPhysicalDeviceVulkan12Features f12 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
      .pNext = &f11,
      .timelineSemaphore = only && (!strcmp(only, "signal") || !strcmp(only, "syncfd"))};
   VkDeviceCreateInfo di = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                            .pNext = &f12,
                            .queueCreateInfoCount = 1,
                            .pQueueCreateInfos = &qi,
                            .enabledExtensionCount = importing ? 4 : sharing ? 3 : syncfd ? 2 : 0,
                            .ppEnabledExtensionNames = syncfd ? sem_exts : exts};
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
   else if (syncfd)
      fails += sync_fd_follows_work();
   else if (exporting)
      fails += export_to_foreign();
   else if (only && !strcmp(only, "vadecode"))
      fails += vadecode();
   else if (nv12)
      fails += nv12_from_va(!strcmp(only, "nv12"));
   else if (importing)
      fails += import_foreign();
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
   if (!only || !strcmp(only, "timestamps"))
      fails += timestamps();

   vkDeviceWaitIdle(dev);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   printf("%s\n", fails ? "FAIL" : "PASS");
   return fails ? 1 : 0;
}
