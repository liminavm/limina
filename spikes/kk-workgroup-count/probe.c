// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Whether a dispatch with more workgroups than KK advertises in maxComputeWorkGroupCount runs every
// workgroup once. count.comp counts the workgroups that ran and keeps the largest id in each
// dimension; `probe X Y Z` dispatches that grid and compares.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "count_comp.h"

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

  float prio = 1;
  VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1,
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

int main(int argc, char **argv) {
  if (argc != 4) { fprintf(stderr, "usage: %s X Y Z\n", argv[0]); return 2; }
  const uint32_t g[3] = {strtoul(argv[1], NULL, 0), strtoul(argv[2], NULL, 0), strtoul(argv[3], NULL, 0)};
  setup();
  VkPhysicalDeviceProperties props;
  vkGetPhysicalDeviceProperties(pd, &props);
  printf("advertised maxComputeWorkGroupCount = {%u, %u, %u}\n", props.limits.maxComputeWorkGroupCount[0],
         props.limits.maxComputeWorkGroupCount[1], props.limits.maxComputeWorkGroupCount[2]);

  struct buf out = make_buf(64);
  VkDescriptorSetLayoutBinding lb = {.binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                                     .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT};
  VkDescriptorSetLayoutCreateInfo lci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &lb};
  VkDescriptorSetLayout l;
  if (vkCreateDescriptorSetLayout(dev, &lci, NULL, &l)) return 2;
  VkDescriptorPoolSize ps = {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1};
  VkDescriptorPoolCreateInfo dpci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &ps};
  VkDescriptorPool pool;
  if (vkCreateDescriptorPool(dev, &dpci, NULL, &pool)) return 2;
  VkDescriptorSetAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = pool,
                                    .descriptorSetCount = 1, .pSetLayouts = &l};
  VkDescriptorSet ds;
  if (vkAllocateDescriptorSets(dev, &ai, &ds)) return 2;
  VkDescriptorBufferInfo bi = {out.b, 0, VK_WHOLE_SIZE};
  VkWriteDescriptorSet w = {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0,
                            .descriptorCount = 1, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &bi};
  vkUpdateDescriptorSets(dev, 1, &w, 0, NULL);

  VkPipelineLayoutCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &l};
  VkPipelineLayout pl;
  if (vkCreatePipelineLayout(dev, &pci, NULL, &pl)) return 2;
  VkShaderModuleCreateInfo smci = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = sizeof(count_comp),
                                   .pCode = count_comp};
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
  vkCmdDispatch(cb, g[0], g[1], g[2]);
  VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
                        .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
  vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
  vkEndCommandBuffer(cb);
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
  VkResult r = vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE);
  if (!r) r = vkQueueWaitIdle(queue);
  const uint32_t *o = out.p;
  const uint64_t want = (uint64_t)g[0] * g[1] * g[2];
  printf("dispatch {%u, %u, %u}: result %d, ran %u of %llu workgroups, largest id {%u, %u, %u}\n", g[0], g[1], g[2], r,
         o[0], (unsigned long long)want, o[1], o[2], o[3]);
  /* The counter is 32 bits; at 2^32 workgroups and up the largest ids carry the evidence. */
  const int ok = !r && o[0] == (uint32_t)want && o[1] == g[0] - 1 && o[2] == g[1] - 1 && o[3] == g[2] - 1;
  printf("%s\n", ok ? "PASS" : "FAIL");
  return !ok;
}
