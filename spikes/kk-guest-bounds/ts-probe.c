// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// A minimal KosmicKrisp vehicle for the piglit ext_timer_query-time-elapsed GPU hang
// (docs/hardening-backlog.md). That test brackets a draw with two GL_TIME_ELAPSED timestamps and
// hangs a Metal command buffer on the vrend tier (vrend -> zink -> KK). This isolates the KK half:
// it drives KK's VK_QUERY_TYPE_TIMESTAMP path directly -- two vkCmdWriteTimestamp2 at different
// stages inside a dynamic-rendering pass (the render-encoder write path), resolved into the pool
// BO, read back with VK_QUERY_RESULT_WAIT_BIT. If KK's timestamp path is what wedges, the fence
// never signals: the wait returns VK_TIMEOUT or VK_ERROR_DEVICE_LOST. Valid, increasing timestamps
// mean the hang lives above KK (in how zink/vrend drives it), not here.
//
// This SUBMITS GPU work and a hang is host-wide, so run it only on an idle host and watch the
// kernel oracle alongside:
//   log show --last 2m --predicate 'process == "kernel" AND eventMessage CONTAINS "GPURestart"'
//
// Cases: ts-render (one submit), ts-render-loop (400 submits, matching the real test's repetition).
//
// Measured 2026-10-10 on KK 564598b99be: both PASS, no watchdog hang, no kernel GPURestart. KK's
// direct Vulkan timestamp path (render-encoder write, counter-heap resolve, WAIT_BIT readback) is
// therefore NOT the ext_timer_query hang; the trigger lives above KK (zink/vrend) and/or on the
// timeline-semaphore / shared-event sync path this fence-only vehicle does not drive.
//
// Build:
//   cc -Wall -I/opt/homebrew/include ts-probe.c -L/opt/homebrew/lib -lvulkan -o tsp
//   VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./tsp [case]
// Each case re-executes the binary: a bare fork() cannot reach Metal's XPC services.
// ts_vert_spv.h / ts_frag_spv.h are a fullscreen-triangle vertex shader and a constant-color
// fragment shader, generated once with glslangValidator -V --target-env vulkan1.3.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/wait.h>
#include <unistd.h>

#include "ts_vert_spv.h"
#include "ts_frag_spv.h"

static VkInstance inst;
static VkPhysicalDevice pd;
static VkDevice dev;
static VkQueue q;
static uint32_t qfam;
static VkCommandPool cpool;

static uint32_t mem_type(uint32_t bits, VkMemoryPropertyFlags want) {
  VkPhysicalDeviceMemoryProperties mp;
  vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
      return i;
  fprintf(stderr, "no memory type\n");
  exit(2);
}

static void setup(void) {
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3};
  const char *ie[] = {"VK_KHR_portability_enumeration"};
  VkInstanceCreateInfo ic = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
                             .flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
                             .enabledExtensionCount = 1, .ppEnabledExtensionNames = ie};
  if (vkCreateInstance(&ic, NULL, &inst)) { fprintf(stderr, "instance\n"); exit(2); }
  uint32_t n = 1;
  vkEnumeratePhysicalDevices(inst, &n, &pd);
  if (!n) { fprintf(stderr, "no device\n"); exit(2); }

  // Queue family with timestamp support (KK advertises 64 valid bits on its queues).
  uint32_t nf = 0;
  vkGetPhysicalDeviceQueueFamilyProperties(pd, &nf, NULL);
  VkQueueFamilyProperties fp[8];
  if (nf > 8) nf = 8;
  vkGetPhysicalDeviceQueueFamilyProperties(pd, &nf, fp);
  qfam = 0;
  for (uint32_t i = 0; i < nf; i++)
    if ((fp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) && fp[i].timestampValidBits) { qfam = i; break; }

  float pr = 1;
  VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                 .queueFamilyIndex = qfam, .queueCount = 1, .pQueuePriorities = &pr};
  VkPhysicalDeviceVulkan13Features f13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
                                          .dynamicRendering = VK_TRUE, .synchronization2 = VK_TRUE};
  VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f13,
                           .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci};
  if (vkCreateDevice(pd, &dc, NULL, &dev)) { fprintf(stderr, "device\n"); exit(2); }
  vkGetDeviceQueue(dev, qfam, 0, &q);
  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = qfam};
  vkCreateCommandPool(dev, &cpi, NULL, &cpool);
}

// A 64x64 color target we can clear inside a render pass, for GPU work to time.
static void make_target(VkImage *img_out, VkImageView *view_out) {
  VkImageCreateInfo ii = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
                          .format = VK_FORMAT_R8G8B8A8_UNORM, .extent = {64, 64, 1}, .mipLevels = 1,
                          .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
                          .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED};
  VkImage img;
  if (vkCreateImage(dev, &ii, NULL, &img)) { fprintf(stderr, "image\n"); exit(2); }
  VkMemoryRequirements r;
  vkGetImageMemoryRequirements(dev, img, &r);
  VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = r.size,
                             .memoryTypeIndex = mem_type(r.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)};
  VkDeviceMemory m;
  vkAllocateMemory(dev, &ai, NULL, &m);
  vkBindImageMemory(dev, img, m, 0);
  VkImageViewCreateInfo vi = {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = img,
                              .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
                              .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  VkImageView view;
  vkCreateImageView(dev, &vi, NULL, &view);
  *img_out = img;
  *view_out = view;
}

static VkShaderModule shader(const uint32_t *code, size_t bytes) {
  VkShaderModuleCreateInfo si = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                 .codeSize = bytes, .pCode = code};
  VkShaderModule m;
  if (vkCreateShaderModule(dev, &si, NULL, &m)) { fprintf(stderr, "shader\n"); exit(2); }
  return m;
}

// A trivial fullscreen-triangle pipeline (no vertex inputs), rendering into an R8G8B8A8 target via
// dynamic rendering -- the real fragment work GL_TIME_ELAPSED measures.
static VkPipeline make_pipeline(void) {
  VkShaderModule vs = shader(tri_vert_spv, sizeof(tri_vert_spv));
  VkShaderModule fs = shader(tri_frag_spv, sizeof(tri_frag_spv));
  VkPipelineShaderStageCreateInfo stages[2] = {
    {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT,
     .module = vs, .pName = "main"},
    {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
     .module = fs, .pName = "main"},
  };
  VkPipelineVertexInputStateCreateInfo vin = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo ia = {.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                                               .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
  VkViewport vp = {0, 0, 64, 64, 0, 1};
  VkRect2D sc = {{0, 0}, {64, 64}};
  VkPipelineViewportStateCreateInfo vps = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                                           .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc};
  VkPipelineRasterizationStateCreateInfo rs = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                                               .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
                                               .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE, .lineWidth = 1.0f};
  VkPipelineMultisampleStateCreateInfo ms = {.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                                             .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
  VkPipelineColorBlendAttachmentState cba = {.colorWriteMask = 0xf};
  VkPipelineColorBlendStateCreateInfo cb = {.sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
                                            .attachmentCount = 1, .pAttachments = &cba};
  VkPipelineLayoutCreateInfo pli = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
  VkPipelineLayout pl;
  vkCreatePipelineLayout(dev, &pli, NULL, &pl);
  VkFormat fmt = VK_FORMAT_R8G8B8A8_UNORM;
  VkPipelineRenderingCreateInfo prc = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
                                       .colorAttachmentCount = 1, .pColorAttachmentFormats = &fmt};
  VkGraphicsPipelineCreateInfo gp = {.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &prc,
                                     .stageCount = 2, .pStages = stages, .pVertexInputState = &vin,
                                     .pInputAssemblyState = &ia, .pViewportState = &vps, .pRasterizationState = &rs,
                                     .pMultisampleState = &ms, .pColorBlendState = &cb, .layout = pl};
  VkPipeline pipe;
  if (vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gp, NULL, &pipe)) { fprintf(stderr, "pipeline\n"); exit(2); }
  return pipe;
}

// Bracket a render-pass draw with two timestamps at different stages (the render-encoder write
// path) for `iters` separate submissions, reading each back with WAIT_BIT. Resources are built
// once; each iteration re-records, submits, waits, and resolves, the way a GL app repeating a
// GL_TIME_ELAPSED timing does. Returns 1 if every iteration completed with increasing timestamps
// (no hang), 0 at the first VK_TIMEOUT / DEVICE_LOST / wrong value (reproduced).
static int ts_render_n(int iters) {
  VkImage img;
  VkImageView view;
  make_target(&img, &view);

  VkQueryPoolCreateInfo qpi = {.sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
                               .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = 2};
  VkQueryPool qp;
  if (vkCreateQueryPool(dev, &qpi, NULL, &qp)) { fprintf(stderr, "query pool\n"); return 0; }

  VkPipeline pipe = make_pipeline();
  VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                      .commandPool = cpool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                      .commandBufferCount = 1};
  VkFenceCreateInfo fi = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};

  for (int it = 0; it < iters; it++) {
    VkCommandBuffer cb;
    vkAllocateCommandBuffers(dev, &cbai, &cb);
    VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    vkBeginCommandBuffer(cb, &bi);

    vkCmdResetQueryPool(cb, qp, 0, 2);

    // UNDEFINED -> COLOR_ATTACHMENT_OPTIMAL (loadOp CLEAR discards prior contents, so this is
    // legal every iteration).
    VkImageMemoryBarrier2 imb = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER_2,
                                 .srcStageMask = VK_PIPELINE_STAGE_2_TOP_OF_PIPE_BIT,
                                 .dstStageMask = VK_PIPELINE_STAGE_2_COLOR_ATTACHMENT_OUTPUT_BIT,
                                 .dstAccessMask = VK_ACCESS_2_COLOR_ATTACHMENT_WRITE_BIT,
                                 .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                 .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .image = img,
                                 .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    VkDependencyInfo di = {.sType = VK_STRUCTURE_TYPE_DEPENDENCY_INFO, .imageMemoryBarrierCount = 1,
                           .pImageMemoryBarriers = &imb};
    vkCmdPipelineBarrier2(cb, &di);

    VkRenderingAttachmentInfo cat = {.sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = view,
                                     .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                                     .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                     .clearValue = {.color = {.float32 = {0.2f, 0.4f, 0.6f, 1.0f}}}};
    VkRenderingInfo ri = {.sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
                          .renderArea = {{0, 0}, {64, 64}}, .layerCount = 1,
                          .colorAttachmentCount = 1, .pColorAttachments = &cat};

    vkCmdBeginRendering(cb, &ri);
    // Begin timestamp before the draw, draw a triangle (real fragment work), end after color
    // output -- two distinct Metal stages, mirroring GL_TIME_ELAPSED around a draw.
    vkCmdWriteTimestamp2(cb, VK_PIPELINE_STAGE_2_TOP_OF_PIPE_BIT, qp, 0);
    vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
    vkCmdDraw(cb, 3, 1, 0, 0);
    vkCmdWriteTimestamp2(cb, VK_PIPELINE_STAGE_2_COLOR_ATTACHMENT_OUTPUT_BIT, qp, 1);
    vkCmdEndRendering(cb);

    vkEndCommandBuffer(cb);

    VkFence fence;
    vkCreateFence(dev, &fi, NULL, &fence);
    VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
    if (vkQueueSubmit(q, 1, &si, fence)) { fprintf(stderr, "submit (iter %d)\n", it); return 0; }

    VkResult w = vkWaitForFences(dev, 1, &fence, VK_TRUE, 3000000000ull); // 3s
    if (w != VK_SUCCESS) {
      printf("  iter %d: fence wait -> %d (VK_TIMEOUT=%d DEVICE_LOST=%d) -- HANG\n", it, w, VK_TIMEOUT,
             VK_ERROR_DEVICE_LOST);
      return 0;
    }

    uint64_t ts[2] = {0, 0};
    VkResult g = vkGetQueryPoolResults(dev, qp, 0, 2, sizeof(ts), ts, sizeof(uint64_t),
                                       VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT);
    if (g != VK_SUCCESS || ts[1] < ts[0] || ts[1] == 0) {
      printf("  iter %d: getresults -> %d  t0=%llu t1=%llu -- BAD\n", it, g,
             (unsigned long long)ts[0], (unsigned long long)ts[1]);
      return 0;
    }
    if (it == 0 || it == iters - 1)
      printf("  iter %d: t0=%llu t1=%llu elapsed=%lld\n", it, (unsigned long long)ts[0],
             (unsigned long long)ts[1], (long long)(ts[1] - ts[0]));

    vkFreeCommandBuffers(dev, cpool, 1, &cb);
    vkDestroyFence(dev, fence, NULL);
  }
  printf("  %d iterations completed, no hang\n", iters);
  return 1;
}

static int ts_render(void) { return ts_render_n(1); }
static int ts_render_loop(void) { return ts_render_n(400); }

// Closer to zink's GL_TIME_ELAPSED pattern than ts_render_n: a persistent timestamp pool whose
// query index CLIMBS each measurement (a fresh counter-heap slot per frame, not a reused pair),
// and a COMPUTE-path begin timestamp (written before the render pass, so KK takes the
// no-render-encoder branch that stamps LIBKK_QUERY_UNAVAILABLE + a compute write) followed by a
// render-path end timestamp. Readback is still CPU-side WAIT_BIT (the GPU-side
// vkCmdCopyQueryPoolResults zink uses needs VK_KHR_copy_memory_indirect, absent from the host SDK
// headers; tested separately if this does not reproduce). Returns 1 if every measurement completed
// (no hang), 0 at the first timeout / device loss / bad value.
static int ts_zink(void) {
  enum { POOL = 256, ITERS = 120 };
  VkImage img;
  VkImageView view;
  make_target(&img, &view);
  VkPipeline pipe = make_pipeline();

  VkQueryPoolCreateInfo qpi = {.sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
                               .queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = POOL};
  VkQueryPool qp;
  if (vkCreateQueryPool(dev, &qpi, NULL, &qp)) { fprintf(stderr, "query pool\n"); return 0; }

  VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                      .commandPool = cpool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                      .commandBufferCount = 1};
  VkFenceCreateInfo fi = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};

  for (int it = 0; it < ITERS; it++) {
    uint32_t qb = (uint32_t)(2 * it) % POOL;      // begin index, climbing
    uint32_t qe = (uint32_t)(2 * it + 1) % POOL;  // end index, climbing

    VkCommandBuffer cb;
    vkAllocateCommandBuffers(dev, &cbai, &cb);
    VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    vkBeginCommandBuffer(cb, &bi);

    vkCmdResetQueryPool(cb, qp, qb, 1);
    vkCmdResetQueryPool(cb, qp, qe, 1);

    // Begin timestamp with NO render pass active -> KK compute path.
    vkCmdWriteTimestamp2(cb, VK_PIPELINE_STAGE_2_TOP_OF_PIPE_BIT, qp, qb);

    VkImageMemoryBarrier2 imb = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER_2,
                                 .srcStageMask = VK_PIPELINE_STAGE_2_TOP_OF_PIPE_BIT,
                                 .dstStageMask = VK_PIPELINE_STAGE_2_COLOR_ATTACHMENT_OUTPUT_BIT,
                                 .dstAccessMask = VK_ACCESS_2_COLOR_ATTACHMENT_WRITE_BIT,
                                 .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                 .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .image = img,
                                 .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    VkDependencyInfo di = {.sType = VK_STRUCTURE_TYPE_DEPENDENCY_INFO, .imageMemoryBarrierCount = 1,
                           .pImageMemoryBarriers = &imb};
    vkCmdPipelineBarrier2(cb, &di);

    VkRenderingAttachmentInfo cat = {.sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = view,
                                     .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                                     .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                     .clearValue = {.color = {.float32 = {0.2f, 0.4f, 0.6f, 1.0f}}}};
    VkRenderingInfo ri = {.sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = {{0, 0}, {64, 64}},
                          .layerCount = 1, .colorAttachmentCount = 1, .pColorAttachments = &cat};
    vkCmdBeginRendering(cb, &ri);
    vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
    vkCmdDraw(cb, 3, 1, 0, 0);
    // End timestamp inside the render pass -> KK render-encoder path.
    vkCmdWriteTimestamp2(cb, VK_PIPELINE_STAGE_2_COLOR_ATTACHMENT_OUTPUT_BIT, qp, qe);
    vkCmdEndRendering(cb);
    vkEndCommandBuffer(cb);

    VkFence fence;
    vkCreateFence(dev, &fi, NULL, &fence);
    VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
    if (vkQueueSubmit(q, 1, &si, fence)) { fprintf(stderr, "submit (iter %d)\n", it); return 0; }
    VkResult w = vkWaitForFences(dev, 1, &fence, VK_TRUE, 3000000000ull);
    if (w != VK_SUCCESS) {
      printf("  iter %d (qb=%u qe=%u): fence wait -> %d -- HANG\n", it, qb, qe, w);
      return 0;
    }
    uint64_t ts[2] = {0, 0};
    uint64_t qr[2];
    VkResult g = vkGetQueryPoolResults(dev, qp, qb, 1, sizeof(uint64_t), &qr[0], sizeof(uint64_t),
                                       VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT);
    VkResult g2 = vkGetQueryPoolResults(dev, qp, qe, 1, sizeof(uint64_t), &qr[1], sizeof(uint64_t),
                                        VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT);
    ts[0] = qr[0];
    ts[1] = qr[1];
    if (g != VK_SUCCESS || g2 != VK_SUCCESS || ts[1] < ts[0] || ts[1] == 0) {
      printf("  iter %d (qb=%u qe=%u): getresults %d/%d t0=%llu t1=%llu -- BAD\n", it, qb, qe, g, g2,
             (unsigned long long)ts[0], (unsigned long long)ts[1]);
      return 0;
    }
    vkFreeCommandBuffers(dev, cpool, 1, &cb);
    vkDestroyFence(dev, fence, NULL);
  }
  printf("  %d measurements (climbing index, compute-path begin) completed, no hang\n", ITERS);
  return 1;
}

static const struct { const char *name; int (*fn)(void); } tests[] = {
  {"ts-render", ts_render},
  {"ts-render-loop", ts_render_loop},
  {"ts-zink", ts_zink},
};

int main(int argc, char **argv) {
  if (argc > 2 && !strcmp(argv[2], "--child")) {
    for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++)
      if (!strcmp(argv[1], tests[i].name)) { setup(); return tests[i].fn() == 1 ? 0 : 1; }
    return 2;
  }
  int fails = 0;
  for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
    if (argc > 1 && strcmp(argv[1], tests[i].name)) continue;
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) { execl(argv[0], argv[0], tests[i].name, "--child", (char *)NULL); _exit(3); }
    int st;
    waitpid(p, &st, 0);
    const char *v = WIFSIGNALED(st) ? "CRASH" : WEXITSTATUS(st) == 0 ? "PASS" : "FAIL";
    if (WIFSIGNALED(st)) printf("  (signal %d)\n", WTERMSIG(st));
    printf("%-14s %s\n", tests[i].name, v);
    fails += strcmp(v, "PASS") != 0;
  }
  return fails != 0;
}
