/* Drive KosmicKrisp's command-allocator pool past any ceiling, host-only.
 *
 *   poolprobe open  <n>               begin n command buffers and never end them: one borrowed
 *                                     allocator each, so the pool must hold n at once
 *   poolprobe stall <n> <fills> <ms>  n submits that the GPU cannot run until a host thread
 *                                     signals a timeline semaphore <ms> after start; each recording
 *                                     is <fills> vkCmdFillBuffer, enough to push its allocator over
 *                                     a small LIMINA_KK_ALLOC_BUDGET_MIB so it drains (pending) and
 *                                     the pool must mint. <ms>=0 never signals until the end.
 *
 *   poolprobe resubmit <n> <fills> <ms>  like stall, but ONE command buffer (no ONE_TIME_SUBMIT)
 *                                     submitted n times: every submit after the first is
 *                                     re-recorded by KK's queue, which takes its own allocator
 *
 * Prints per-begin results and slow begins; the pool's own [LIMINA-ALLOC-*] lines go to stderr. */
#include <vulkan/vulkan.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define CHECK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { \
   fprintf(stderr, "%s:%d %s -> %d\n", __FILE__, __LINE__, #x, r_); exit(1); } } while (0)

static double now_ms(void) {
   struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static VkDevice dev;
static VkSemaphore sem;
static int signal_ms;

static void *signaller(void *arg) {
   (void)arg;
   usleep(signal_ms * 1000);
   VkSemaphoreSignalInfo si = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO, .semaphore = sem, .value = 1 };
   fprintf(stderr, "probe: signalling at %d ms\n", signal_ms);
   vkSignalSemaphore(dev, &si);
   return NULL;
}

int main(int argc, char **argv) {
   if (argc < 3) { fprintf(stderr, "usage: see source\n"); return 2; }
   const char *mode = argv[1];
   int n = atoi(argv[2]);
   int fills = argc > 3 ? atoi(argv[3]) : 0;
   signal_ms = argc > 4 ? atoi(argv[4]) : 0;

   const char *iext[] = { VK_KHR_PORTABILITY_ENUMERATION_EXTENSION_NAME };
   VkApplicationInfo ai = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &ai,
      .flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR, .enabledExtensionCount = 1, .ppEnabledExtensionNames = iext };
   VkInstance inst; CHECK(vkCreateInstance(&ici, NULL, &inst));
   uint32_t npd = 1; VkPhysicalDevice pd; vkEnumeratePhysicalDevices(inst, &npd, &pd);
   if (!npd) { fprintf(stderr, "no device\n"); return 1; }

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &prio };
   VkPhysicalDeviceVulkan12Features f12 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .timelineSemaphore = VK_TRUE };
   const char *dext[] = { "VK_KHR_portability_subset" };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f12, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 0, .ppEnabledExtensionNames = dext };
   CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
   VkQueue q; vkGetDeviceQueue(dev, 0, 0, &q);

   VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT };
   VkCommandPool pool; CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
   VkCommandBuffer *cbs = calloc(n, sizeof(*cbs));
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = n };
   CHECK(vkAllocateCommandBuffers(dev, &cai, cbs));
   VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };

   int ok = 0, failed = 0; double worst = 0, t0 = now_ms();

   if (!strcmp(mode, "open")) {
      for (int i = 0; i < n; i++) {
         double b = now_ms();
         VkResult r = vkBeginCommandBuffer(cbs[i], &bi);
         double d = now_ms() - b; if (d > worst) worst = d;
         if (r == VK_SUCCESS) ok++;
         else { if (!failed) printf("first failure at begin %d: VkResult %d after %.1f ms\n", i, r, d); failed++; }
      }
      /* The fail path: End and Reset every one, including those whose Begin failed. */
      for (int i = 0; i < n; i++) vkEndCommandBuffer(cbs[i]);
      for (int i = 0; i < n; i++) vkResetCommandBuffer(cbs[i], 0);
      /* And the pool still serves after all of that. */
      CHECK(vkBeginCommandBuffer(cbs[0], &bi));
      CHECK(vkEndCommandBuffer(cbs[0]));
      VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cbs[0] };
      CHECK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE));
      CHECK(vkQueueWaitIdle(q));
   } else if (!strcmp(mode, "stall") || !strcmp(mode, "resubmit")) {
      int resubmit = !strcmp(mode, "resubmit");
      VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = 1 << 16, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT };
      VkBuffer buf; CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
      VkMemoryRequirements mr; vkGetBufferMemoryRequirements(dev, buf, &mr);
      VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size, .memoryTypeIndex = __builtin_ctz(mr.memoryTypeBits) };
      VkDeviceMemory mem; CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
      CHECK(vkBindBufferMemory(dev, buf, mem, 0));

      VkSemaphoreTypeCreateInfo stci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO, .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE };
      VkSemaphoreCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &stci };
      CHECK(vkCreateSemaphore(dev, &sci, NULL, &sem));
      pthread_t th;
      if (signal_ms) pthread_create(&th, NULL, signaller, NULL);

      uint64_t one = 1;
      VkPipelineStageFlags ws = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
      VkTimelineSemaphoreSubmitInfo tsi = { .sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO, .waitSemaphoreValueCount = 1, .pWaitSemaphoreValues = &one };
      if (resubmit) {
         VkCommandBufferBeginInfo rbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
         CHECK(vkBeginCommandBuffer(cbs[0], &rbi));
         for (int k = 0; k < fills; k++) vkCmdFillBuffer(cbs[0], buf, (k % 256) * 256, 256, k);
         CHECK(vkEndCommandBuffer(cbs[0]));
         VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &tsi, .waitSemaphoreCount = 1, .pWaitSemaphores = &sem,
            .pWaitDstStageMask = &ws, .commandBufferCount = 1, .pCommandBuffers = &cbs[0] };
         for (int i = 0; i < n; i++) {
            double b = now_ms();
            VkResult r = vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE);
            double d = now_ms() - b; if (d > worst) worst = d;
            if (d > 20) printf("submit %d took %.1f ms (VkResult %d)\n", i, d, r);
            if (r == VK_SUCCESS) ok++;
            else { if (!failed) printf("first failure at submit %d: VkResult %d after %.1f ms\n", i, r, d); failed++; }
         }
      }
      for (int i = 0; i < n && !resubmit; i++) {
         double b = now_ms();
         VkResult r = vkBeginCommandBuffer(cbs[i], &bi);
         double d = now_ms() - b; if (d > worst) worst = d;
         if (d > 20) printf("begin %d took %.1f ms (VkResult %d)\n", i, d, r);
         if (r != VK_SUCCESS) {
            if (!failed) printf("first failure at begin %d: VkResult %d after %.1f ms\n", i, r, d);
            failed++;
            vkEndCommandBuffer(cbs[i]);
            continue;
         }
         ok++;
         for (int k = 0; k < fills; k++) vkCmdFillBuffer(cbs[i], buf, (k % 256) * 256, 256, k);
         CHECK(vkEndCommandBuffer(cbs[i]));
         VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &tsi, .waitSemaphoreCount = 1, .pWaitSemaphores = &sem,
            .pWaitDstStageMask = &ws, .commandBufferCount = 1, .pCommandBuffers = &cbs[i] };
         CHECK(vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE));
      }
      if (signal_ms) pthread_join(th, NULL);
      else {
         VkSemaphoreSignalInfo ssi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO, .semaphore = sem, .value = 1 };
         vkSignalSemaphore(dev, &ssi);
      }
      VkResult wr = vkQueueWaitIdle(q);
      printf("vkQueueWaitIdle -> %d\n", wr);
      if (resubmit) {
         /* The queue must still take and complete work after refusals. */
         VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cbs[0] };
         VkResult r1 = vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE);
         VkResult r2 = vkQueueWaitIdle(q);
         printf("after: resubmit -> %d, wait -> %d\n", r1, r2);
      }
   } else {
      fprintf(stderr, "unknown mode\n"); return 2;
   }
   printf("RESULT mode=%s n=%d ok=%d failed=%d worst_begin=%.1f ms total=%.0f ms\n", mode, n, ok, failed, worst, now_ms() - t0);
   fflush(stdout);
   vkDeviceWaitIdle(dev);
   vkFreeCommandBuffers(dev, pool, n, cbs);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return 0;
}
