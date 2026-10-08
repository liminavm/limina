/* Controls for the validation layer's binary-semaphore tracking.
 *   double-signal  signal a binary semaphore in two submits, no wait between
 *                  (invalid: expect an error)
 *   unsignaled     wait+signal the same semaphore in one batch, nothing ever
 *                  signaled it (invalid: expect an error)
 *   resignal       signal it, then wait+signal it in one batch, no import
 *                  (the pattern under question, plain payload)
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "FAILED: %s\n", #x); exit(2); } } while (0)

int
main(int argc, char **argv)
{
   const char *mode = argv[1];
   const VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3 };
   const VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   VkInstance instance;
   CHECK(vkCreateInstance(&ici, NULL, &instance) == VK_SUCCESS);
   uint32_t count = 1;
   VkPhysicalDevice pdev;
   CHECK(vkEnumeratePhysicalDevices(instance, &count, &pdev) >= 0 && count == 1);
   const float prio = 1.0f;
   const VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &prio };
   const VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   VkDevice device;
   CHECK(vkCreateDevice(pdev, &dci, NULL, &device) == VK_SUCCESS);
   VkQueue queue;
   vkGetDeviceQueue(device, 0, 0, &queue);
   const VkSemaphoreCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   VkSemaphore sem;
   CHECK(vkCreateSemaphore(device, &sci, NULL, &sem) == VK_SUCCESS);

   const VkPipelineStageFlags stage = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
   const VkSubmitInfo signal = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .signalSemaphoreCount = 1, .pSignalSemaphores = &sem };
   const VkSubmitInfo resignal = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .waitSemaphoreCount = 1, .pWaitSemaphores = &sem, .pWaitDstStageMask = &stage,
      .signalSemaphoreCount = 1, .pSignalSemaphores = &sem,
   };
   const VkSubmitInfo wait = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = 1, .pWaitSemaphores = &sem, .pWaitDstStageMask = &stage };

   if (!strcmp(mode, "double-signal")) {
      vkQueueSubmit(queue, 1, &signal, VK_NULL_HANDLE);
      vkQueueSubmit(queue, 1, &signal, VK_NULL_HANDLE);
      vkQueueSubmit(queue, 1, &wait, VK_NULL_HANDLE);
   } else if (!strcmp(mode, "unsignaled")) {
      vkQueueSubmit(queue, 1, &resignal, VK_NULL_HANDLE);
   } else if (!strcmp(mode, "resignal")) {
      vkQueueSubmit(queue, 1, &signal, VK_NULL_HANDLE);
      vkQueueSubmit(queue, 1, &resignal, VK_NULL_HANDLE);
      vkQueueSubmit(queue, 1, &wait, VK_NULL_HANDLE);
   } else {
      CHECK(!"mode");
   }
   vkQueueWaitIdle(queue);
   printf("%s done\n", mode);
   vkDestroySemaphore(device, sem, NULL);
   vkDestroyDevice(device, NULL);
   vkDestroyInstance(instance, NULL);
   return 0;
}
