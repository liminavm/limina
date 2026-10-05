/* venus: a submission that waits and signals the same binary semaphore,
 * which holds a temporarily imported sync fd, crashes in virtgpu_submit.
 *
 * vn_queue_submission_count_semaphore counts the signal semaphore as needing
 * a renderer sync because it still holds the imported payload. The wait then
 * restores its permanent payload, vn_queue_submission_init_syncs skips it,
 * and the submission goes out with sync_count one larger than the number of
 * syncs actually filled in.
 *
 * Build: cc -o venus-sync-count venus-sync-count.c -lvulkan
 * Run:   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json \
 *            ./venus-sync-count
 *
 * Unfixed: SIGSEGV in virtgpu_submit.
 * Fixed:   "submission completed", exit 0.
 */
#include <stdio.h>
#include <stdlib.h>
#include <vulkan/vulkan.h>

#define CHECK(x)                                                               \
   do {                                                                        \
      if (!(x)) {                                                              \
         fprintf(stderr, "FAILED: %s\n", #x);                                  \
         exit(2);                                                              \
      }                                                                        \
   } while (0)

int
main(void)
{
   const VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .apiVersion = VK_API_VERSION_1_1,
   };
   const VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance instance;
   CHECK(vkCreateInstance(&ici, NULL, &instance) == VK_SUCCESS);

   uint32_t count = 8;
   VkPhysicalDevice pdevs[8];
   CHECK(vkEnumeratePhysicalDevices(instance, &count, pdevs) >= 0);

   VkPhysicalDevice pdev = VK_NULL_HANDLE;
   for (uint32_t i = 0; i < count; i++) {
      VkPhysicalDeviceDriverProperties driver = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
      };
      VkPhysicalDeviceProperties2 props = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
         .pNext = &driver,
      };
      vkGetPhysicalDeviceProperties2(pdevs[i], &props);
      if (driver.driverID == VK_DRIVER_ID_MESA_VENUS) {
         printf("device: %s\n", props.properties.deviceName);
         pdev = pdevs[i];
         break;
      }
   }
   CHECK(pdev != VK_NULL_HANDLE);

   const char *exts[] = { VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME };
   const float prio = 1.0f;
   const VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = 0,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   const VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = exts,
   };
   VkDevice device;
   CHECK(vkCreateDevice(pdev, &dci, NULL, &device) == VK_SUCCESS);

   VkQueue queue;
   vkGetDeviceQueue(device, 0, 0, &queue);

   PFN_vkImportSemaphoreFdKHR import_fd = (PFN_vkImportSemaphoreFdKHR)
      vkGetDeviceProcAddr(device, "vkImportSemaphoreFdKHR");
   CHECK(import_fd);

   const VkSemaphoreCreateInfo sci = {
      .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
   };
   VkSemaphore sem;
   CHECK(vkCreateSemaphore(device, &sci, NULL, &sem) == VK_SUCCESS);

   /* -1 is an already-signaled sync file. */
   const VkImportSemaphoreFdInfoKHR import = {
      .sType = VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR,
      .semaphore = sem,
      .flags = VK_SEMAPHORE_IMPORT_TEMPORARY_BIT,
      .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
      .fd = -1,
   };
   CHECK(import_fd(device, &import) == VK_SUCCESS);

   const VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   CHECK(vkCreateFence(device, &fci, NULL, &fence) == VK_SUCCESS);

   /* Waiting unsignals the semaphore, so signaling it again in the same
    * submission is valid. */
   const VkPipelineStageFlags stage = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
   const VkSubmitInfo submit = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .waitSemaphoreCount = 1,
      .pWaitSemaphores = &sem,
      .pWaitDstStageMask = &stage,
      .signalSemaphoreCount = 1,
      .pSignalSemaphores = &sem,
   };
   CHECK(vkQueueSubmit(queue, 1, &submit, fence) == VK_SUCCESS);
   CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, 5000000000ull) ==
         VK_SUCCESS);
   printf("submission completed\n");

   vkDestroyFence(device, fence, NULL);
   vkDestroySemaphore(device, sem, NULL);
   vkDestroyDevice(device, NULL);
   vkDestroyInstance(instance, NULL);
   return 0;
}
