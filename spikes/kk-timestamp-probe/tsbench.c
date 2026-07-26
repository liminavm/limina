// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * tsbench — what do timestamp queries COST on KosmicKrisp?
 *
 * `0013` moved the timestamp resolve from the GPU to the CPU, which buys
 * correctness (see RESULTS.md) but changes command-buffer structure: a
 * timestamp followed by a consumer in the same submission now retires the
 * sampling command buffer and makes the next one wait, on the GPU, for an event
 * the CPU signals from the completion handler. That is a GPU->CPU->GPU hop
 * inserted into the frame. This measures it instead of guessing.
 *
 * Three shapes, because the cost is not uniform:
 *
 *   none  — no timestamps at all. The control: this path is untouched by 0013,
 *           so any difference here is noise, not the change.
 *   ts    — two timestamps per submit, results read on the CPU afterwards. No
 *           in-stream consumer, so NO barrier and no split.
 *   tscp  — two timestamps + an in-stream vkCmdCopyQueryPoolResults, i.e. what
 *           venus produces for every guest vkGetQueryPoolResults (its
 *           query-feedback command buffer rides in the same submission). This
 *           is the shape that pays for the barrier.
 *
 * Two timing modes, because they answer different questions:
 *
 *   latency    — submit, wait, repeat. What one frame's round trip costs.
 *   throughput — submit DEPTH frames, then wait. Whether the barrier serialises
 *                the queue and eats overlap.
 *
 * Build/run: ./tsbench.sh [iters]   (add KK_ICD=… to point at another driver)
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
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

enum shape { SHAPE_NONE, SHAPE_TS, SHAPE_TSCP };
static const char *shape_name[] = { "none", "ts  ", "tscp" };

static double
now_ms(void)
{
   struct timespec t;
   clock_gettime(CLOCK_MONOTONIC, &t);
   return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static int
cmp_double(const void *a, const void *b)
{
   double x = *(const double *)a, y = *(const double *)b;
   return (x > y) - (x < y);
}

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

/* A little real GPU work, so a submit is not purely driver overhead — the
 * question is what the timestamp machinery adds to a frame, not what an empty
 * command buffer costs. */
static VkBuffer work_buf;
static VkDeviceMemory work_mem;
static VkBuffer dst_buf;
static VkDeviceMemory dst_mem;

static void
make_buffers(void)
{
   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = 4u << 20,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   CHECK(vkCreateBuffer(dev, &bci, NULL, &work_buf));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, work_buf, &mr);
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = find_mem_type(mr.memoryTypeBits,
                                       VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT),
   };
   CHECK(vkAllocateMemory(dev, &mai, NULL, &work_mem));
   CHECK(vkBindBufferMemory(dev, work_buf, work_mem, 0));

   bci.size = 4 * sizeof(uint64_t);
   bci.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
   CHECK(vkCreateBuffer(dev, &bci, NULL, &dst_buf));
   vkGetBufferMemoryRequirements(dev, dst_buf, &mr);
   mai.allocationSize = mr.size;
   mai.memoryTypeIndex =
      find_mem_type(mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
   CHECK(vkAllocateMemory(dev, &mai, NULL, &dst_mem));
   CHECK(vkBindBufferMemory(dev, dst_buf, dst_mem, 0));
}

static void
record(VkCommandBuffer cb, VkQueryPool pool, enum shape s)
{
   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkBeginCommandBuffer(cb, &bi));

   if (s != SHAPE_NONE) {
      vkCmdResetQueryPool(cb, pool, 0, 2);
      vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, pool, 0);
   }

   vkCmdFillBuffer(cb, work_buf, 0, 4u << 20, 0x5a5a5a5au);

   if (s != SHAPE_NONE)
      vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, pool, 1);

   if (s == SHAPE_TSCP)
      vkCmdCopyQueryPoolResults(cb, pool, 0, 2, dst_buf, 0, sizeof(uint64_t),
                                VK_QUERY_RESULT_64_BIT |
                                   VK_QUERY_RESULT_WAIT_BIT);

   CHECK(vkEndCommandBuffer(cb));
}

/* latency: submit -> wait -> repeat. Returns median ms per iteration. */
static double
run_latency(enum shape s, int iters, double *p95_out)
{
   VkQueryPool pool = VK_NULL_HANDLE;
   if (s != SHAPE_NONE) {
      VkQueryPoolCreateInfo qi = {
         .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
         .queryType = VK_QUERY_TYPE_TIMESTAMP,
         .queryCount = 2,
      };
      CHECK(vkCreateQueryPool(dev, &qi, NULL, &pool));
   }

   VkCommandBufferAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cmd_pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cb;
   CHECK(vkAllocateCommandBuffers(dev, &ai, &cb));

   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));

   double *samples = calloc(iters, sizeof(double));

   for (int i = 0; i < iters; i++) {
      CHECK(vkResetCommandBuffer(cb, 0));
      record(cb, pool, s);

      double t0 = now_ms();
      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                          .commandBufferCount = 1,
                          .pCommandBuffers = &cb };
      CHECK(vkQueueSubmit(queue, 1, &si, fence));
      CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 5ull * 1000000000ull));
      CHECK(vkResetFences(dev, 1, &fence));

      /* The consumer, for the shape that has no in-stream one. A real app reads
       * its timestamps; leaving that out would measure a frame nobody wants. */
      if (s == SHAPE_TS) {
         uint64_t out[2];
         vkGetQueryPoolResults(dev, pool, 0, 2, sizeof(out), out,
                               sizeof(uint64_t), VK_QUERY_RESULT_64_BIT);
      }
      samples[i] = now_ms() - t0;
   }

   qsort(samples, iters, sizeof(double), cmp_double);
   double median = samples[iters / 2];
   if (p95_out)
      *p95_out = samples[(int)(iters * 0.95)];
   free(samples);

   vkDestroyFence(dev, fence, NULL);
   vkFreeCommandBuffers(dev, cmd_pool, 1, &cb);
   if (pool)
      vkDestroyQueryPool(dev, pool, NULL);
   return median;
}

/* throughput: keep DEPTH submits in flight, wait once at the end. */
#define DEPTH 32
static double
run_throughput(enum shape s, int iters)
{
   VkQueryPool pools[DEPTH] = { 0 };
   VkCommandBuffer cbs[DEPTH];

   VkCommandBufferAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = cmd_pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = DEPTH,
   };
   CHECK(vkAllocateCommandBuffers(dev, &ai, cbs));

   for (int i = 0; i < DEPTH; i++) {
      if (s != SHAPE_NONE) {
         VkQueryPoolCreateInfo qi = {
            .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
            .queryType = VK_QUERY_TYPE_TIMESTAMP,
            .queryCount = 2,
         };
         CHECK(vkCreateQueryPool(dev, &qi, NULL, &pools[i]));
      }
      record(cbs[i], pools[i], s);
   }

   double t0 = now_ms();
   int batches = iters / DEPTH;
   for (int b = 0; b < batches; b++) {
      for (int i = 0; i < DEPTH; i++) {
         VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                             .commandBufferCount = 1,
                             .pCommandBuffers = &cbs[i] };
         CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
      }
      CHECK(vkQueueWaitIdle(queue));
      for (int i = 0; i < DEPTH; i++) {
         CHECK(vkResetCommandBuffer(cbs[i], 0));
         record(cbs[i], pools[i], s);
      }
   }
   double total = now_ms() - t0;

   vkFreeCommandBuffers(dev, cmd_pool, DEPTH, cbs);
   for (int i = 0; i < DEPTH; i++)
      if (pools[i])
         vkDestroyQueryPool(dev, pools[i], NULL);
   return total / (batches * DEPTH);
}

int
main(int argc, char **argv)
{
   int iters = argc > 1 ? atoi(argv[1]) : 2000;

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .apiVersion = VK_API_VERSION_1_3 };
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
   make_buffers();

   printf("device: %s   iters=%d\n", props.deviceName, iters);
   printf("shape  latency-median  latency-p95   throughput\n");
   for (int s = SHAPE_NONE; s <= SHAPE_TSCP; s++) {
      run_latency(s, 200, NULL); /* warm up */
      double p95 = 0;
      double lat = run_latency(s, iters, &p95);
      double thr = run_throughput(s, iters);
      printf("%s   %8.4f ms    %8.4f ms   %8.4f ms\n", shape_name[s], lat, p95,
             thr);
      fflush(stdout);
   }
   return 0;
}
