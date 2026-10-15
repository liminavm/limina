// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// NIR's shifts take their count modulo the operand's bit size, so NIR drops a mask that only keeps
// the count in range: `(b >> (4 * (i & 1)))` on a byte becomes `b >> (i << 2)`. MSL shifts by the
// whole count, and a byte or a short is promoted to int first, so a count past the operand's width
// shifts its bits away instead of wrapping. shift.comp has that shape for 8-, 16- and 32-bit
// operands (the 8-bit one is ggml's IQ scale read); this dispatches it and checks every result.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "shift_comp.h"

static VkInstance inst;
static VkPhysicalDevice pd;
static VkDevice dev;
static VkQueue queue;

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

  VkPhysicalDeviceVulkan12Features f12 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
                                          .storageBuffer8BitAccess = VK_TRUE, .shaderInt8 = VK_TRUE};
  VkPhysicalDeviceVulkan11Features f11 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES,
                                          .pNext = &f12, .storageBuffer16BitAccess = VK_TRUE};
  VkPhysicalDeviceFeatures2 f2 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &f11,
                                  .features = {.shaderInt16 = VK_TRUE}};
  float prio = 1;
  VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f2, .queueCreateInfoCount = 1,
                            .pQueueCreateInfos = &q};
  VkResult r = vkCreateDevice(pd, &dci, NULL, &dev);
  if (r) { fprintf(stderr, "device: %d\n", r); exit(2); }
  vkGetDeviceQueue(dev, 0, 0, &queue);
}

struct buf { VkBuffer b; VkDeviceMemory m; void *p; };

static struct buf make_buf(VkDeviceSize size) {
  struct buf r = {0};
  VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = size,
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
  vkMapMemory(dev, r.m, 0, VK_WHOLE_SIZE, 0, &r.p);
  memset(r.p, 0, size);
  return r;
}

int main(void) {
  setup();

  struct buf b8 = make_buf(64), b16 = make_buf(64), b32 = make_buf(64), out = make_buf(256);
  const uint8_t s8[4] = {0x21, 0x43, 0x65, 0x87};
  const uint16_t s16 = 0xDCBA;
  const uint32_t s32 = 0x89ABCDEFu;
  memcpy(b8.p, s8, sizeof(s8));
  memcpy(b16.p, &s16, sizeof(s16));
  memcpy(b32.p, &s32, sizeof(s32));

  VkDescriptorSetLayoutBinding lb[4];
  for (uint32_t i = 0; i < 4; i++)
    lb[i] = (VkDescriptorSetLayoutBinding){.binding = i, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                                           .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT};
  VkDescriptorSetLayoutCreateInfo lci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 4, .pBindings = lb};
  VkDescriptorSetLayout l;
  if (vkCreateDescriptorSetLayout(dev, &lci, NULL, &l)) return 2;
  VkDescriptorPoolSize ps = {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 4};
  VkDescriptorPoolCreateInfo dpci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &ps};
  VkDescriptorPool pool;
  if (vkCreateDescriptorPool(dev, &dpci, NULL, &pool)) return 2;
  VkDescriptorSetAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = pool,
                                    .descriptorSetCount = 1, .pSetLayouts = &l};
  VkDescriptorSet ds;
  if (vkAllocateDescriptorSets(dev, &ai, &ds)) return 2;
  VkDescriptorBufferInfo bi[4] = {{b8.b, 0, VK_WHOLE_SIZE}, {b16.b, 0, VK_WHOLE_SIZE}, {b32.b, 0, VK_WHOLE_SIZE},
                                  {out.b, 0, VK_WHOLE_SIZE}};
  VkWriteDescriptorSet w[4];
  for (uint32_t i = 0; i < 4; i++)
    w[i] = (VkWriteDescriptorSet){.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = i,
                                  .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                                  .pBufferInfo = &bi[i]};
  vkUpdateDescriptorSets(dev, 4, w, 0, NULL);

  VkPipelineLayoutCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &l};
  VkPipelineLayout pl;
  if (vkCreatePipelineLayout(dev, &pci, NULL, &pl)) return 2;
  VkShaderModuleCreateInfo smci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = sizeof(shift_comp),
                                   .pCode = shift_comp};
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
  VkCommandBufferBeginInfo cbi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  vkBeginCommandBuffer(cb, &cbi);
  vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
  vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
  vkCmdDispatch(cb, 1, 1, 1);
  VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
                        .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
  vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
  vkEndCommandBuffer(cb);
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
  if (vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE) || vkQueueWaitIdle(queue)) return 2;

  static const char *const what[4] = {"8-bit  (s8[i/2] >> 4*(i&1)) & 0xF", "16-bit (s16 >> (4i & 15)) & 0xF",
                                      "32-bit (s32 >> (8i & 31)) & 0xFF", "32-bit (s32 << (8i & 31)) >> 24"};
  const uint32_t *o = out.p;
  int bad = 0;
  for (uint32_t row = 0; row < 4; row++) {
    printf("%-36s", what[row]);
    for (uint32_t i = 0; i < 8; i++) {
      uint32_t want;
      switch (row) {
      case 0: want = (s8[i / 2] >> (4 * (i & 1))) & 0xF; break;
      case 1: want = (s16 >> ((4 * i) & 15)) & 0xF; break;
      case 2: want = (s32 >> ((8 * i) & 31)) & 0xFF; break;
      default: want = (s32 << ((8 * i) & 31)) >> 24; break;
      }
      const uint32_t got = o[row * 8 + i];
      printf(" %02x%s", got, got == want ? "" : "!");
      bad += got != want;
    }
    printf("\n");
  }
  printf("%s (%d wrong; ! marks a value that differs from the CPU's)\n", bad ? "FAIL" : "PASS", bad);
  return bad != 0;
}
