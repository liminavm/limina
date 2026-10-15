// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Descriptor updates that span consecutive bindings, which the spec allows: a write, copy or
// template entry whose count runs past dstBinding goes on into dstBinding+1 at element 0,
// skipping bindings with no descriptors (for an inline uniform block, in bytes). ggml's Vulkan
// backend writes all of a dispatch's buffers this way. Each case updates a set, dispatches a
// compute shader that copies what the spanned bindings hold into binding 0, and checks the
// values -- what a descriptor holds cannot be read back any other way. The `bad-*` cases are
// spans the spec forbids, which KK must still refuse without overrunning anything.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "span/span_array.h"
#include "span/span_inline.h"
#include "span/span_skip.h"
#include "span/span_three.h"

#define A_VAL 0xA1A1A1A1u
#define B_VAL 0xB2B2B2B2u
#define BUF_SIZE 256

static VkInstance inst;
static VkPhysicalDevice pd;
static VkDevice dev;
static VkQueue queue;
static int have_inline;

static void setup(void) {
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3};
  const char *iexts[] = {"VK_KHR_portability_enumeration"};
  VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
                              .flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
                              .enabledExtensionCount = 1, .ppEnabledExtensionNames = iexts};
  // A guest loader may not offer the portability extension; retry without it.
  if (vkCreateInstance(&ici, NULL, &inst)) {
    ici.flags = 0;
    ici.enabledExtensionCount = 0;
    if (vkCreateInstance(&ici, NULL, &inst)) { fprintf(stderr, "instance\n"); exit(2); }
  }
  uint32_t n = 1;
  vkEnumeratePhysicalDevices(inst, &n, &pd);
  if (!n) { fprintf(stderr, "no device\n"); exit(2); }

  VkPhysicalDeviceVulkan13Features f13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES};
  VkPhysicalDeviceFeatures2 f2 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &f13};
  vkGetPhysicalDeviceFeatures2(pd, &f2);
  have_inline = f13.inlineUniformBlock;
  VkPhysicalDeviceVulkan13Features want13 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES,
                                             .inlineUniformBlock = f13.inlineUniformBlock};

  float prio = 1;
  VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio};
  const char *dexts[] = {"VK_KHR_push_descriptor"};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &want13, .queueCreateInfoCount = 1,
                            .pQueueCreateInfos = &q, .enabledExtensionCount = 1, .ppEnabledExtensionNames = dexts};
  VkResult r = vkCreateDevice(pd, &dci, NULL, &dev);
  if (r) { fprintf(stderr, "device: %d\n", r); exit(2); }
  vkGetDeviceQueue(dev, 0, 0, &queue);
}

struct buf { VkBuffer b; VkDeviceMemory m; uint32_t *p; };

static struct buf make_buf(uint32_t first) {
  struct buf r = {0};
  VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = BUF_SIZE,
                            .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT};
  if (vkCreateBuffer(dev, &bci, NULL, &r.b)) { fprintf(stderr, "buffer\n"); exit(2); }
  VkMemoryRequirements mr;
  vkGetBufferMemoryRequirements(dev, r.b, &mr);
  VkPhysicalDeviceMemoryProperties mp;
  vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  const VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
  uint32_t type = UINT32_MAX;
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    if ((mr.memoryTypeBits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) { type = i; break; }
  if (type == UINT32_MAX) { fprintf(stderr, "no host-visible memory\n"); exit(2); }
  VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size, .memoryTypeIndex = type};
  if (vkAllocateMemory(dev, &mai, NULL, &r.m)) { fprintf(stderr, "memory\n"); exit(2); }
  vkBindBufferMemory(dev, r.b, r.m, 0);
  vkMapMemory(dev, r.m, 0, VK_WHOLE_SIZE, 0, (void **)&r.p);
  memset(r.p, 0, BUF_SIZE);
  r.p[0] = first;
  return r;
}

static VkDescriptorBufferInfo info(struct buf b) { return (VkDescriptorBufferInfo){b.b, 0, VK_WHOLE_SIZE}; }

// A set layout from `n` bindings numbered 0..n-1, each `counts[i]` descriptors of `types[i]`.
static VkDescriptorSetLayout make_layout(uint32_t n, const VkDescriptorType *types, const uint32_t *counts,
                                         VkDescriptorSetLayoutCreateFlags flags) {
  VkDescriptorSetLayoutBinding b[8];
  for (uint32_t i = 0; i < n; i++)
    b[i] = (VkDescriptorSetLayoutBinding){.binding = i, .descriptorType = types[i], .descriptorCount = counts[i],
                                          .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT};
  VkDescriptorSetLayoutCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .flags = flags,
                                        .bindingCount = n, .pBindings = b};
  VkDescriptorSetLayout l;
  if (vkCreateDescriptorSetLayout(dev, &ci, NULL, &l)) { fprintf(stderr, "set layout\n"); exit(2); }
  return l;
}

static VkDescriptorSet make_set(VkDescriptorSetLayout l) {
  VkDescriptorPoolSize ps[] = {{VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 16}, {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER_DYNAMIC, 16},
                               {VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, 16}, {VK_DESCRIPTOR_TYPE_INLINE_UNIFORM_BLOCK, 256}};
  VkDescriptorPoolInlineUniformBlockCreateInfo ipi = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_INLINE_UNIFORM_BLOCK_CREATE_INFO,
                                                      .maxInlineUniformBlockBindings = 4};
  VkDescriptorPoolCreateInfo dpci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .pNext = have_inline ? &ipi : NULL,
                                     .maxSets = 1, .poolSizeCount = have_inline ? 4 : 3, .pPoolSizes = ps};
  VkDescriptorPool pool;
  if (vkCreateDescriptorPool(dev, &dpci, NULL, &pool)) { fprintf(stderr, "pool\n"); exit(2); }
  VkDescriptorSetAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = pool,
                                    .descriptorSetCount = 1, .pSetLayouts = &l};
  VkDescriptorSet ds;
  if (vkAllocateDescriptorSets(dev, &ai, &ds)) { fprintf(stderr, "set\n"); exit(2); }
  return ds;
}

// Dispatch `code` once against `l` and check binding 0's buffer `out` came back {A_VAL, B_VAL}.
// `bind` binds the set (or pushes it) into the recorded command buffer.
struct bind_ctx { VkDescriptorSet set; uint32_t dyn_count; void (*push)(VkCommandBuffer, VkPipelineLayout, void *); void *arg; };

static int dispatch_and_check(VkDescriptorSetLayout l, const uint32_t *code, size_t code_size, struct bind_ctx bc,
                              const uint32_t *out) {
  VkPipelineLayoutCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &l};
  VkPipelineLayout pl;
  if (vkCreatePipelineLayout(dev, &pci, NULL, &pl)) return 2;
  VkShaderModuleCreateInfo smci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = code_size, .pCode = code};
  VkShaderModule sm;
  if (vkCreateShaderModule(dev, &smci, NULL, &sm)) return 2;
  VkComputePipelineCreateInfo cpci = {.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
                                      .stage = {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                                                .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = sm, .pName = "main"},
                                      .layout = pl};
  VkPipeline pipe;
  if (vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe)) return 2;

  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  VkCommandPool cp;
  vkCreateCommandPool(dev, &cpi, NULL, &cp);
  VkCommandBufferAllocateInfo cbai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cp, .commandBufferCount = 1};
  VkCommandBuffer cb;
  vkAllocateCommandBuffers(dev, &cbai, &cb);
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  vkBeginCommandBuffer(cb, &bi);
  vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
  if (bc.push) {
    bc.push(cb, pl, bc.arg);
  } else {
    static const uint32_t zero[8];
    vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &bc.set, bc.dyn_count, zero);
  }
  vkCmdDispatch(cb, 1, 1, 1);
  VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
                        .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
  vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
  vkEndCommandBuffer(cb);
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
  if (vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE)) return 2;
  if (vkQueueWaitIdle(queue)) return 2;

  printf("  out = {0x%08x, 0x%08x}, want {0x%08x, 0x%08x}\n", out[0], out[1], A_VAL, B_VAL);
  return out[0] == A_VAL && out[1] == B_VAL;
}

static const VkDescriptorType SSBO = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;

// One write of three buffers at binding 0 of a three-binding layout: ggml's shape.
static int write_span(void) {
  VkDescriptorType t[] = {SSBO, SSBO, SSBO};
  uint32_t c[] = {1, 1, 1};
  VkDescriptorSetLayout l = make_layout(3, t, c, 0);
  VkDescriptorSet ds = make_set(l);
  struct buf o = make_buf(0), a = make_buf(A_VAL), b = make_buf(B_VAL);
  VkDescriptorBufferInfo bi[] = {info(o), info(a), info(b)};
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
                            .descriptorCount = 3, .descriptorType = SSBO, .pBufferInfo = bi};
  vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
  return dispatch_and_check(l, span_three, sizeof(span_three), (struct bind_ctx){.set = ds}, o.p);
}

// The same with dynamic storage buffers, which KK keeps outside the descriptor buffer.
static int write_span_dynamic(void) {
  const VkDescriptorType D = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER_DYNAMIC;
  VkDescriptorType t[] = {D, D, D};
  uint32_t c[] = {1, 1, 1};
  VkDescriptorSetLayout l = make_layout(3, t, c, 0);
  VkDescriptorSet ds = make_set(l);
  struct buf o = make_buf(0), a = make_buf(A_VAL), b = make_buf(B_VAL);
  VkDescriptorBufferInfo bi[] = {info(o), info(a), info(b)};
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
                            .descriptorCount = 3, .descriptorType = D, .pBufferInfo = bi};
  vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
  return dispatch_and_check(l, span_three, sizeof(span_three), (struct bind_ctx){.set = ds, .dyn_count = 3}, o.p);
}

// A binding with no descriptors is skipped: three from binding 0 fill 0, 2 and 3.
static int write_span_skip(void) {
  VkDescriptorType t[] = {SSBO, SSBO, SSBO, SSBO};
  uint32_t c[] = {1, 0, 1, 1};
  VkDescriptorSetLayout l = make_layout(4, t, c, 0);
  VkDescriptorSet ds = make_set(l);
  struct buf o = make_buf(0), a = make_buf(A_VAL), b = make_buf(B_VAL);
  VkDescriptorBufferInfo bi[] = {info(o), info(a), info(b)};
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
                            .descriptorCount = 3, .descriptorType = SSBO, .pBufferInfo = bi};
  vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
  return dispatch_and_check(l, span_skip, sizeof(span_skip), (struct bind_ctx){.set = ds}, o.p);
}

// A span that starts inside an array: element 1 of binding 0, then binding 1.
static int write_span_array(void) {
  VkDescriptorType t[] = {SSBO, SSBO};
  uint32_t c[] = {2, 1};
  VkDescriptorSetLayout l = make_layout(2, t, c, 0);
  VkDescriptorSet ds = make_set(l);
  struct buf o = make_buf(0), a = make_buf(A_VAL), b = make_buf(B_VAL);
  VkDescriptorBufferInfo bo = info(o), bi[] = {info(a), info(b)};
  VkWriteDescriptorSet w[] = {
    {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0, .dstArrayElement = 0,
     .descriptorCount = 1, .descriptorType = SSBO, .pBufferInfo = &bo},
    {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0, .dstArrayElement = 1,
     .descriptorCount = 2, .descriptorType = SSBO, .pBufferInfo = bi},
  };
  vkUpdateDescriptorSets(dev, 2, w, 0, NULL);
  return dispatch_and_check(l, span_array, sizeof(span_array), (struct bind_ctx){.set = ds}, o.p);
}

// Inline uniform blocks span in bytes: 32 bytes at binding 1 fill two 16-byte blocks.
static int write_span_inline(void) {
  if (!have_inline) { printf("  no inlineUniformBlock\n"); return 3; }
  const VkDescriptorType I = VK_DESCRIPTOR_TYPE_INLINE_UNIFORM_BLOCK;
  VkDescriptorType t[] = {SSBO, I, I};
  uint32_t c[] = {1, 16, 16};
  VkDescriptorSetLayout l = make_layout(3, t, c, 0);
  VkDescriptorSet ds = make_set(l);
  struct buf o = make_buf(0);
  VkDescriptorBufferInfo bo = info(o);
  uint32_t data[8] = {A_VAL, 0, 0, 0, B_VAL, 0, 0, 0};
  VkWriteDescriptorSetInlineUniformBlock wi = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET_INLINE_UNIFORM_BLOCK,
                                               .dataSize = sizeof(data), .pData = data};
  VkWriteDescriptorSet w[] = {
    {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0, .descriptorCount = 1,
     .descriptorType = SSBO, .pBufferInfo = &bo},
    {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .pNext = &wi, .dstSet = ds, .dstBinding = 1,
     .descriptorCount = sizeof(data), .descriptorType = I},
  };
  vkUpdateDescriptorSets(dev, 2, w, 0, NULL);
  return dispatch_and_check(l, span_inline, sizeof(span_inline), (struct bind_ctx){.set = ds}, o.p);
}

// A copy of three from binding 0 of a set written one binding at a time.
static int copy_span(void) {
  VkDescriptorType t[] = {SSBO, SSBO, SSBO};
  uint32_t c[] = {1, 1, 1};
  VkDescriptorSetLayout l = make_layout(3, t, c, 0);
  VkDescriptorSet src = make_set(l), dst = make_set(l);
  struct buf o = make_buf(0), a = make_buf(A_VAL), b = make_buf(B_VAL);
  VkDescriptorBufferInfo bi[] = {info(o), info(a), info(b)};
  VkWriteDescriptorSet w[3];
  for (uint32_t i = 0; i < 3; i++)
    w[i] = (VkWriteDescriptorSet){.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = src, .dstBinding = i,
                                  .descriptorCount = 1, .descriptorType = SSBO, .pBufferInfo = &bi[i]};
  vkUpdateDescriptorSets(dev, 3, w, 0, NULL);
  VkCopyDescriptorSet cp = {.sType = VK_STRUCTURE_TYPE_COPY_DESCRIPTOR_SET, .srcSet = src, .srcBinding = 0,
                            .dstSet = dst, .dstBinding = 0, .descriptorCount = 3};
  vkUpdateDescriptorSets(dev, 0, NULL, 1, &cp);
  return dispatch_and_check(l, span_three, sizeof(span_three), (struct bind_ctx){.set = dst}, o.p);
}

// An update template whose one entry spans the three bindings.
static int template_span(void) {
  VkDescriptorType t[] = {SSBO, SSBO, SSBO};
  uint32_t c[] = {1, 1, 1};
  VkDescriptorSetLayout l = make_layout(3, t, c, 0);
  VkDescriptorSet ds = make_set(l);
  struct buf o = make_buf(0), a = make_buf(A_VAL), b = make_buf(B_VAL);
  VkDescriptorBufferInfo bi[] = {info(o), info(a), info(b)};
  VkDescriptorUpdateTemplateEntry e = {.dstBinding = 0, .descriptorCount = 3, .descriptorType = SSBO,
                                       .offset = 0, .stride = sizeof(VkDescriptorBufferInfo)};
  VkDescriptorUpdateTemplateCreateInfo tci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_UPDATE_TEMPLATE_CREATE_INFO,
                                              .descriptorUpdateEntryCount = 1, .pDescriptorUpdateEntries = &e,
                                              .templateType = VK_DESCRIPTOR_UPDATE_TEMPLATE_TYPE_DESCRIPTOR_SET,
                                              .descriptorSetLayout = l};
  VkDescriptorUpdateTemplate tmpl;
  if (vkCreateDescriptorUpdateTemplate(dev, &tci, NULL, &tmpl)) return 2;
  vkUpdateDescriptorSetWithTemplate(dev, ds, tmpl, bi);
  return dispatch_and_check(l, span_three, sizeof(span_three), (struct bind_ctx){.set = ds}, o.p);
}

struct push_arg { VkDescriptorBufferInfo bi[3]; };
static void push_three(VkCommandBuffer cb, VkPipelineLayout pl, void *arg) {
  struct push_arg *p = arg;
  PFN_vkCmdPushDescriptorSetKHR fn = (PFN_vkCmdPushDescriptorSetKHR)vkGetDeviceProcAddr(dev, "vkCmdPushDescriptorSetKHR");
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstBinding = 0, .descriptorCount = 3,
                            .descriptorType = SSBO, .pBufferInfo = p->bi};
  fn(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &w);
}

// A push-descriptor write that spans the three bindings.
static int push_span(void) {
  VkDescriptorType t[] = {SSBO, SSBO, SSBO};
  uint32_t c[] = {1, 1, 1};
  VkDescriptorSetLayout l = make_layout(3, t, c, VK_DESCRIPTOR_SET_LAYOUT_CREATE_PUSH_DESCRIPTOR_BIT_KHR);
  struct buf o = make_buf(0), a = make_buf(A_VAL), b = make_buf(B_VAL);
  struct push_arg arg = {{info(o), info(a), info(b)}};
  return dispatch_and_check(l, span_three, sizeof(span_three), (struct bind_ctx){.push = push_three, .arg = &arg}, o.p);
}

// Invalid: the span reaches a binding of another type. KK must refuse, not write a storage
// descriptor into a uniform-buffer slot. Recorded only; nothing is dispatched.
static int bad_span_type(void) {
  VkDescriptorType t[] = {SSBO, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER};
  uint32_t c[] = {1, 1};
  VkDescriptorSet ds = make_set(make_layout(2, t, c, 0));
  struct buf a = make_buf(A_VAL);
  VkDescriptorBufferInfo bi[] = {info(a), info(a)};
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
                            .descriptorCount = 2, .descriptorType = SSBO, .pBufferInfo = bi};
  vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
  printf("  wrote two storage buffers over a storage and a uniform binding\n");
  return 1;
}

// Invalid: the span runs past the last binding.
static int bad_span_end(void) {
  VkDescriptorType t[] = {SSBO, SSBO};
  uint32_t c[] = {1, 1};
  VkDescriptorSet ds = make_set(make_layout(2, t, c, 0));
  struct buf a = make_buf(A_VAL);
  VkDescriptorBufferInfo bi[64];
  for (int i = 0; i < 64; i++) bi[i] = info(a);
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
                            .descriptorCount = 64, .descriptorType = SSBO, .pBufferInfo = bi};
  vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);
  VkCopyDescriptorSet cp = {.sType = VK_STRUCTURE_TYPE_COPY_DESCRIPTOR_SET, .srcSet = ds, .dstSet = ds,
                            .dstBinding = 1, .descriptorCount = 64};
  vkUpdateDescriptorSets(dev, 0, NULL, 1, &cp);
  printf("  wrote and copied 64 descriptors into a two-binding set\n");
  return 1;
}

struct test { const char *name; int (*fn)(void); };
static const struct test tests[] = {
  {"write-span", write_span}, {"write-span-dynamic", write_span_dynamic}, {"write-span-skip", write_span_skip},
  {"write-span-array", write_span_array}, {"write-span-inline", write_span_inline}, {"copy-span", copy_span},
  {"template-span", template_span}, {"push-span", push_span},
  {"bad-span-type", bad_span_type}, {"bad-span-end", bad_span_end},
};

int main(int argc, char **argv) {
  if (argc > 2 && !strcmp(argv[2], "--child")) {
    for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++)
      if (!strcmp(argv[1], tests[i].name)) {
        setup();
        int r = tests[i].fn();
        return r == 1 ? 0 : r == 3 ? 3 : 1;
      }
    return 2;
  }
  int fails = 0;
  for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
    if (argc > 1 && strcmp(argv[1], tests[i].name)) continue;
    fflush(stdout);
    // A bare fork cannot reach Metal's XPC services, so each case re-executes this binary.
    pid_t p = fork();
    if (p == 0) { execl(argv[0], argv[0], tests[i].name, "--child", (char *)NULL); _exit(4); }
    int st;
    waitpid(p, &st, 0);
    const char *v = WIFSIGNALED(st) ? "CRASH" : WEXITSTATUS(st) == 0 ? "PASS" : WEXITSTATUS(st) == 3 ? "SKIP" : "FAIL";
    if (WIFSIGNALED(st)) printf("  (signal %d)\n", WTERMSIG(st));
    printf("%-22s %s\n", tests[i].name, v);
    fails += strcmp(v, "PASS") != 0 && strcmp(v, "SKIP") != 0;
  }
  return fails != 0;
}
