// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * rtt: what one guest -> host -> guest round trip costs on venus today.
 *
 * Two probes, each at several gaps between calls (the venus ring thread spins, then backs
 * off, then parks one VN_RING_IDLE_TIMEOUT_NS -- 1 ms -- after its last work; a parked ring
 * needs the doorbell, which is a virtqueue kick):
 *   query  vkGetBufferDeviceAddress: venus has no cache for it, so every call is a
 *          vn_call_* round trip (vn_buffer.c), and it is the very value a guest Metal
 *          driver would otherwise have to ask for
 *   fence  an empty vkQueueSubmit with a fence, then vkWaitForFences: ring + host fence
 *
 * Build in the guest: gcc -O2 -o rtt rtt.c -lvulkan
 * Usage: rtt [iters]   prints p50/p90/p99/max in microseconds per probe and gap.
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <vulkan/vulkan.h>

#define CHECK(x)                                                                                   \
   do {                                                                                            \
      VkResult r_ = (x);                                                                           \
      if (r_ != VK_SUCCESS) {                                                                      \
         fprintf(stderr, "%s failed: %d\n", #x, r_);                                              \
         exit(1);                                                                                  \
      }                                                                                            \
   } while (0)

static uint64_t now_ns(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

static void gap(uint64_t ns)
{
   if (!ns)
      return;
   struct timespec ts = {ns / 1000000000ull, ns % 1000000000ull};
   nanosleep(&ts, NULL);
}

static int cmp(const void *a, const void *b)
{
   uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
   return x < y ? -1 : x > y;
}

static void report(const char *probe, uint64_t gap_ns, uint64_t *t, int n)
{
   qsort(t, n, sizeof(*t), cmp);
   printf("%-6s gap=%6.0fus n=%-6d p50=%8.1fus p90=%8.1fus p99=%8.1fus max=%9.1fus\n", probe,
          gap_ns / 1e3, n, t[n / 2] / 1e3, t[n * 9 / 10] / 1e3, t[n * 99 / 100] / 1e3,
          t[n - 1] / 1e3);
   fflush(stdout);
}

int main(int argc, char **argv)
{
   int iters = argc > 1 ? atoi(argv[1]) : 5000;

   VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                            .apiVersion = VK_API_VERSION_1_2};
   VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                               .pApplicationInfo = &app};
   VkInstance inst;
   CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t npd = 1;
   VkPhysicalDevice pd;
   vkEnumeratePhysicalDevices(inst, &npd, &pd);
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   printf("device: %s\n", props.deviceName);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                  .queueFamilyIndex = 0,
                                  .queueCount = 1,
                                  .pQueuePriorities = &prio};
   VkPhysicalDeviceVulkan12Features f12 = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .bufferDeviceAddress = 1};
   VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                             .pNext = &f12,
                             .queueCreateInfoCount = 1,
                             .pQueueCreateInfos = &qci};
   VkDevice dev;
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q;
   vkGetDeviceQueue(dev, 0, 0, &q);

   VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                             .size = 65536,
                             .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT |
                                      VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT};
   VkBuffer buf;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, buf, &mr);
   VkMemoryAllocateFlagsInfo mafi = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
                                     .flags = VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT};
   VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                               .pNext = &mafi,
                               .allocationSize = mr.size,
                               .memoryTypeIndex = __builtin_ctz(mr.memoryTypeBits)};
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
   CHECK(vkBindBufferMemory(dev, buf, mem, 0));
   VkBufferDeviceAddressInfo bdai = {.sType = VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
                                     .buffer = buf};
   VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));

   uint64_t *t = malloc(sizeof(*t) * iters);
   const uint64_t gaps[] = {0, 200000, 5000000};
   for (unsigned g = 0; g < sizeof(gaps) / sizeof(gaps[0]); g++) {
      /* the parked case is slow per sample; cap its count */
      int n = gaps[g] >= 1000000 ? (iters < 400 ? iters : 400) : iters;
      uint64_t acc = 0;
      for (int i = 0; i < n; i++) {
         gap(gaps[g]);
         uint64_t a = now_ns();
         acc += vkGetBufferDeviceAddress(dev, &bdai);
         t[i] = now_ns() - a;
      }
      report("query", gaps[g], t, n);
      for (int i = 0; i < n; i++) {
         gap(gaps[g]);
         uint64_t a = now_ns();
         CHECK(vkQueueSubmit(q, 0, NULL, fence));
         CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
         t[i] = now_ns() - a;
         CHECK(vkResetFences(dev, 1, &fence));
      }
      report("fence", gaps[g], t, n);
      if (acc == 1)
         printf("unreachable\n");
   }
   vkDeviceWaitIdle(dev);
   return 0;
}
