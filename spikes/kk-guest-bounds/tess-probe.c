// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Tessellation patch sizes a venus guest can hand KosmicKrisp (companion to probe.c). KK
// advertises tessellationShader, extendedDynamicState2PatchControlPoints and
// maxTessellationPatchSize = 32, and a guest's patch size -- from the pipeline's
// VkPipelineTessellationStateCreateInfo or vkCmdSetPatchControlPointsEXT -- reaches KK's draw
// path unvalidated, where it divides the vertex count (on the CPU for a direct draw, on the GPU
// for an indirect one). Each case runs in its own process.
//
// Every case first draws the control (3 control points, one patch covering the target) to prove
// the pipeline renders, then the bad draw, then the control again in a new submission: PASS
// means the device survived (no abort, no lost device, no hang) and still renders red. What the
// bad draw itself rendered is not checked; drawing nothing is the expected outcome.
//
// Build (the SPIR-V headers come from tess/gen.sh):
//   cc -Wall -I/opt/homebrew/include tess-probe.c -L/opt/homebrew/lib -lvulkan -o tp
//   VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./tp [case]
// Each case re-executes the binary: a bare fork() cannot reach Metal's XPC services.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/wait.h>
#include <unistd.h>

#include "tess/tess_vert.h"
#include "tess/tess_tesc.h"
#include "tess/tess_tese.h"
#include "tess/tess_frag.h"

enum { W = 16, H = 16 };

static VkInstance inst; static VkPhysicalDevice pd; static VkDevice dev; static VkQueue q;
static VkCommandPool cpool;
static VkImage img; static VkImageView view;
static VkBuffer readback, indirect; static void *readback_map, *indirect_map;
static VkPipelineLayout layout;
static PFN_vkCmdSetPatchControlPointsEXT set_pcp;

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
  VkPhysicalDeviceExtendedDynamicState2FeaturesEXT eds2 = {
    .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_DYNAMIC_STATE_2_FEATURES_EXT,
    .extendedDynamicState2PatchControlPoints = VK_TRUE};
  VkPhysicalDeviceVulkan13Features f13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
                                          .pNext = &eds2, .dynamicRendering = VK_TRUE, .synchronization2 = VK_TRUE};
  VkPhysicalDeviceFeatures2 f = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &f13,
                                 .features = {.tessellationShader = VK_TRUE}};
  const char *de[] = {"VK_EXT_extended_dynamic_state2"};
  VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f, .queueCreateInfoCount = 1,
                           .pQueueCreateInfos = &qci, .enabledExtensionCount = 1, .ppEnabledExtensionNames = de};
  if (vkCreateDevice(pd, &dc, NULL, &dev)) { fprintf(stderr, "device\n"); exit(2); }
  vkGetDeviceQueue(dev, 0, 0, &q);
  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  vkCreateCommandPool(dev, &cpi, NULL, &cpool);
  set_pcp = (PFN_vkCmdSetPatchControlPointsEXT)vkGetDeviceProcAddr(dev, "vkCmdSetPatchControlPointsEXT");

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
  VkPipelineLayoutCreateInfo li = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
  vkCreatePipelineLayout(dev, &li, NULL, &layout);
}

static VkShaderModule module(const uint32_t *code, size_t size) {
  VkShaderModuleCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = size, .pCode = code};
  VkShaderModule s; vkCreateShaderModule(dev, &ci, NULL, &s);
  return s;
}

// dynamic: patch size from vkCmdSetPatchControlPointsEXT; otherwise baked as static_pcp.
static VkPipeline make_pipeline(int dynamic, uint32_t static_pcp) {
  VkPipelineShaderStageCreateInfo st[4] = {
    {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_VERTEX_BIT, module(tess_vert, sizeof(tess_vert)), "main", NULL},
    {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_TESSELLATION_CONTROL_BIT, module(tess_tesc, sizeof(tess_tesc)), "main", NULL},
    {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_TESSELLATION_EVALUATION_BIT, module(tess_tese, sizeof(tess_tese)), "main", NULL},
    {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0, VK_SHADER_STAGE_FRAGMENT_BIT, module(tess_frag, sizeof(tess_frag)), "main", NULL},
  };
  VkPipelineVertexInputStateCreateInfo vin = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo ia = {.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                                               .topology = VK_PRIMITIVE_TOPOLOGY_PATCH_LIST};
  VkPipelineTessellationStateCreateInfo ts = {.sType = VK_STRUCTURE_TYPE_PIPELINE_TESSELLATION_STATE_CREATE_INFO,
                                              .patchControlPoints = static_pcp};
  VkPipelineViewportStateCreateInfo vp = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                                          .viewportCount = 1, .scissorCount = 1};
  VkPipelineRasterizationStateCreateInfo rs = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                                               .cullMode = VK_CULL_MODE_NONE, .lineWidth = 1};
  VkPipelineMultisampleStateCreateInfo ms = {.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                                             .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
  VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
  VkPipelineColorBlendStateCreateInfo cb = {.sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
                                            .attachmentCount = 1, .pAttachments = &cba};
  VkDynamicState ds[] = {VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR, VK_DYNAMIC_STATE_PATCH_CONTROL_POINTS_EXT};
  VkPipelineDynamicStateCreateInfo dyn = {.sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
                                          .dynamicStateCount = dynamic ? 3 : 2, .pDynamicStates = ds};
  VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;
  VkPipelineRenderingCreateInfo ri = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
                                      .colorAttachmentCount = 1, .pColorAttachmentFormats = &fmt};
  VkGraphicsPipelineCreateInfo gi = {.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &ri,
    .stageCount = 4, .pStages = st, .pVertexInputState = &vin, .pInputAssemblyState = &ia,
    .pTessellationState = &ts, .pViewportState = &vp, .pRasterizationState = &rs, .pMultisampleState = &ms,
    .pColorBlendState = &cb, .pDynamicState = &dyn, .layout = layout};
  VkPipeline p = VK_NULL_HANDLE;
  VkResult res = vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gi, NULL, &p);
  if (res) { printf("  pipeline: %d\n", res); exit(1); }
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

// Clear to black, draw `vertices` with patch size `pcp` (-1 = leave the pipeline's), read back.
// Returns the submission's result.
static VkResult draw(VkPipeline p, int64_t pcp, uint32_t vertices, int indirect_draw) {
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
  if (pcp >= 0) set_pcp(cb, (uint32_t)pcp);
  if (indirect_draw) {
    *(VkDrawIndirectCommand *)indirect_map = (VkDrawIndirectCommand){vertices, 1, 0, 0};
    vkCmdDrawIndirect(cb, indirect, 0, 1, 0);
  } else {
    vkCmdDraw(cb, vertices, 1, 0, 0);
  }
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

static int control(VkPipeline p, int64_t pcp, const char *when) {
  VkResult r = draw(p, pcp, 3, 0);
  if (r) { printf("  control %s: %d\n", when, r); return 0; }
  if (!center_is_red()) { printf("  control %s: did not render\n", when); return 0; }
  return 1;
}

// The bad draw between two controls. A refusal of the bad command buffer (an error from
// vkEndCommandBuffer) is fine; a lost device or a control that no longer renders is not.
static int bad_draw(VkPipeline p, int64_t ctl_pcp, int64_t pcp, uint32_t vertices, int indirect_draw) {
  if (!control(p, ctl_pcp, "before")) return 0;
  VkResult r = draw(p, pcp, vertices, indirect_draw);
  if (r == VK_ERROR_DEVICE_LOST) { printf("  bad draw lost the device\n"); return 0; }
  if (r) printf("  bad draw refused: %d\n", r);
  return control(p, ctl_pcp, "after");
}

static int ctl_direct(void) { return control(make_pipeline(1, 0), 3, "direct"); }
static int ctl_indirect(void) {
  VkPipeline p = make_pipeline(1, 0);
  VkResult r = draw(p, 3, 3, 1);
  if (r) { printf("  indirect control: %d\n", r); return 0; }
  if (!center_is_red()) { printf("  indirect control: did not render\n"); return 0; }
  return 1;
}
static int ctl_static(void) { return control(make_pipeline(0, 3), -1, "static"); }
static int pcp0_direct(void) { return bad_draw(make_pipeline(1, 0), 3, 0, 3, 0); }
static int pcp0_indirect(void) { return bad_draw(make_pipeline(1, 0), 3, 0, 3, 1); }

static int pcp64_direct(void) { return bad_draw(make_pipeline(1, 0), 3, 64, 192, 0); }
static int pcp64_indirect(void) { return bad_draw(make_pipeline(1, 0), 3, 64, 192, 1); }
// The runtime stores the patch size in a uint8_t: 256 wraps to 0, 257 to a valid-looking 1.
static int pcp256_direct(void) { return bad_draw(make_pipeline(1, 0), 3, 256, 3, 0); }
static int pcp256_indirect(void) { return bad_draw(make_pipeline(1, 0), 3, 256, 3, 1); }
static int pcp257_direct(void) { return bad_draw(make_pipeline(1, 0), 3, 257, 3, 0); }
static int pcpmax_direct(void) { return bad_draw(make_pipeline(1, 0), 3, UINT32_MAX, 3, 0); }
static int static_pcp0(void) {
  VkPipeline p = make_pipeline(0, 0);
  VkPipeline good = make_pipeline(0, 3);
  if (!control(good, -1, "before")) return 0;
  VkResult r = draw(p, -1, 3, 0);
  if (r == VK_ERROR_DEVICE_LOST) { printf("  bad draw lost the device\n"); return 0; }
  if (r) printf("  bad draw refused: %d\n", r);
  r = draw(p, -1, 3, 1);
  if (r == VK_ERROR_DEVICE_LOST) { printf("  bad indirect draw lost the device\n"); return 0; }
  if (r) printf("  bad indirect draw refused: %d\n", r);
  return control(good, -1, "after");
}

static const struct { const char *name; int (*fn)(void); } tests[] = {
  {"ctl-direct", ctl_direct},
  {"ctl-indirect", ctl_indirect},
  {"ctl-static", ctl_static},
  {"pcp0-direct", pcp0_direct},
  {"pcp0-indirect", pcp0_indirect},
  {"pcp64-direct", pcp64_direct},
  {"pcp64-indirect", pcp64_indirect},
  {"pcp256-direct", pcp256_direct},
  {"pcp256-indirect", pcp256_indirect},
  {"pcp257-direct", pcp257_direct},
  {"pcpmax-direct", pcpmax_direct},
  {"static-pcp0", static_pcp0},
};

int main(int argc, char **argv) {
  if (argc > 2 && !strcmp(argv[2], "--child")) {
    alarm(30);  // a hung submission is a result too (SIGALRM)
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
