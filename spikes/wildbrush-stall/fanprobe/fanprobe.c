// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Host-side oracle for KosmicKrisp triangle-fan draws: renders fans with a flat-shaded colour
// that encodes the PROVOKING vertex index, reads the image back, and compares every pixel with a
// CPU rasterisation of the same fans under Vulkan's rule -- fan triangle i is (i+1, i+2, 0) with
// vertex i+1 provoking. Exercises firstVertex, instancing and multi-draw, which are the inputs the
// static-index fast path rewrites.
//
// A/B: LIMINA_KK_NO_FAN_STATIC=1 sends the same draws through KK's GPU unroll instead.
// Build+run: see run.sh.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <vulkan/vulkan.h>

#define W 128
#define H 128
#define CHECK(x)                                                              \
   do {                                                                       \
      VkResult r_ = (x);                                                      \
      if (r_ != VK_SUCCESS) {                                                 \
         fprintf(stderr, "%s:%d: %s -> %d\n", __FILE__, __LINE__, #x, r_);    \
         exit(2);                                                             \
      }                                                                       \
   } while (0)

static uint32_t *read_file(const char *path, size_t *len)
{
   FILE *f = fopen(path, "rb");
   if (!f) { perror(path); exit(2); }
   fseek(f, 0, SEEK_END);
   *len = ftell(f);
   fseek(f, 0, SEEK_SET);
   uint32_t *buf = malloc(*len);
   if (fread(buf, 1, *len, f) != *len) exit(2);
   fclose(f);
   return buf;
}

// Must match fan.vert: vertex v of the fan, for instance k, sits on a circle around the
// instance's centre; v % SEG picks the angle, the hub is v == first.
struct push { int32_t first; int32_t seg; float radius; float pad; };

static void vpos(int32_t v, int32_t inst, const struct push *p, float *x, float *y)
{
   float cx = inst == 0 ? -0.45f : 0.45f, cy = 0.0f;
   int32_t local = v - p->first;
   if (local == 0) { *x = cx; *y = cy; return; }
   float a = (float)(local - 1) * 6.2831853f / (float)p->seg;
   *x = cx + p->radius * cosf(a);
   *y = cy + p->radius * sinf(a);
}

static float edge(float ax, float ay, float bx, float by, float px, float py)
{
   return (bx - ax) * (py - ay) - (by - ay) * (px - ax);
}

int main(void)
{
   VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3};
   const char *inst_ext[] = {VK_KHR_PORTABILITY_ENUMERATION_EXTENSION_NAME};
   VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                               .flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
                               .pApplicationInfo = &app,
                               .enabledExtensionCount = 1, .ppEnabledExtensionNames = inst_ext};
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t n = 1;
   VkPhysicalDevice pd;
   vkEnumeratePhysicalDevices(inst, &n, &pd);
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   printf("device: %s\n", props.deviceName);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                  .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio};
   VkPhysicalDeviceMultiDrawFeaturesEXT md = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MULTI_DRAW_FEATURES_EXT,
                                              .multiDraw = VK_TRUE};
   VkPhysicalDeviceVulkan13Features v13 = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
                                           .pNext = &md, .dynamicRendering = VK_TRUE};
   const char *dev_ext[] = {VK_EXT_MULTI_DRAW_EXTENSION_NAME};
   VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &v13,
                             .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                             .enabledExtensionCount = 1, .ppEnabledExtensionNames = dev_ext};
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, 0, 0, &q);
   PFN_vkCmdDrawMultiEXT drawMulti = (PFN_vkCmdDrawMultiEXT)vkGetDeviceProcAddr(dev, "vkCmdDrawMultiEXT");

   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);

   // Colour target: R32_UINT holds the provoking vertex index + 1 (0 = background).
   VkImageCreateInfo imci = {VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
                             .format = VK_FORMAT_R32_UINT, .extent = {W, H, 1}, .mipLevels = 1,
                             .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
                             .tiling = VK_IMAGE_TILING_OPTIMAL,
                             .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT};
   VkImage img;
   CHECK(vkCreateImage(dev, &imci, NULL, &img));
   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, img, &mr);
   uint32_t mt = 0;
   while (!(mr.memoryTypeBits & (1u << mt))) mt++;
   VkMemoryAllocateInfo mai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size, .memoryTypeIndex = mt};
   VkDeviceMemory imem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &imem));
   CHECK(vkBindImageMemory(dev, img, imem, 0));
   VkImageViewCreateInfo ivci = {VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = img,
                                 .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_R32_UINT,
                                 .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
   VkImageView view;
   CHECK(vkCreateImageView(dev, &ivci, NULL, &view));

   VkBufferCreateInfo bci = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = W * H * 4,
                             .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT};
   VkBuffer rb;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &rb));
   vkGetBufferMemoryRequirements(dev, rb, &mr);
   mt = 0;
   while (!((mr.memoryTypeBits & (1u << mt)) &&
            (mp.memoryTypes[mt].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) &&
            (mp.memoryTypes[mt].propertyFlags & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT))) mt++;
   mai.allocationSize = mr.size;
   mai.memoryTypeIndex = mt;
   VkDeviceMemory bmem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &bmem));
   CHECK(vkBindBufferMemory(dev, rb, bmem, 0));

   size_t vlen, flen;
   uint32_t *vs = read_file("fan.vert.spv", &vlen), *fs = read_file("fan.frag.spv", &flen);
   VkShaderModule vsm, fsm;
   VkShaderModuleCreateInfo smci = {VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = vlen, .pCode = vs};
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &vsm));
   smci.codeSize = flen;
   smci.pCode = fs;
   CHECK(vkCreateShaderModule(dev, &smci, NULL, &fsm));

   VkPushConstantRange pcr = {VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(struct push)};
   VkPipelineLayoutCreateInfo plci = {VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                      .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr};
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));
   VkPipelineShaderStageCreateInfo stages[2] = {
      {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vsm, .pName = "main"},
      {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fsm, .pName = "main"}};
   VkPipelineVertexInputStateCreateInfo vi = {VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
   VkPipelineInputAssemblyStateCreateInfo ia = {VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                                                .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_FAN};
   VkViewport vp = {0, 0, W, H, 0, 1};
   VkRect2D sc = {{0, 0}, {W, H}};
   VkPipelineViewportStateCreateInfo vps = {VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                                            .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc};
   VkPipelineRasterizationStateCreateInfo rs = {VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                                                .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
                                                .lineWidth = 1.0f};
   VkPipelineMultisampleStateCreateInfo ms = {VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                                              .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
   VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
   VkPipelineColorBlendStateCreateInfo cb = {VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
                                             .attachmentCount = 1, .pAttachments = &cba};
   VkFormat fmt = VK_FORMAT_R32_UINT;
   VkPipelineRenderingCreateInfo prci = {VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
                                         .colorAttachmentCount = 1, .pColorAttachmentFormats = &fmt};
   VkGraphicsPipelineCreateInfo gpci = {VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &prci,
                                        .stageCount = 2, .pStages = stages, .pVertexInputState = &vi,
                                        .pInputAssemblyState = &ia, .pViewportState = &vps,
                                        .pRasterizationState = &rs, .pMultisampleState = &ms,
                                        .pColorBlendState = &cb, .layout = pl};
   VkPipeline pipe;
   CHECK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpci, NULL, &pipe));

   VkCommandPoolCreateInfo cpci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = 0};
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &cpci, NULL, &pool));
   VkCommandBufferAllocateInfo cbai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool,
                                       .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cmd));

   // Two cases, each a separate submit + readback:
   //   case 0: vkCmdDraw, 13-vertex fan (12 segments, closing the circle), firstVertex 1000,
   //           2 instances (left and right circle).
   //   case 1: vkCmdDrawMultiEXT: two fans in one call, 7 and 5 vertices at firstVertex 20 / 300,
   //           plus a 2-vertex draw that must produce nothing, 1 instance offset by firstInstance 1.
   int failures = 0;
   for (int c = 0; c < 2; c++) {
      struct push p[3];
      int ndraws;
      uint32_t count[3], first[3], ninst, first_inst;
      if (c == 0) {
         ndraws = 1; count[0] = 13; first[0] = 1000; ninst = 2; first_inst = 0;
      } else {
         ndraws = 3; count[0] = 7; first[0] = 20; count[1] = 5; first[1] = 300;
         count[2] = 2; first[2] = 50; ninst = 1; first_inst = 1;
      }

      VkCommandBufferBeginInfo cbbi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                       .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
      CHECK(vkBeginCommandBuffer(cmd, &cbbi));
      VkImageMemoryBarrier imb = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                                  .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                                  .image = img, .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
      vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                           0, 0, NULL, 0, NULL, 1, &imb);
      VkRenderingAttachmentInfo att = {VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = view,
                                       .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                                       .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE};
      VkRenderingInfo ri = {VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = {{0, 0}, {W, H}}, .layerCount = 1,
                            .colorAttachmentCount = 1, .pColorAttachments = &att};
      vkCmdBeginRendering(cmd, &ri);
      vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
      if (c == 0) {
         p[0] = (struct push){(int32_t)first[0], (int32_t)count[0] - 2, 0.4f, 0};
         vkCmdPushConstants(cmd, pl, VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(p[0]), &p[0]);
         vkCmdDraw(cmd, count[0], ninst, first[0], first_inst);
      } else {
         // One push-constant set for the whole multi-draw: the shader recovers each fan's hub
         // from gl_VertexIndex by the known firstVertex values, so encode them all at once.
         p[0] = (struct push){0, 0, 0.3f, 0};
         vkCmdPushConstants(cmd, pl, VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(p[0]), &p[0]);
         VkMultiDrawInfoEXT mdi[3];
         for (int d = 0; d < ndraws; d++) mdi[d] = (VkMultiDrawInfoEXT){first[d], count[d]};
         drawMulti(cmd, ndraws, mdi, ninst, first_inst, sizeof(VkMultiDrawInfoEXT));
      }
      vkCmdEndRendering(cmd);
      imb.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
      imb.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
      imb.oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
      imb.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
      vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                           0, 0, NULL, 0, NULL, 1, &imb);
      VkBufferImageCopy bic = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1}, .imageExtent = {W, H, 1}};
      vkCmdCopyImageToBuffer(cmd, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, rb, 1, &bic);
      CHECK(vkEndCommandBuffer(cmd));
      VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cmd};
      CHECK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE));
      CHECK(vkQueueWaitIdle(q));
      CHECK(vkResetCommandPool(dev, pool, 0));

      uint32_t *got;
      CHECK(vkMapMemory(dev, bmem, 0, W * H * 4, 0, (void **)&got));

      // CPU reference. Sample at pixel centres; skip pixels within half a pixel of any edge,
      // where rasterisation rules may legitimately differ.
      int bad = 0, covered = 0, skipped = 0;
      for (int y = 0; y < H; y++) {
         for (int x = 0; x < W; x++) {
            float px = (x + 0.5f) / W * 2.0f - 1.0f, py = (y + 0.5f) / H * 2.0f - 1.0f;
            uint32_t want = 0;
            int ambiguous = 0;
            for (int d = 0; d < ndraws; d++) {
               if (count[d] < 3) continue;
               for (uint32_t k = 0; k < ninst; k++) {
                  int32_t inst = (int32_t)(first_inst + k);
                  struct push pp = c == 0 ? p[0] : (struct push){(int32_t)first[d], (int32_t)count[d] - 2, 0.3f, 0};
                  for (uint32_t t = 0; t + 2 < count[d]; t++) {
                     int32_t v[3] = {(int32_t)(first[d] + t + 1), (int32_t)(first[d] + t + 2), (int32_t)first[d]};
                     float vx[3], vy[3];
                     for (int j = 0; j < 3; j++) vpos(v[j], c == 0 ? inst : (d == 0 ? 0 : 1), &pp, &vx[j], &vy[j]);
                     float e0 = edge(vx[0], vy[0], vx[1], vy[1], px, py);
                     float e1 = edge(vx[1], vy[1], vx[2], vy[2], px, py);
                     float e2 = edge(vx[2], vy[2], vx[0], vy[0], px, py);
                     float area = edge(vx[0], vy[0], vx[1], vy[1], vx[2], vy[2]);
                     float s = area < 0 ? -1.0f : 1.0f;
                     float m = fminf(fminf(e0 * s, e1 * s), e2 * s);
                     // Normalised distance to the nearest edge, roughly in NDC units.
                     float tol = 2.0f / W * 1.5f;
                     float len = fmaxf(hypotf(vx[1] - vx[0], vy[1] - vy[0]),
                                       fmaxf(hypotf(vx[2] - vx[1], vy[2] - vy[1]), hypotf(vx[0] - vx[2], vy[0] - vy[2])));
                     if (fabsf(m) < tol * len) ambiguous = 1;
                     if (m > 0) want = (uint32_t)v[0] + 1u + (uint32_t)inst * 100000u;
                  }
               }
            }
            if (ambiguous) { skipped++; continue; }
            uint32_t g = got[y * W + x];
            if (want) covered++;
            if (g != want) {
               if (bad < 10)
                  fprintf(stderr, "  case %d pixel %d,%d: got %u want %u\n", c, x, y, g, want);
               bad++;
            }
         }
      }
      vkUnmapMemory(dev, bmem);
      printf("case %d: %d covered pixels checked, %d edge pixels skipped, %d mismatches -> %s\n",
             c, covered, skipped, bad, bad ? "FAIL" : "ok");
      failures += bad != 0;
   }
   printf("%s\n", failures ? "FAIL" : "PASS");
   return failures ? 1 : 0;
}
