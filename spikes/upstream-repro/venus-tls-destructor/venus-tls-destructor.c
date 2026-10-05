/* venus: thread exit after vkDestroyInstance calls a TLS destructor in an
 * unloaded ICD.
 *
 * A worker thread creates a VkInstance and a VkDevice on the first venus
 * physical device, destroys both and returns. vkCreateDevice makes venus
 * allocate its per-thread state behind a tss key whose destructor lives in
 * libvulkan_virtio.so; vkDestroyInstance lets the loader dlclose() the ICD;
 * the thread then exits and glibc runs the destructor through a pointer into
 * the unmapped library.
 *
 * Build: cc -o venus-tls-destructor venus-tls-destructor.c -lvulkan -lpthread
 * Run:   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json \
 *            ./venus-tls-destructor
 *
 * Unfixed: SIGSEGV (exit 139) after "worker done".
 * Fixed:   "worker joined", exit 0.
 */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define CHECK(x)                                                               \
   do {                                                                        \
      if (!(x)) {                                                              \
         fprintf(stderr, "FAILED: %s\n", #x);                                  \
         exit(2);                                                              \
      }                                                                        \
   } while (0)

static void *
worker(void *arg)
{
   (void)arg;

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
   };
   VkDevice device;
   CHECK(vkCreateDevice(pdev, &dci, NULL, &device) == VK_SUCCESS);

   vkDestroyDevice(device, NULL);
   vkDestroyInstance(instance, NULL);

   printf("worker done\n");
   fflush(stdout);
   return NULL;
}

int
main(void)
{
   pthread_t t;
   CHECK(pthread_create(&t, NULL, worker, NULL) == 0);
   CHECK(pthread_join(t, NULL) == 0);
   printf("worker joined\n");
   return 0;
}
