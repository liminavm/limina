// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * fdtruth — is an exported sync_file fence truthful about GPU completion?
 *
 * venus-explicit-sync-gap.md §6.2: DRM_CAP_SYNCOBJ_TIMELINE=1 proves the API
 * exists, not that a guest sync_file backed by a venus submission signals at
 * host *GPU completion* rather than at decode/submit. This probe measures it:
 *
 *   1. Baseline: submit N serialized big buffer copies with a plain VkFence,
 *      wall-time vkWaitForFences  -> gpu_ms (how long the work really takes).
 *   2. Truth run: same workload with a SYNC_FD-exportable fence + signal
 *      semaphore. Export both fds right after vkQueueSubmit, then poll(2)
 *      each until POLLIN                -> fence_fd_ms / sem_fd_ms.
 *
 * Verdict: an fd that signals in ~0 ms while the GPU needs ~gpu_ms is LYING —
 * exactly what would tear under NIRI_VK_ASYNC_SCANOUT (atomic IN_FENCE_FD).
 * A truthful fd signals at ~gpu_ms.
 *
 * Build (guest):  cc -O2 -Wall -o fdtruth fdtruth.c -lvulkan
 * Run   (guest):  VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json ./fdtruth
 * Also meaningful on the host against KK directly (sync_file absent there, so
 * export will fail cleanly — the guest is the target).
 */

#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
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

/* Poll a sync_file fd until POLLIN; returns ms from t0 (or -1 on error). */
static double
wait_fd_signaled(int fd, double t0, double timeout_ms, int *initially)
{
   struct pollfd pfd = { .fd = fd, .events = POLLIN };
   int r = poll(&pfd, 1, 0);
   *initially = (r > 0 && (pfd.revents & POLLIN)) ? 1 : 0;
   for (;;) {
      if (now_ms() - t0 > timeout_ms)
         return -1.0;
      pfd.revents = 0;
      r = poll(&pfd, 1, 5);
      if (r < 0)
         return -1.0;
      if (r > 0 && (pfd.revents & POLLIN))
         return now_ms() - t0;
   }
}

static uint32_t
find_mem_type(VkPhysicalDevice phys, uint32_t type_bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(phys, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((type_bits & (1u << i)) &&
          (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type\n");
   exit(1);
}

struct buf {
   VkBuffer b;
   VkDeviceMemory m;
};

static struct buf
make_buf(VkDevice dev, VkPhysicalDevice phys, VkDeviceSize size)
{
   struct buf out;
   VkBufferCreateInfo bi = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = size,
      .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   CHECK(vkCreateBuffer(dev, &bi, NULL, &out.b));
   VkMemoryRequirements mr;
   vkGetBufferMemoryRequirements(dev, out.b, &mr);
   VkMemoryAllocateInfo ai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = find_mem_type(phys, mr.memoryTypeBits,
                                       VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
   };
   CHECK(vkAllocateMemory(dev, &ai, NULL, &out.m));
   CHECK(vkBindBufferMemory(dev, out.b, out.m, 0));
   return out;
}

int
main(int argc, char **argv)
{
   setvbuf(stdout, NULL, _IOLBF, 0);
   VkDeviceSize buf_mib = 64;
   uint32_t copies = 256;
   if (argc > 1)
      copies = (uint32_t)atoi(argv[1]);
   if (argc > 2)
      buf_mib = (VkDeviceSize)atoi(argv[2]);
   VkDeviceSize size = buf_mib << 20;

   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .pApplicationName = "fdtruth",
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

   /* External fence/semaphore SYNC_FD capabilities (§6.1 while we're here). */
   {
      VkPhysicalDeviceExternalFenceInfo fi = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_FENCE_INFO,
         .handleType = VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT,
      };
      VkExternalFenceProperties fp = {
         .sType = VK_STRUCTURE_TYPE_EXTERNAL_FENCE_PROPERTIES,
      };
      vkGetPhysicalDeviceExternalFenceProperties(phys, &fi, &fp);
      printf("fence SYNC_FD: export=%d import=%d\n",
             !!(fp.externalFenceFeatures & VK_EXTERNAL_FENCE_FEATURE_EXPORTABLE_BIT),
             !!(fp.externalFenceFeatures & VK_EXTERNAL_FENCE_FEATURE_IMPORTABLE_BIT));
      VkPhysicalDeviceExternalSemaphoreInfo si = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_INFO,
         .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
      };
      VkExternalSemaphoreProperties sp = {
         .sType = VK_STRUCTURE_TYPE_EXTERNAL_SEMAPHORE_PROPERTIES,
      };
      vkGetPhysicalDeviceExternalSemaphoreProperties(phys, &si, &sp);
      printf("semaphore SYNC_FD: export=%d import=%d\n",
             !!(sp.externalSemaphoreFeatures & VK_EXTERNAL_SEMAPHORE_FEATURE_EXPORTABLE_BIT),
             !!(sp.externalSemaphoreFeatures & VK_EXTERNAL_SEMAPHORE_FEATURE_IMPORTABLE_BIT));
   }

   uint32_t qfam = 0;
   {
      uint32_t qn = 0;
      vkGetPhysicalDeviceQueueFamilyProperties(phys, &qn, NULL);
      VkQueueFamilyProperties *qp = calloc(qn, sizeof(*qp));
      vkGetPhysicalDeviceQueueFamilyProperties(phys, &qn, qp);
      for (uint32_t i = 0; i < qn; i++)
         if (qp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
            qfam = i;
            break;
         }
      free(qp);
   }

   const char *dev_exts[] = {
      "VK_KHR_external_fence_fd",
      "VK_KHR_external_semaphore_fd",
   };
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfam,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 2,
      .ppEnabledExtensionNames = dev_exts,
   };
   VkDevice dev;
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
   VkQueue queue;
   vkGetDeviceQueue(dev, qfam, 0, &queue);

   PFN_vkGetFenceFdKHR pvkGetFenceFdKHR =
      (PFN_vkGetFenceFdKHR)vkGetDeviceProcAddr(dev, "vkGetFenceFdKHR");
   PFN_vkGetSemaphoreFdKHR pvkGetSemaphoreFdKHR =
      (PFN_vkGetSemaphoreFdKHR)vkGetDeviceProcAddr(dev, "vkGetSemaphoreFdKHR");
   if (!pvkGetFenceFdKHR || !pvkGetSemaphoreFdKHR) {
      fprintf(stderr, "no fd export entrypoints\n");
      return 1;
   }

   struct buf a = make_buf(dev, phys, size);
   struct buf b = make_buf(dev, phys, size);

   VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = qfam,
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
      vkCmdCopyBuffer(cb, (i & 1) ? b.b : a.b, (i & 1) ? a.b : b.b, 1, &region);
      vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                           VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &mb, 0, NULL,
                           0, NULL);
   }
   CHECK(vkEndCommandBuffer(cb));

   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cb,
   };

   /* ---- Phase 1: baseline GPU duration with a plain fence ---- */
   double gpu_ms;
   {
      VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
      VkFence f;
      CHECK(vkCreateFence(dev, &fci, NULL, &f));
      double t0 = now_ms();
      CHECK(vkQueueSubmit(queue, 1, &si, f));
      double t_submit = now_ms() - t0;
      printf("baseline: submitted (%.2f ms); waiting fence...\n", t_submit);
      CHECK(vkWaitForFences(dev, 1, &f, VK_TRUE, UINT64_MAX));
      gpu_ms = now_ms() - t0;
      printf("baseline: submit returned in %.2f ms; GPU work took %.1f ms "
             "(%u copies of %llu MiB)\n",
             t_submit, gpu_ms, copies, (unsigned long long)buf_mib);
      vkDestroyFence(dev, f, NULL);
      if (gpu_ms < 20.0)
         printf("WARNING: workload too short for a clear verdict; rerun with "
                "more copies (arg1)\n");
   }

   /* ---- Phase 2: exportable fence + semaphore, poll the fds ---- */
   VkExportFenceCreateInfo efci = {
      .sType = VK_STRUCTURE_TYPE_EXPORT_FENCE_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT,
   };
   VkFenceCreateInfo fci = {
      .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
      .pNext = &efci,
   };
   VkFence fence;
   CHECK(vkCreateFence(dev, &fci, NULL, &fence));

   VkExportSemaphoreCreateInfo esci = {
      .sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
   };
   VkSemaphoreCreateInfo sci = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
      .pNext = &esci,
   };
   VkSemaphore sem;
   CHECK(vkCreateSemaphore(dev, &sci, NULL, &sem));

   si.signalSemaphoreCount = 1;
   si.pSignalSemaphores = &sem;

   double t0 = now_ms();
   CHECK(vkQueueSubmit(queue, 1, &si, fence));

   VkFenceGetFdInfoKHR fgi = {
      .sType = VK_STRUCTURE_TYPE_FENCE_GET_FD_INFO_KHR,
      .fence = fence,
      .handleType = VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT,
   };
   int ffd = -2;
   CHECK(pvkGetFenceFdKHR(dev, &fgi, &ffd));
   double t_fexport = now_ms() - t0;
   /* Poll IMMEDIATELY after export, before anything else can block on the
    * GPU: a POLLIN here while the GPU is mid-workload is the lie. */
   int f_at_export = 0;
   if (ffd >= 0) {
      struct pollfd pfd = { .fd = ffd, .events = POLLIN };
      f_at_export = poll(&pfd, 1, 0) > 0 && (pfd.revents & POLLIN);
   }
   printf("fence fd=%d exported @%.2f ms: %s\n", ffd, t_fexport,
          ffd < 0 ? "sentinel(-1)" : (f_at_export ? "ALREADY SIGNALED" : "pending"));

   /* Bracket the fence fd's signal time BEFORE anything else can block on
    * the GPU (semaphore export was measured to): this is the IN_FENCE
    * truthfulness number. */
   double gpu_ms_est = gpu_ms;
   if (ffd >= 0) {
      int f_init2;
      double f_sig = wait_fd_signaled(ffd, t0, gpu_ms_est * 5 + 5000, &f_init2);
      printf("fence fd signal bracketed: %.1f ms (GPU baseline %.1f ms) -> %s\n",
             f_sig, gpu_ms_est,
             f_sig >= 0 && f_sig < gpu_ms_est * 0.5 ? "LIES-EARLY" : "truthful");
   }

   VkSemaphoreGetFdInfoKHR sgi = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
      .semaphore = sem,
      .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
   };
   int sfd = -2;
   CHECK(pvkGetSemaphoreFdKHR(dev, &sgi, &sfd));
   double t_sexport = now_ms() - t0;
   int s_at_export = 0;
   if (sfd >= 0) {
      struct pollfd pfd = { .fd = sfd, .events = POLLIN };
      s_at_export = poll(&pfd, 1, 0) > 0 && (pfd.revents & POLLIN);
   }
   printf("sem   fd=%d exported @%.2f ms: %s\n", sfd, t_sexport,
          sfd < 0 ? "sentinel(-1)" : (s_at_export ? "ALREADY SIGNALED" : "pending"));

   printf("exported: fence fd=%d @%.2f ms, semaphore fd=%d @%.2f ms after "
          "submit\n",
          ffd, t_fexport, sfd, t_sexport);
   if (ffd < 0 || sfd < 0) {
      printf("fd export returned the already-signaled sentinel (-1) -> "
             "MAXIMAL LIE if GPU still running\n");
   }

   double timeout = gpu_ms * 5 + 5000;
   int f_initial = -1, s_initial = -1;
   double f_ms = ffd >= 0 ? wait_fd_signaled(ffd, t0, timeout, &f_initial)
                          : (t_fexport);
   printf("fence fd: initial=%s signaled at %.1f ms\n",
          ffd < 0 ? "sentinel" : (f_initial ? "SIGNALED" : "pending"), f_ms);
   double s_ms = sfd >= 0 ? wait_fd_signaled(sfd, t0, timeout, &s_initial)
                          : (t_sexport);
   printf("sem   fd: initial=%s signaled at %.1f ms\n",
          sfd < 0 ? "sentinel" : (s_initial ? "SIGNALED" : "pending"), s_ms);

   /* NOT vkWaitForFences(fence): SYNC_FD export resets the fence
    * (vk_fence.c GetFenceFdKHR, spec 1.2.194) — waiting it now blocks forever. */
   CHECK(vkQueueWaitIdle(queue));
   double vk_ms = now_ms() - t0;

   printf("\nresults (ms after submit; GPU baseline %.1f ms):\n", gpu_ms);
   printf("  fence sync_file:     signaled at %8.1f  (already signaled at "
          "export: %s)\n",
          f_ms, ffd < 0 ? "fd=-1 sentinel" : (f_initial ? "YES" : "no"));
   printf("  semaphore sync_file: signaled at %8.1f  (already signaled at "
          "export: %s)\n",
          s_ms, sfd < 0 ? "fd=-1 sentinel" : (s_initial ? "YES" : "no"));
   printf("  vkQueueWaitIdle:     returned at %8.1f\n", vk_ms);

   double honest_floor = gpu_ms * 0.5;
   int fence_lies = f_ms >= 0 && f_ms < honest_floor;
   int sem_lies = s_ms >= 0 && s_ms < honest_floor;
   printf("\nVERDICT: fence fd %s, semaphore fd %s (honest floor %.1f ms)\n",
          fence_lies ? "LIES-EARLY" : "truthful",
          sem_lies ? "LIES-EARLY" : "truthful", honest_floor);

   if (ffd >= 0)
      close(ffd);
   if (sfd >= 0)
      close(sfd);
   vkDestroySemaphore(dev, sem, NULL);
   vkDestroyFence(dev, fence, NULL);
   vkDestroyCommandPool(dev, pool, NULL);
   vkDestroyBuffer(dev, a.b, NULL);
   vkFreeMemory(dev, a.m, NULL);
   vkDestroyBuffer(dev, b.b, NULL);
   vkFreeMemory(dev, b.m, NULL);
   vkDestroyDevice(dev, NULL);
   vkDestroyInstance(inst, NULL);
   return fence_lies || sem_lies ? 2 : 0;
}
