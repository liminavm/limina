// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Indirect draw counts above KosmicKrisp's advertised maxDrawIndirectCount (companion to
// probe.c). A venus guest's drawCount / maxDrawCount reaches KK's indirect entry points
// unchecked; KK must drop an over-limit draw rather than act on it.
//
// Each case draws the control (one indirect triangle covering the target), then the over-limit
// draw, then the control again in a new submission. PASS: the device survived within the case's
// 30 s and the control still renders red.
//
// Build (the SPIR-V headers come from tess/gen.sh):
//   cc -Wall -I/opt/homebrew/include indirect-probe.c -L/opt/homebrew/lib -lvulkan -o ip
//   VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./ip [case]
// Each case re-executes the binary: a bare fork() cannot reach Metal's XPC services.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/wait.h>
#include <unistd.h>

#include "tess/tess_vert.h"
#include "tess/tess_frag.h"

enum { W = 16, H = 16 };
// Above maxDrawIndirectCount (65535); 20 times it does not fit in 32 bits.
#define OVER_LIMIT 214748365u

static VkInstance inst; static VkPhysicalDevice pd; static VkDevice dev; static VkQueue q;
static VkCommandPool cpool;
static VkImage img; static VkImageView view;
static VkBuffer readback, indirect, count_buf;
static void *readback_map, *indirect_map, *count_map;
static VkPipelineLayout layout;

static uint32_t mem_type(uint32_t bits, VkMemoryPropertyFlags want) {
  VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) return i;
  fprintf(stderr, "no memory type\n"); exit(2);
}

static VkBuffer host_buffer(VkDeviceSize size, VkBufferUsageFlags usage, void **map) {
  VkBufferCreateInfo bi = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = size, .usage = usage};
  VkBuffer b; vkCreateBuffer(dev, &bi, NULL, &b);
  VkMemoryRequirements r; vkGetBufferMemoryRequirements(dev, b, &r);
  VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = r.size,
    .memoryTypeIndex = mem_type(r.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)};
  VkDeviceMemory m; vkAllocateMemory(dev, &ai, NULL, &m);
  vkBindBufferMemory(dev, b, m, 0);
  vkMapMemory(dev, m, 0, VK_WHOLE_SIZE, 0, map);
  memset(*map, 0, size);
  return b;
}

static void setup(void) {
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3};
  const char *ie[] = {"VK_KHR_portability_enumeration"};
  VkInstanceCreateInfo ic = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
                             .flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
                             .enabledExtensionCount = 1, .ppEnabledExtensionNames = ie};
  if (vkCreateInstance(&ic, NULL, &inst)) { fprintf(stderr, "instance\n"); exit(2); }
  uint32_t n = 1; vkEnumeratePhysicalDevices(inst, &n, &pd);
  if (!n) { fprintf(stderr, "no device\n"); exit(2); }
  float pr = 1;
  VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &pr};
  VkPhysicalDeviceVulkan13Features f13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
                                          .dynamicRendering = VK_TRUE, .synchronization2 = VK_TRUE};
  VkPhysicalDeviceVulkan12Features f12 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
                                          .pNext = &f13, .drawIndirectCount = VK_TRUE};
  VkPhysicalDeviceFeatures2 f = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &f12,
                                 .features = {.multiDrawIndirect = VK_TRUE}};
  VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f, .queueCreateInfoCount = 1,
                           .pQueueCreateInfos = &qci};
  if (vkCreateDevice(pd, &dc, NULL, &dev)) { fprintf(stderr, "device\n"); exit(2); }
  vkGetDeviceQueue(dev, 0, 0, &q);
  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  vkCreateCommandPool(dev, &cpi, NULL, &cpool);

  VkImageCreateInfo ii = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
    .format = VK_FORMAT_R8G8B8A8_UNORM, .extent = {W, H, 1}, .mipLevels = 1, .arrayLayers = 1,
    .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
    .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT};
  vkCreateImage(dev, &ii, NULL, &img);
  VkMemoryRequirements r; vkGetImageMemoryRequirements(dev, img, &r);
  VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = r.size,
                             .memoryTypeIndex = mem_type(r.memoryTypeBits, 0)};
  VkDeviceMemory m; vkAllocateMemory(dev, &ai, NULL, &m);
  vkBindImageMemory(dev, img, m, 0);
  VkImageViewCreateInfo vi = {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = img,
    .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
    .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  vkCreateImageView(dev, &vi, NULL, &view);
  readback = host_buffer(W * H * 4, VK_BUFFER_USAGE_TRANSFER_DST_BIT, &readback_map);
  indirect = host_buffer(sizeof(VkDrawIndirectCommand), VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT, &indirect_map);
  *(VkDrawIndirectCommand *)indirect_map = (VkDrawIndirectCommand){3, 1, 0, 0};
  count_buf = host_buffer(sizeof(uint32_t), VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT, &count_map);
  *(uint32_t *)count_map = 1;
  VkPipelineLayoutCreateInfo li = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
  vkCreatePipelineLayout(dev, &li, NULL, &layout);
}

static VkShaderModule module(const uint32_t *code, size_t size) {
  VkShaderModuleCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = size, .pCode = code};
  VkShaderModule s; vkCreateShaderModule(dev, &ci, NULL, &s);
  return s;
}

static VkPipeline make_pipeline(VkPrimitiveTopology topology) {
  VkPipelineShaderStageCreateInfo st[2] = {
    {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_VERTEX_BIT, module(tess_vert, sizeof(tess_vert)), "main", NULL},
    {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_FRAGMENT_BIT, module(tess_frag, sizeof(tess_frag)), "main", NULL},
  };
  VkPipelineVertexInputStateCreateInfo vin = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo ia = {.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                                               .topology = topology};
  VkPipelineViewportStateCreateInfo vp = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                                          .viewportCount = 1, .scissorCount = 1};
  VkPipelineRasterizationStateCreateInfo rs = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                                               .cullMode = VK_CULL_MODE_NONE, .lineWidth = 1};
  VkPipelineMultisampleStateCreateInfo ms = {.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                                             .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
  VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
  VkPipelineColorBlendStateCreateInfo cb = {.sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
                                            .attachmentCount = 1, .pAttachments = &cba};
  VkDynamicState ds[] = {VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR};
  VkPipelineDynamicStateCreateInfo dyn = {.sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
                                          .dynamicStateCount = 2, .pDynamicStates = ds};
  VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;
  VkPipelineRenderingCreateInfo ri = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
                                      .colorAttachmentCount = 1, .pColorAttachmentFormats = &fmt};
  VkGraphicsPipelineCreateInfo gi = {.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &ri,
    .stageCount = 2, .pStages = st, .pVertexInputState = &vin, .pInputAssemblyState = &ia,
    .pViewportState = &vp, .pRasterizationState = &rs, .pMultisampleState = &ms,
    .pColorBlendState = &cb, .pDynamicState = &dyn, .layout = layout};
  VkPipeline p = VK_NULL_HANDLE;
  if (vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gi, NULL, &p)) { printf("  pipeline\n"); exit(1); }
  return p;
}

static void image_barrier(VkCommandBuffer cb, VkImageLayout from, VkImageLayout to) {
  VkImageMemoryBarrier2 b = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER_2,
    .srcStageMask = VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT, .srcAccessMask = VK_ACCESS_2_MEMORY_WRITE_BIT,
    .dstStageMask = VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT, .dstAccessMask = VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT,
    .oldLayout = from, .newLayout = to, .image = img, .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  VkDependencyInfo di = {.sType = VK_STRUCTURE_TYPE_DEPENDENCY_INFO, .imageMemoryBarrierCount = 1, .pImageMemoryBarriers = &b};
  vkCmdPipelineBarrier2(cb, &di);
}

enum mode { PLAIN, WITH_COUNT };

// Clear to black, one indirect draw of `count` records (from the count buffer for WITH_COUNT,
// whose maxDrawCount is `count`), read back. Returns the submission's result.
static VkResult draw(VkPipeline p, enum mode mode, uint32_t count) {
  VkCommandBufferAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool,
                                    .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
  VkCommandBuffer cb; vkAllocateCommandBuffers(dev, &ai, &cb);
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  vkBeginCommandBuffer(cb, &bi);
  image_barrier(cb, VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL);
  VkRenderingAttachmentInfo ca = {.sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = view,
    .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
    .storeOp = VK_ATTACHMENT_STORE_OP_STORE, .clearValue = {.color = {{0, 0, 0, 1}}}};
  VkRenderingInfo ri = {.sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = {{0, 0}, {W, H}},
                        .layerCount = 1, .colorAttachmentCount = 1, .pColorAttachments = &ca};
  vkCmdBeginRendering(cb, &ri);
  vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, p);
  VkViewport v = {0, 0, W, H, 0, 1}; vkCmdSetViewport(cb, 0, 1, &v);
  VkRect2D s = {{0, 0}, {W, H}}; vkCmdSetScissor(cb, 0, 1, &s);
  if (mode == WITH_COUNT)
    vkCmdDrawIndirectCount(cb, indirect, 0, count_buf, 0, count, sizeof(VkDrawIndirectCommand));
  else
    vkCmdDrawIndirect(cb, indirect, 0, count, sizeof(VkDrawIndirectCommand));
  vkCmdEndRendering(cb);
  image_barrier(cb, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL);
  VkBufferImageCopy c = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1}, .imageExtent = {W, H, 1}};
  vkCmdCopyImageToBuffer(cb, img, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, readback, 1, &c);
  VkResult r = vkEndCommandBuffer(cb);
  if (r) return r;
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
  r = vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE);
  if (r) return r;
  return vkQueueWaitIdle(q);
}

static int center_is_red(void) {
  const uint8_t *px = (const uint8_t *)readback_map + ((H / 2) * W + W / 2) * 4;
  return px[0] == 255 && px[1] == 0 && px[2] == 0;
}

static int control(VkPipeline p, enum mode mode, const char *when) {
  VkResult r = draw(p, mode, 1);
  if (r) { printf("  control %s: %d\n", when, r); return 0; }
  if (!center_is_red()) { printf("  control %s: did not render\n", when); return 0; }
  return 1;
}

static int over_limit(VkPrimitiveTopology topology, enum mode mode) {
  VkPipeline p = make_pipeline(topology);
  if (!control(p, mode, "before")) return 0;
  VkResult r = draw(p, mode, OVER_LIMIT);
  if (r == VK_ERROR_DEVICE_LOST) { printf("  over-limit draw lost the device\n"); return 0; }
  if (r) printf("  over-limit draw refused: %d\n", r);
  return control(p, mode, "after");
}

static int ctl_plain(void) { return control(make_pipeline(VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST), PLAIN, "plain"); }
static int ctl_count(void) { return control(make_pipeline(VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST), WITH_COUNT, "count"); }
static int ctl_fan(void) { return control(make_pipeline(VK_PRIMITIVE_TOPOLOGY_TRIANGLE_FAN), PLAIN, "fan"); }
static int plain_over(void) { return over_limit(VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST, PLAIN); }
static int count_over(void) { return over_limit(VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST, WITH_COUNT); }
// A fan is unrolled to a list, which rewrites the draw records.
static int fan_over(void) { return over_limit(VK_PRIMITIVE_TOPOLOGY_TRIANGLE_FAN, PLAIN); }

static const struct { const char *name; int (*fn)(void); } tests[] = {
  {"ctl-plain", ctl_plain},
  {"ctl-count", ctl_count},
  {"ctl-fan", ctl_fan},
  {"plain-over", plain_over},
  {"count-over", count_over},
  {"fan-over", fan_over},
};

int main(int argc, char **argv) {
  if (argc > 2 && !strcmp(argv[2], "--child")) {
    alarm(30);  // a stalled case is a result too (SIGALRM)
    for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++)
      if (!strcmp(argv[1], tests[i].name)) { setup(); return tests[i].fn() == 1 ? 0 : 1; }
    return 2;
  }
  int fails = 0;
  for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
    if (argc > 1 && strcmp(argv[1], tests[i].name)) continue;
    fflush(stdout);
    // A bare fork cannot reach Metal's XPC services, so each case re-executes this binary.
    pid_t p = fork();
    if (p == 0) { execl(argv[0], argv[0], tests[i].name, "--child", (char *)NULL); _exit(3); }
    int st;
    waitpid(p, &st, 0);
    const char *v = WIFSIGNALED(st) ? (WTERMSIG(st) == SIGALRM ? "HANG" : "CRASH")
                    : WEXITSTATUS(st) == 0 ? "PASS" : "FAIL";
    if (WIFSIGNALED(st)) printf("  (signal %d)\n", WTERMSIG(st));
    printf("%-22s %s\n", tests[i].name, v);
    fails += strcmp(v, "PASS") != 0;
  }
  return fails != 0;
}
