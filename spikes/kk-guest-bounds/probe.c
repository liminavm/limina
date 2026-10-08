// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Invalid-usage inputs a venus guest can hand KosmicKrisp, which KK must refuse or survive. Each case runs in a forked child, so a crash
// or an assert is that case's result. PASS = KK refuses (or survives) the input cleanly.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static VkInstance inst;
static VkPhysicalDevice pd;
static VkDevice dev;

static void setup(void) {
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3};
  const char *iexts[] = {"VK_KHR_portability_enumeration"};
  VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
                              .flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
                              .enabledExtensionCount = 1, .ppEnabledExtensionNames = iexts};
  if (vkCreateInstance(&ici, NULL, &inst)) { fprintf(stderr, "instance\n"); exit(2); }
  uint32_t n = 1;
  vkEnumeratePhysicalDevices(inst, &n, &pd);
  if (!n) { fprintf(stderr, "no device\n"); exit(2); }
  float prio = 1;
  VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio};
  const char *dexts[] = {"VK_KHR_push_descriptor"};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &q,
                            .enabledExtensionCount = 1, .ppEnabledExtensionNames = dexts};
  VkResult r = vkCreateDevice(pd, &dci, NULL, &dev);
  if (r) { fprintf(stderr, "device: %d\n", r); exit(2); }
}

static VkResult layout(uint32_t binding, VkDescriptorType type, uint32_t count, VkDescriptorSetLayout *out) {
  VkDescriptorSetLayoutBinding b = {.binding = binding, .descriptorType = type, .descriptorCount = count,
                                    .stageFlags = VK_SHADER_STAGE_ALL};
  VkDescriptorSetLayoutCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &b};
  VkDescriptorSetLayout l = VK_NULL_HANDLE;
  VkResult r = vkCreateDescriptorSetLayout(dev, &ci, NULL, &l);
  if (out) *out = l;
  return r;
}

// 1 = pass
static int binding_wrap(void) {
  VkResult r = layout(0xFFFFFFFFu, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, 1, NULL);
  printf("  binding 0xFFFFFFFF -> %d\n", r);
  return r != VK_SUCCESS;
}
static int dynamic_65(void) {
  VkResult r = layout(0, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER_DYNAMIC, 65, NULL);
  printf("  65 dynamic UBOs -> %d\n", r);
  return r != VK_SUCCESS;
}
static int dynamic_support_300(void) {
  VkDescriptorSetLayoutBinding b = {.binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER_DYNAMIC,
                                    .descriptorCount = 300, .stageFlags = VK_SHADER_STAGE_ALL};
  VkDescriptorSetLayoutCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &b};
  VkDescriptorSetLayoutSupport s = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_SUPPORT};
  vkGetDescriptorSetLayoutSupport(dev, &ci, &s);
  printf("  support for 300 dynamic SSBOs -> %u\n", s.supported);
  return !s.supported;
}
static int size_wrap(void) {
  // A count whose descriptor bytes overflow 32 bits.
  VkResult r = layout(0, VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 0x10000001u, NULL);
  printf("  0x10000001 storage images -> %d\n", r);
  return r != VK_SUCCESS;
}
static VkImageCreateInfo mip_image(uint32_t levels) {
  return (VkImageCreateInfo){.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                             .flags = VK_IMAGE_CREATE_BLOCK_TEXEL_VIEW_COMPATIBLE_BIT | VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT |
                                      VK_IMAGE_CREATE_EXTENDED_USAGE_BIT,
                             .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_BC1_RGBA_UNORM_BLOCK,
                             .extent = {4096, 4096, 1}, .mipLevels = levels, .arrayLayers = 1,
                             .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
                             .usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT};
}
static int mip_create(void) {
  VkImageCreateInfo ci = mip_image(20);
  VkImage img = VK_NULL_HANDLE;
  VkResult r = vkCreateImage(dev, &ci, NULL, &img);
  printf("  vkCreateImage 20 levels -> %d\n", r);
  return r != VK_SUCCESS;
}
static int mip_query(void) {
  VkImageCreateInfo ci = mip_image(20);
  VkDeviceImageMemoryRequirements q = {.sType = VK_STRUCTURE_TYPE_DEVICE_IMAGE_MEMORY_REQUIREMENTS, .pCreateInfo = &ci};
  VkMemoryRequirements2 mr = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2};
  vkGetDeviceImageMemoryRequirements(dev, &q, &mr);
  printf("  vkGetDeviceImageMemoryRequirements 20 levels -> size %llu\n", (unsigned long long)mr.memoryRequirements.size);
  return mr.memoryRequirements.size == 0;
}
static int mip_legal(void) {
  // The largest legal chain still works: 4096x4096 has 13 levels.
  VkImageCreateInfo ci = mip_image(13);
  VkImage img = VK_NULL_HANDLE;
  VkResult r = vkCreateImage(dev, &ci, NULL, &img);
  printf("  vkCreateImage 13 levels -> %d\n", r);
  if (r == VK_SUCCESS) vkDestroyImage(dev, img, NULL);
  return r == VK_SUCCESS;
}

// Bind a set with dynamic buffers through a pipeline layout; dyn_offsets may be short or NULL.
static int bind(uint32_t sets, uint32_t per_set, uint32_t first_set, uint32_t dyn_count, int null_offsets) {
  VkDescriptorSetLayout l;
  if (layout(0, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER_DYNAMIC, per_set, &l)) { printf("  layout refused\n"); return 2; }
  VkDescriptorSetLayout ls[32];
  for (uint32_t i = 0; i < sets; i++) ls[i] = l;
  VkPipelineLayoutCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = sets, .pSetLayouts = ls};
  VkPipelineLayout pl;
  if (vkCreatePipelineLayout(dev, &pci, NULL, &pl)) return 2;
  VkDescriptorPoolSize ps = {VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER_DYNAMIC, per_set * sets};
  VkDescriptorPoolCreateInfo dpci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = sets, .poolSizeCount = 1, .pPoolSizes = &ps};
  VkDescriptorPool pool;
  if (vkCreateDescriptorPool(dev, &dpci, NULL, &pool)) return 2;
  VkDescriptorSet ds[32];
  VkDescriptorSetAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = pool, .descriptorSetCount = sets, .pSetLayouts = ls};
  if (vkAllocateDescriptorSets(dev, &ai, ds)) return 2;
  VkCommandPoolCreateInfo cpci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  VkCommandPool cp;
  vkCreateCommandPool(dev, &cpci, NULL, &cp);
  VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cp, .commandBufferCount = 1};
  VkCommandBuffer cb;
  vkAllocateCommandBuffers(dev, &cbai, &cb);
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  vkBeginCommandBuffer(cb, &bi);
  static uint32_t offs[4096];
  vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, first_set, sets, ds, dyn_count, null_offsets ? NULL : offs);
  vkEndCommandBuffer(cb);
  printf("  bound %u sets of %u dynamic at set %u with %u offsets%s\n", sets, per_set, first_set, dyn_count, null_offsets ? " (NULL)" : "");
  return 1;
}
static int bind_null_offsets(void) { return bind(1, 4, 0, 0, 1); }
static int bind_total_128(void) { return bind(2, 64, 0, 128, 0); }

// A set with one binding of `count` descriptors of `type`.
static VkDescriptorSet one_set(VkDescriptorType type, uint32_t count, VkDescriptorSetLayout *lo) {
  VkDescriptorSetLayout l;
  if (layout(0, type, count, &l)) return VK_NULL_HANDLE;
  VkDescriptorPoolSize ps = {type, count};
  VkDescriptorPoolCreateInfo dpci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &ps};
  VkDescriptorPool pool;
  vkCreateDescriptorPool(dev, &dpci, NULL, &pool);
  VkDescriptorSetAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = pool, .descriptorSetCount = 1, .pSetLayouts = &l};
  VkDescriptorSet ds = VK_NULL_HANDLE;
  vkAllocateDescriptorSets(dev, &ai, &ds);
  if (lo) *lo = l;
  return ds;
}
static int write_past_array(void) {
  VkDescriptorSet ds = one_set(VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, 1, NULL);
  VkBufferView views[1] = {VK_NULL_HANDLE};
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
                            .dstArrayElement = 0x40000000u, .descriptorCount = 1,
                            .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, .pTexelBufferView = views};
  vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
  printf("  wrote element 0x40000000 of a 1-element binding\n");
  return 1;
}
static int write_past_binding(void) {
  VkDescriptorSet ds = one_set(VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, 1, NULL);
  VkBufferView views[1] = {VK_NULL_HANDLE};
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 100000,
                            .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER,
                            .pTexelBufferView = views};
  vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
  printf("  wrote binding 100000 of a 1-binding layout\n");
  return 1;
}
static int copy_past_array(void) {
  VkDescriptorSet a = one_set(VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, 1, NULL);
  VkDescriptorSet b = one_set(VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, 1, NULL);
  VkCopyDescriptorSet c = {.sType = VK_STRUCTURE_TYPE_COPY_DESCRIPTOR_SET, .srcSet = a, .srcArrayElement = 0,
                           .dstSet = b, .dstArrayElement = 0x40000000u, .descriptorCount = 1};
  vkUpdateDescriptorSets(dev, 0, NULL, 1, &c);
  printf("  copied to element 0x40000000 of a 1-element binding\n");
  return 1;
}
static VkCommandBuffer begin_cb(void) {
  VkCommandPoolCreateInfo cpci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  VkCommandPool cp;
  vkCreateCommandPool(dev, &cpci, NULL, &cp);
  VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cp, .commandBufferCount = 1};
  VkCommandBuffer cb;
  vkAllocateCommandBuffers(dev, &cbai, &cb);
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  vkBeginCommandBuffer(cb, &bi);
  return cb;
}
// Push to `set` through a pipeline layout of one set laid out as `l`.
static int push(VkDescriptorSetLayout l, uint32_t set, uint32_t elem) {
  VkPipelineLayoutCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &l};
  VkPipelineLayout pl;
  if (vkCreatePipelineLayout(dev, &pci, NULL, &pl)) return 2;
  PFN_vkCmdPushDescriptorSetKHR push_fn = (PFN_vkCmdPushDescriptorSetKHR)vkGetDeviceProcAddr(dev, "vkCmdPushDescriptorSetKHR");
  VkCommandBuffer cb = begin_cb();
  VkBufferView views[1] = {VK_NULL_HANDLE};
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstBinding = 0, .dstArrayElement = elem,
                            .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER,
                            .pTexelBufferView = views};
  push_fn(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, set, 1, &w);
  vkEndCommandBuffer(cb);
  printf("  pushed set %u element %u\n", set, elem);
  return 1;
}
static int push_set_index(void) {
  VkDescriptorSetLayout l;
  VkDescriptorSetLayoutBinding b = {.binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, .descriptorCount = 1,
                                    .stageFlags = VK_SHADER_STAGE_ALL};
  VkDescriptorSetLayoutCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                                        .flags = VK_DESCRIPTOR_SET_LAYOUT_CREATE_PUSH_DESCRIPTOR_BIT_KHR, .bindingCount = 1, .pBindings = &b};
  if (vkCreateDescriptorSetLayout(dev, &ci, NULL, &l)) return 2;
  return push(l, 1, 0);
}
static int push_big_layout(void) {
  // A layout made without the push flag, far larger than the push buffer.
  VkDescriptorSetLayout l;
  if (layout(0, VK_DESCRIPTOR_TYPE_STORAGE_TEXEL_BUFFER, 100000, &l)) return 2;
  return push(l, 0, 99999);
}

struct test { const char *name; int (*fn)(void); };
static const struct test tests[] = {
  {"binding-wrap", binding_wrap}, {"dynamic-65", dynamic_65}, {"dynamic-support-300", dynamic_support_300},
  {"size-wrap", size_wrap}, {"mip-create", mip_create}, {"mip-query", mip_query}, {"mip-legal", mip_legal},
  {"bind-null-offsets", bind_null_offsets}, {"bind-total-128", bind_total_128},
  {"write-past-array", write_past_array}, {"write-past-binding", write_past_binding},
  {"copy-past-array", copy_past_array}, {"push-set-index", push_set_index}, {"push-big-layout", push_big_layout},
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
    // A bare fork cannot reach Metal's XPC services, so each case re-executes this binary.
    pid_t p = fork();
    if (p == 0) { execl(argv[0], argv[0], tests[i].name, "--child", (char *)NULL); _exit(3); }
    int st;
    waitpid(p, &st, 0);
    const char *v = WIFSIGNALED(st) ? "CRASH" : WEXITSTATUS(st) == 0 ? "PASS" : "FAIL";
    if (WIFSIGNALED(st)) printf("  (signal %d)\n", WTERMSIG(st));
    printf("%-22s %s\n", tests[i].name, v);
    fails += strcmp(v, "PASS") != 0;
  }
  return fails != 0;
}
