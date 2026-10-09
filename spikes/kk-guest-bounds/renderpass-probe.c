// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Render-pass lifecycle a venus guest can drive on KosmicKrisp (companion to probe.c). An
// attachment-less dynamic-rendering pass starts its Metal encoder lazily (deferred to the first
// draw), so between vkCmdBeginRendering and that first draw KK holds a pending-start flag. A
// guest can end the pass before any draw: vkCmdEndRendering frees the Metal render-pass
// descriptor but the pending-start flag is separate state and survived, so the next draw (one
// the guest issues with no pass active) saw the flag set and tried to start a pass from the
// freed, NULL descriptor -- a host crash.
//
// Each case runs in its own process; PASS = KK draws or drops the work cleanly, CRASH = it does
// not. `noattach-draw` is the control: a real draw inside an attachment-less pass must still
// render (i.e. start the pass from the live descriptor). `noattach-end-draw` ends the pass with
// no draw, then draws with no pass active; KK must drop that stray draw, not start a pass from a
// freed descriptor.
//
// The pipeline rasterizes nothing (rasterizerDiscardEnable), so no attachment, image or readback
// is needed; the draw still takes the render encoder, which is what exercises the lazy start.
//
// Build (the SPIR-V vertex header comes from tess/gen.sh):
//   cc -Wall -I/opt/homebrew/include renderpass-probe.c -L/opt/homebrew/lib -lvulkan -o rp
//   VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./rp [case]
// Each case re-executes the binary: a bare fork() cannot reach Metal's XPC services.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/wait.h>
#include <unistd.h>

#include "tess/tess_vert.h"

enum { W = 16, H = 16 };

static VkInstance inst; static VkPhysicalDevice pd; static VkDevice dev; static VkQueue q;
static VkCommandPool cpool;
static VkPipelineLayout layout;
static VkPipeline gpipe;

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
  VkPhysicalDeviceVulkan13Features f13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE};
  VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f13, .queueCreateInfoCount = 1,
                           .pQueueCreateInfos = &qci};
  if (vkCreateDevice(pd, &dc, NULL, &dev)) { fprintf(stderr, "device\n"); exit(2); }
  vkGetDeviceQueue(dev, 0, 0, &q);
  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  vkCreateCommandPool(dev, &cpi, NULL, &cpool);
  VkPipelineLayoutCreateInfo li = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
  vkCreatePipelineLayout(dev, &li, NULL, &layout);

  VkShaderModuleCreateInfo smi = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                  .codeSize = sizeof(tess_vert), .pCode = tess_vert};
  VkShaderModule vs; vkCreateShaderModule(dev, &smi, NULL, &vs);
  // Vertex-only, rasterization discarded: no fragment stage, no colour attachment.
  VkPipelineShaderStageCreateInfo st = {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, NULL, 0,
                                        VK_SHADER_STAGE_VERTEX_BIT, vs, "main", NULL};
  VkPipelineVertexInputStateCreateInfo vin = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo ia = {.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                                               .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
  VkPipelineViewportStateCreateInfo vp = {.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                                          .viewportCount = 1, .scissorCount = 1};
  VkPipelineRasterizationStateCreateInfo rs = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                                               .rasterizerDiscardEnable = VK_TRUE, .cullMode = VK_CULL_MODE_NONE, .lineWidth = 1};
  VkPipelineMultisampleStateCreateInfo ms = {.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                                             .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
  VkDynamicState ds[] = {VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR};
  VkPipelineDynamicStateCreateInfo dyn = {.sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
                                          .dynamicStateCount = 2, .pDynamicStates = ds};
  VkPipelineRenderingCreateInfo ri = {.sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO};
  VkGraphicsPipelineCreateInfo gi = {.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &ri,
    .stageCount = 1, .pStages = &st, .pVertexInputState = &vin, .pInputAssemblyState = &ia,
    .pViewportState = &vp, .pRasterizationState = &rs, .pMultisampleState = &ms,
    .pDynamicState = &dyn, .layout = layout};
  if (vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gi, NULL, &gpipe)) { fprintf(stderr, "pipeline\n"); exit(2); }
}

static VkCommandBuffer begin_cmd(void) {
  VkCommandBufferAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool,
                                    .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
  VkCommandBuffer cb; vkAllocateCommandBuffers(dev, &ai, &cb);
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  vkBeginCommandBuffer(cb, &bi);
  return cb;
}

static void begin_noattach_pass(VkCommandBuffer cb) {
  VkRenderingInfo ri = {.sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = {{0, 0}, {W, H}}, .layerCount = 1};
  vkCmdBeginRendering(cb, &ri);
}

static void set_vp(VkCommandBuffer cb) {
  VkViewport v = {0, 0, W, H, 0, 1}; vkCmdSetViewport(cb, 0, 1, &v);
  VkRect2D s = {{0, 0}, {W, H}}; vkCmdSetScissor(cb, 0, 1, &s);
}

static int submit(VkCommandBuffer cb) {
  if (vkEndCommandBuffer(cb)) return 0;
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
  if (vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE)) return 0;
  return vkQueueWaitIdle(q) == VK_SUCCESS;
}

// Control: a draw inside an attachment-less pass. The lazy start must fire from the live
// descriptor, so this exercises the same path the bug's stray draw hits, but with a valid pass.
static int noattach_draw(void) {
  VkCommandBuffer cb = begin_cmd();
  vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, gpipe);
  begin_noattach_pass(cb);
  set_vp(cb);
  vkCmdDraw(cb, 3, 1, 0, 0);
  vkCmdEndRendering(cb);
  if (!submit(cb)) { printf("  submit failed\n"); return 0; }
  return 1;
}

// The bug: end the attachment-less pass with no draw, then draw with no pass active. KK must
// drop the stray draw, not start a pass from the freed descriptor.
static int noattach_end_draw(void) {
  VkCommandBuffer cb = begin_cmd();
  vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, gpipe);
  begin_noattach_pass(cb);
  vkCmdEndRendering(cb);
  set_vp(cb);
  vkCmdDraw(cb, 3, 1, 0, 0);
  if (!submit(cb)) { printf("  submit failed\n"); return 0; }
  return 1;
}

static const struct { const char *name; int (*fn)(void); } tests[] = {
  {"noattach-draw", noattach_draw},
  {"noattach-end-draw", noattach_end_draw},
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
