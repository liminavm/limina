// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * emptysub — does an EMPTY QueueSubmit's fence order behind prior submissions?
 *
 * This is vkr's guest-fence retirement pattern distilled: virglrenderer retires
 * every venus guest fence by submitting QueueSubmit(0 cmds, fence) on the ring's
 * VkQueue after the real work, and waiting that fence (vkr_queue.c). Per the
 * Vulkan queue-forward-progress model that fence must not signal before all
 * previously submitted work on the queue completes.
 *
 * kk 0017 (threaded submit, LIMINA_KK_SUBMIT_THREAD=1/default) violated this:
 * the submit thread completed a fence-only submission immediately, ~200 ms
 * before the queued copies finished — on a guest this is an early flip fence,
 * i.e. the 2026-07-29 dogfood overview-animation tearing.
 *
 * RED (bug present): fence signals in ~0 ms while the GPU is mid-workload.
 * GREEN:             fence signals at ~workload duration.
 *
 * Build:  cc -O2 -Wall -I/opt/homebrew/include -L/opt/homebrew/lib -o emptysub emptysub.c -lvulkan
 * Run:    VK_ICD_FILENAMES=<kk icd json> [LIMINA_KK_SUBMIT_THREAD=0|1] ./emptysub [copies] [buf_mib]
 * Exit:   0 = ordered (green), 2 = early-signal (red), 1 = harness error.
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <vulkan/vulkan.h>

#define CHECK(r)                                                              \
   do {                                                                       \
      VkResult res_ = (r);                                                    \
      if (res_ != VK_SUCCESS) {                                               \
         fprintf(stderr, "%s:%d: %s = %d\n", __FILE__, __LINE__, #r, res_);   \
         exit(1);                                                             \
      }                                                                       \
   } while (0)

static double
now_ms(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static uint32_t
find_mem_type(VkPhysicalDevice phys, uint32_t bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(phys, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type\n");
   exit(1);
}

int
main(int argc, char **argv)
{
   setvbuf(stdout, NULL, _IOLBF, 0);
   uint32_t copies = argc > 1 ? (uint32_t)atoi(argv[1]) : 128;
   VkDeviceSize size = (argc > 2 ? (VkDeviceSize)atoi(argv[2]) : 64) << 20;

   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "emptysub",
      .apiVersion = VK_API_VERSION_1_1,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t n = 1;
   VkPhysicalDevice phys;
   CHECK(vkEnumeratePhysicalDevices(inst, &n, &phys));
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   printf("device: %s\n", props.deviceName);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   VkDevice dev;
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
   VkQueue queue;
   vkGetDeviceQueue(dev, 0, 0, &queue);

   VkBuffer bufs[2];
   VkDeviceMemory mems[2];
   for (int i = 0; i < 2; i++) {
      VkBufferCreateInfo bi = {
         .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
         .size = size,
         .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      };
      CHECK(vkCreateBuffer(dev, &bi, NULL, &bufs[i]));
      VkMemoryRequirements mr;
      vkGetBufferMemoryRequirements(dev, bufs[i], &mr);
      VkMemoryAllocateInfo ai = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
         .allocationSize = mr.size,
         .memoryTypeIndex = find_mem_type(phys, mr.memoryTypeBits,
                                          VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
      };
      CHECK(vkAllocateMemory(dev, &ai, NULL, &mems[i]));
      CHECK(vkBindBufferMemory(dev, bufs[i], mems[i], 0));
   }

   VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = 0,
   };
   VkCommandPool pool;
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
   VkCommandBufferAllocateInfo cai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &cai, &cb));
   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
   };
   CHECK(vkBeginCommandBuffer(cb, &bi));
   VkBufferCopy region = { .size = size };
   VkMemoryBarrier mb = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT | VK_ACCESS_TRANSFER_WRITE_BIT,
   };
   for (uint32_t i = 0; i < copies; i++) {
      vkCmdCopyBuffer(cb, bufs[i & 1], bufs[!(i & 1)], 1, &region);
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                           VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, NULL,
                           0, NULL);
   }
   CHECK(vkEndCommandBuffer(cb));

   /* Calibrate the workload with its own fence (also proves plain fences work). */
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fwork, fempty;
   CHECK(vkCreateFence(dev, &fci, NULL, &fwork));
   CHECK(vkCreateFence(dev, &fci, NULL, &fempty));
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cb,
   };
   double t0 = now_ms();
   CHECK(vkQueueSubmit(queue, 1, &si, fwork));
   CHECK(vkWaitForFences(dev, 1, &fwork, VK_TRUE, UINT64_MAX));
   double gpu_ms = now_ms() - t0;
   printf("workload: %u copies of %llu MiB = %.1f ms\n", copies,
          (unsigned long long)(size >> 20), gpu_ms);
   if (gpu_ms < 30.0)
      printf("WARNING: workload short; raise arg1 for a clear verdict\n");
   CHECK(vkResetFences(dev, 1, &fwork));

   /* THE TEST: work submit, then vkr's retirement pattern — an EMPTY submit
    * whose fence must not signal before the work completes. */
   t0 = now_ms();
   CHECK(vkQueueSubmit(queue, 1, &si, fwork));
   CHECK(vkQueueSubmit(queue, 0, NULL, fempty));
   double t_submit = now_ms() - t0;
   CHECK(vkWaitForFences(dev, 1, &fempty, VK_TRUE, UINT64_MAX));
   double t_empty = now_ms() - t0;
   CHECK(vkWaitForFences(dev, 1, &fwork, VK_TRUE, UINT64_MAX));
   double t_work = now_ms() - t0;

   printf("submits returned %.2f ms; empty-submit fence %.1f ms; work fence "
          "%.1f ms\n",
          t_submit, t_empty, t_work);

   int early = t_empty < t_work * 0.5;
   printf("VERDICT: empty-submit fence %s prior work (%s)\n",
          early ? "DOES NOT ORDER BEHIND" : "orders behind",
          early ? "RED - vkr guest fences signal early" : "green");

   vkDestroyFence(dev, fwork, NULL);
   vkDestroyFence(dev, fempty, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   for (int i = 0; i < 2; i++) {
      vkDestroyBuffer(dev, bufs[i], NULL);
      vkFreeMemory(dev, mems[i], NULL);
   }
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return early ? 2 : 0;
}
