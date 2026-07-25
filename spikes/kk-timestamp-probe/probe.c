// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * kk-timestamp-probe — split the "venus timestamp queries resolve to zero" gap
 * (gnome-shell-rs docs/fork/venus-timestamp-gap.md) between the host Vulkan
 * driver (KosmicKrisp) and the venus transport, by running the same shapes
 * natively on the host with NO VM involved.
 *
 * Three cases, because the guest and the host exercise DIFFERENT KK code paths:
 *
 *   A. vkGetQueryPoolResults directly            -> kk_GetQueryPoolResults (CPU readback)
 *   B. vkCmdCopyQueryPoolResults into a buffer   -> libkk_copy_queries (GPU kernel)
 *   C. B, but the copy is in a SEPARATE command buffer submitted together
 *
 * (C) is what the guest actually hits: Mesa venus never calls
 * vkGetQueryPoolResults on the host — it serves the guest from a feedback
 * buffer that it fills with a vkCmdCopyQueryPoolResults recorded into a linked
 * feedback command buffer (mesa src/virtio/vulkan/vn_query_pool.c,
 * vn_get_query_pool_feedback + vn_cmd_record_query). So if A passes and B/C
 * return zero, the bug is host-side in KK's query-copy path, not in venus.
 *
 * Build/run: ./run.sh
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define CHECK(x)                                                               \
   do {                                                                        \
      VkResult _r = (x);                                                       \
      if (_r != VK_SUCCESS) {                                                  \
         fprintf(stderr, "%s:%d: %s -> %d\n", __FILE__, __LINE__, #x, _r);     \
         exit(1);                                                              \
      }                                                                        \
   } while (0)

static VkInstance inst;
static VkPhysicalDevice pdev;
static VkDevice dev;
static VkQueue queue;
static VkCommandPool cmd_pool;
static uint32_t qfam;

static uint32_t
find_mem_type(uint32_t bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type for 0x%x\n", want);
   exit(1);
}

static VkCommandBuffer
new_cmd(void)
{
   VkCommandBuffer cb;
   VkCommandBufferAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cmd_pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   CHECK(vkAllocateCommandBuffers(dev, &ai, &cb));
   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkBeginCommandBuffer(cb, &bi));
   return cb;
}

static void
submit_and_wait(VkCommandBuffer *cbs, uint32_t n)
{
   VkFence fence;
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = n,
      .pCommandBuffers = cbs,
   };
   CHECK(vkQueueSubmit(queue, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 5ull * 1000 * 1000 * 1000));
   vkDestroyFence(dev, fence, NULL);
}

static VkQueryPool
new_ts_pool(void)
{
   VkQueryPool pool;
   VkQueryPoolCreateInfo qi = {
      .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
      .queryType = VK_QUERY_TYPE_TIMESTAMP,
      .queryCount = 2,
   };
   CHECK(vkCreateQueryPool(dev, &qi, NULL, &pool));
   return pool;
}

/* Case A: the guest's API shape, but served by kk_GetQueryPoolResults. */
static void
case_a(void)
{
   VkQueryPool pool = new_ts_pool();
   VkCommandBuffer cb = new_cmd();
   vkCmdResetQueryPool(cb, pool, 0, 2);
   vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, pool, 0);
   vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, pool, 1);
   CHECK(vkEndCommandBuffer(cb));
   submit_and_wait(&cb, 1);

   uint64_t out[4] = { 0 };
   VkResult r = vkGetQueryPoolResults(
      dev, pool, 0, 2, sizeof(out), out, 2 * sizeof(uint64_t),
      VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WITH_AVAILABILITY_BIT);
   printf("A  vkGetQueryPoolResults      -> %d  q0=%llu avail=%llu  q1=%llu avail=%llu\n",
          r, (unsigned long long)out[0], (unsigned long long)out[1],
          (unsigned long long)out[2], (unsigned long long)out[3]);
   if (out[0] && out[2])
      printf("   delta = %lld ns\n", (long long)(out[2] - out[0]));
   vkDestroyQueryPool(dev, pool, NULL);
}

/* Cases B/C: what venus really uses — a GPU-side vkCmdCopyQueryPoolResults
 * into a host-visible buffer (libkk_copy_queries). `separate_cb` records the
 * copy in its own command buffer, as venus's linked feedback cmd does. */
static void
case_bc(int separate_cb, const char *label)
{
   VkQueryPool pool = new_ts_pool();

   VkBuffer buf;
   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = 4 * sizeof(uint64_t),
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, buf, &mr);
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = find_mem_type(mr.memoryTypeBits,
                                       VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                          VK_MEMORY_PROPERTY_HOST_COHERENT_BIT),
   };
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
   CHECK(vkBindBufferMemory(dev, buf, mem, 0));
   void *map;
   CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, &map));
   memset(map, 0xAB, 4 * sizeof(uint64_t)); /* poison: prove the copy ran */

   VkCommandBuffer cbs[2];
   cbs[0] = new_cmd();
   vkCmdResetQueryPool(cbs[0], pool, 0, 2);
   vkCmdWriteTimestamp(cbs[0], VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, pool, 0);
   vkCmdWriteTimestamp(cbs[0], VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, pool, 1);

   VkCommandBuffer copy_cb = cbs[0];
   if (separate_cb) {
      CHECK(vkEndCommandBuffer(cbs[0]));
      cbs[1] = new_cmd();
      copy_cb = cbs[1];
   }
   vkCmdCopyQueryPoolResults(
      copy_cb, pool, 0, 2, buf, 0, 2 * sizeof(uint64_t),
      VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WITH_AVAILABILITY_BIT);
   CHECK(vkEndCommandBuffer(copy_cb));

   submit_and_wait(cbs, separate_cb ? 2 : 1);

   uint64_t *out = map;
   printf("%s vkCmdCopyQueryPoolResults  ->    q0=%llu avail=%llu  q1=%llu avail=%llu\n",
          label, (unsigned long long)out[0], (unsigned long long)out[1],
          (unsigned long long)out[2], (unsigned long long)out[3]);
   if (out[0] && out[2] && out[0] != 0xABABABABABABABABull)
      printf("   delta = %lld ns\n", (long long)(out[2] - out[0]));

   vkUnmapMemory(dev, mem);
   vkDestroyBuffer(dev, buf, NULL);
   vkFreeMemory(dev, mem, NULL);
   vkDestroyQueryPool(dev, pool, NULL);
}

int
main(void)
{
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t n = 1;
   CHECK(vkEnumeratePhysicalDevices(inst, &n, &pdev));

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pdev, &props);

   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, NULL);
   VkQueueFamilyProperties *qs = calloc(nq, sizeof(*qs));
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qs);
   qfam = UINT32_MAX;
   for (uint32_t i = 0; i < nq; i++)
      if (qs[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
         qfam = i;
         break;
      }

   printf("device: %s\n", props.deviceName);
   printf("  timestampPeriod    = %g\n", props.limits.timestampPeriod);
   printf("  timestampValidBits = %u (queue family %u)\n",
          qs[qfam].timestampValidBits, qfam);
   printf("\n");

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfam,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .queueCreateInfoCount = 1,
                              .pQueueCreateInfos = &qci };
   CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, qfam, 0, &queue);

   VkCommandPoolCreateInfo cpi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = qfam,
   };
   CHECK(vkCreateCommandPool(dev, &cpi, NULL, &cmd_pool));

   case_a();
   case_bc(0, "B ");
   case_bc(1, "C ");

   printf("\n0xABAB... = the copy never ran; 0 with avail=1 = the guest symptom.\n");
   return 0;
}
