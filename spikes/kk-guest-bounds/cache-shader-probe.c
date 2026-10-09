// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Guest-reachable inputs to KosmicKrisp that the venus host must refuse or survive, via the
// pipeline-cache + dynamic-rendering paths (companion to probe.c, which covers descriptors and
// images). Each case runs in a forked child; PASS = KK refuses/survives cleanly, CRASH/FAIL = vuln.
//
//  - A venus guest's VkPipelineCache pInitialData is forwarded unchanged. KK registers no
//    pipeline_cache_import_ops, so each entry becomes a raw-data cache object under the guest's
//    key; at the next matching pipeline creation vk_pipeline_cache_lookup_object re-deserializes
//    it through the shader ops (-> kk_deserialize_shader), with no header/BLAKE3 on that path. We
//    round-trip a real compute pipeline to obtain a valid blob + the shader's key, poison the
//    shader entry's serialized body, re-seed a new cache with it, and recreate the pipeline.
//  - kk_CmdBeginRendering forwards guest renderArea/layerCount into the Metal pass.
//
// Build (self-contained; the SPIR-V below is a precompiled "b.v=b.v+1" compute shader):
//   cc -Wall -I/opt/homebrew/include cache-shader-probe.c -L/opt/homebrew/lib -lvulkan -o csp
//   VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./csp [case]
// Each case re-executes the binary: a bare fork() cannot reach Metal's XPC services.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/wait.h>
#include <unistd.h>

static const unsigned char SPV[] = {
  0x03, 0x02, 0x23, 0x07, 0x00, 0x00, 0x01, 0x00, 0x0b, 0x00, 0x08, 0x00,
  0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x00, 0x02, 0x00,
  0x01, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x06, 0x00, 0x01, 0x00, 0x00, 0x00,
  0x47, 0x4c, 0x53, 0x4c, 0x2e, 0x73, 0x74, 0x64, 0x2e, 0x34, 0x35, 0x30,
  0x00, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
  0x01, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x05, 0x00, 0x05, 0x00, 0x00, 0x00,
  0x04, 0x00, 0x00, 0x00, 0x6d, 0x61, 0x69, 0x6e, 0x00, 0x00, 0x00, 0x00,
  0x10, 0x00, 0x06, 0x00, 0x04, 0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00,
  0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
  0x03, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x00, 0xc2, 0x01, 0x00, 0x00,
  0x05, 0x00, 0x04, 0x00, 0x04, 0x00, 0x00, 0x00, 0x6d, 0x61, 0x69, 0x6e,
  0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x03, 0x00, 0x07, 0x00, 0x00, 0x00,
  0x42, 0x00, 0x00, 0x00, 0x06, 0x00, 0x04, 0x00, 0x07, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00, 0x76, 0x00, 0x00, 0x00, 0x05, 0x00, 0x03, 0x00,
  0x09, 0x00, 0x00, 0x00, 0x62, 0x00, 0x00, 0x00, 0x47, 0x00, 0x03, 0x00,
  0x07, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x48, 0x00, 0x05, 0x00,
  0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x23, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00, 0x47, 0x00, 0x04, 0x00, 0x09, 0x00, 0x00, 0x00,
  0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x47, 0x00, 0x04, 0x00,
  0x09, 0x00, 0x00, 0x00, 0x22, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
  0x47, 0x00, 0x04, 0x00, 0x13, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00,
  0x19, 0x00, 0x00, 0x00, 0x13, 0x00, 0x02, 0x00, 0x02, 0x00, 0x00, 0x00,
  0x21, 0x00, 0x03, 0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
  0x15, 0x00, 0x04, 0x00, 0x06, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x03, 0x00, 0x07, 0x00, 0x00, 0x00,
  0x06, 0x00, 0x00, 0x00, 0x20, 0x00, 0x04, 0x00, 0x08, 0x00, 0x00, 0x00,
  0x02, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x3b, 0x00, 0x04, 0x00,
  0x08, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
  0x15, 0x00, 0x04, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00,
  0x01, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x04, 0x00, 0x0a, 0x00, 0x00, 0x00,
  0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x04, 0x00,
  0x0c, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00,
  0x2b, 0x00, 0x04, 0x00, 0x06, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00,
  0x01, 0x00, 0x00, 0x00, 0x17, 0x00, 0x04, 0x00, 0x12, 0x00, 0x00, 0x00,
  0x06, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x06, 0x00,
  0x12, 0x00, 0x00, 0x00, 0x13, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00,
  0x0f, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x36, 0x00, 0x05, 0x00,
  0x02, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
  0x03, 0x00, 0x00, 0x00, 0xf8, 0x00, 0x02, 0x00, 0x05, 0x00, 0x00, 0x00,
  0x41, 0x00, 0x05, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00,
  0x09, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x3d, 0x00, 0x04, 0x00,
  0x06, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00,
  0x80, 0x00, 0x05, 0x00, 0x06, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00,
  0x0e, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x41, 0x00, 0x05, 0x00,
  0x0c, 0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00,
  0x0b, 0x00, 0x00, 0x00, 0x3e, 0x00, 0x03, 0x00, 0x11, 0x00, 0x00, 0x00,
  0x10, 0x00, 0x00, 0x00, 0xfd, 0x00, 0x01, 0x00, 0x38, 0x00, 0x01, 0x00,
};

static VkInstance inst; static VkPhysicalDevice pd; static VkDevice dev;
static VkDescriptorSetLayout dsl; static VkPipelineLayout pl; static VkShaderModule sm;

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
  VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &pr};
  VkPhysicalDeviceVulkan13Features f13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE};
  VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f13, .queueCreateInfoCount = 1, .pQueueCreateInfos = &q};
  if (vkCreateDevice(pd, &dc, NULL, &dev)) { fprintf(stderr, "device\n"); exit(2); }
  VkDescriptorSetLayoutBinding b = {.binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT};
  VkDescriptorSetLayoutCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &b};
  vkCreateDescriptorSetLayout(dev, &dci, NULL, &dsl);
  VkPipelineLayoutCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &dsl};
  vkCreatePipelineLayout(dev, &pci, NULL, &pl);
  VkShaderModuleCreateInfo smci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = sizeof(SPV), .pCode = (const uint32_t *)SPV};
  if (vkCreateShaderModule(dev, &smci, NULL, &sm)) { fprintf(stderr, "module\n"); exit(2); }
}

static VkResult make_pipeline(VkPipelineCache cache) {
  VkComputePipelineCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, .layout = pl,
    .stage = {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = sm, .pName = "main"}};
  VkPipeline p = VK_NULL_HANDLE; VkResult r = vkCreateComputePipelines(dev, cache, 1, &ci, NULL, &p);
  if (p) vkDestroyPipeline(dev, p, NULL); return r;
}

static unsigned char *get_cache_data(size_t *out) {
  VkPipelineCacheCreateInfo cci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO};
  VkPipelineCache c; vkCreatePipelineCache(dev, &cci, NULL, &c);
  if (make_pipeline(c) != VK_SUCCESS) { fprintf(stderr, "warmup pipeline failed\n"); exit(2); }
  size_t sz = 0; vkGetPipelineCacheData(dev, c, &sz, NULL);
  unsigned char *d = malloc(sz); vkGetPipelineCacheData(dev, c, &sz, d);
  vkDestroyPipelineCache(dev, c, NULL); *out = sz; return d;
}

// Locate the kk final-shader entry's data in the cache blob. Layout:
//   header(32) + count(u32) + entries{ i32 type; u32 key_size; u32 data_size; key[]; align8; data[]; }
// The kk entry's MSL part is [ep_len u32][code_len u32][entrypoint][code]; KK renames the entrypoint
// to "main_entrypoint" (ep_len=16). Return the code_len field's offset within the entry data.
static int find_shader_entry(unsigned char *d, size_t sz, size_t *data_off, size_t *data_sz) {
  if (sz < 36) return -1;
  uint32_t count; memcpy(&count, d + 32, 4); size_t p = 36;
  for (uint32_t i = 0; i < count && p + 12 <= sz; i++) {
    uint32_t key_size, data_size; int32_t type;
    memcpy(&type, d + p, 4); memcpy(&key_size, d + p + 4, 4); memcpy(&data_size, d + p + 8, 4);
    p += 12; p += key_size; p = (p + 7) & ~(size_t)7; if (p + data_size > sz) return -1;
    static const char ENT[] = "main_entrypoint";
    for (size_t j = p; j + 8 + sizeof(ENT) <= p + data_size; j++) {
      uint32_t ep; memcpy(&ep, d + j, 4);
      if (ep == sizeof(ENT) && memcmp(d + j + 8, ENT, sizeof(ENT)) == 0) {
        *data_off = p; *data_sz = data_size; return (int)(j + 4 - p);
      }
    }
    p += data_size;
  }
  return -1;
}

// 1 = pass (refused/survived cleanly), other = fail; a child crash is caught by the parent.
static int cache_case(const char *c) {
  size_t sz; unsigned char *blob = get_cache_data(&sz);
  size_t doff, dsz; int codelen_rel = find_shader_entry(blob, sz, &doff, &dsz);
  if (codelen_rel < 0) { fprintf(stderr, "shader entry not found (sz=%zu)\n", sz); return 2; }
  unsigned char *data = blob + doff;
  if (!strcmp(c, "cache-control")) { /* unmodified */ }
  else if (!strcmp(c, "cache-huge-code-len")) { uint32_t big = 64u * 1024u * 1024u; memcpy(data + codelen_rel, &big, 4); }
  else if (!strcmp(c, "cache-bad-stage")) { uint32_t s = 0x08000000u; memcpy(data + 0, &s, 4); } // msl_data[stage] write ~2GB OOB
  else if (!strcmp(c, "cache-no-nul")) { data[dsz - 1] = 0x41; }                                   // clobber trailing NUL of code
  else return 2;
  VkPipelineCacheCreateInfo cci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO, .initialDataSize = sz, .pInitialData = blob};
  VkPipelineCache poisoned; vkCreatePipelineCache(dev, &cci, NULL, &poisoned);
  VkResult r = make_pipeline(poisoned);
  printf("  %s: recreate pipeline = %d\n", c, r);
  // control must create; attack cases may create (recompiled) or fail cleanly, both PASS; a crash fails.
  if (!strcmp(c, "cache-control")) return r == VK_SUCCESS ? 1 : 0;
  return 1;
}

// Begin dynamic rendering with out-of-advertised-range renderArea/layerCount; KK must refuse
// (the error surfaces at vkEndCommandBuffer). PASS = refused.
static int render_case(const char *c) {
  VkCommandPoolCreateInfo cpci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  VkCommandPool cp; vkCreateCommandPool(dev, &cpci, NULL, &cp);
  VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cp, .commandBufferCount = 1};
  VkCommandBuffer cb; vkAllocateCommandBuffers(dev, &cbai, &cb);
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO}; vkBeginCommandBuffer(cb, &bi);
  VkRect2D area = {{0, 0}, {16, 16}}; uint32_t layers = 1;
  if (!strcmp(c, "render-huge-area")) area.extent = (VkExtent2D){0x20000000u, 0x20000000u};
  else if (!strcmp(c, "render-huge-layers")) layers = 100000;
  else if (!strcmp(c, "render-ok")) { /* in-range: must be accepted */ }
  else return 2;
  VkRenderingInfo ri = {.sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = area, .layerCount = layers, .colorAttachmentCount = 0};
  vkCmdBeginRendering(cb, &ri); vkCmdEndRendering(cb);
  VkResult er = vkEndCommandBuffer(cb);
  printf("  %s: endCommandBuffer = %d\n", c, er);
  if (!strcmp(c, "render-ok")) return er == VK_SUCCESS ? 1 : 0;   // in-range must pass
  return er != VK_SUCCESS ? 1 : 0;                                // out-of-range must be refused
}

struct test { const char *name; int (*fn)(const char *); };
static const struct test tests[] = {
  {"cache-control", cache_case}, {"cache-huge-code-len", cache_case},
  {"cache-bad-stage", cache_case}, {"cache-no-nul", cache_case},
  {"render-ok", render_case}, {"render-huge-area", render_case}, {"render-huge-layers", render_case},
};

int main(int argc, char **argv) {
  if (argc > 2 && !strcmp(argv[2], "--child")) {
    for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++)
      if (!strcmp(argv[1], tests[i].name)) { setup(); return tests[i].fn(argv[1]) == 1 ? 0 : 1; }
    return 2;
  }
  int fails = 0;
  for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
    if (argc > 1 && strcmp(argv[1], tests[i].name)) continue;
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) { execl(argv[0], argv[0], tests[i].name, "--child", (char *)NULL); _exit(3); }
    int st; waitpid(p, &st, 0);
    const char *v = WIFSIGNALED(st) ? "CRASH" : WEXITSTATUS(st) == 0 ? "PASS" : "FAIL";
    if (WIFSIGNALED(st)) printf("  (signal %d)\n", WTERMSIG(st));
    printf("%-22s %s\n", tests[i].name, v);
    fails += strcmp(v, "PASS") != 0;
  }
  return fails != 0;
}
