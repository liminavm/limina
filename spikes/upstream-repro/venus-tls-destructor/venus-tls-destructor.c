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
 *            ./venus-tls-destructor [mode]
 *
 * Modes:
 *   thread (default)  the crash above.
 *                     Unfixed: SIGSEGV (exit 139) after "worker done".
 *                     Fixed:   "worker joined", exit 0.
 *   unload            thread, then report whether the driver is still mapped
 *                     after one more instance create/destroy (which gives the
 *                     loader a dlclose to unload it on).
 *   cycle             100 threads in turn, each with its own instance and
 *                     device, then the same unload report.
 *   main-unload       the main thread creates and destroys an instance and a
 *                     device, then reports whether the driver is still
 *                     mapped. A fix that registers a thread-exit teardown on
 *                     the main thread holds the driver here ("yes").
 *   main-alive        the main thread creates an instance and a device and
 *                     returns from main() without destroying them: exit()
 *                     with live venus state on the main thread.
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

static VkInstance
create_instance(void)
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
   return instance;
}

static VkDevice
create_device(VkInstance instance, int verbose)
{
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
         if (verbose)
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
   return device;
}

static void *
worker(void *arg)
{
   const int verbose = arg != NULL;

   VkInstance instance = create_instance();
   VkDevice device = create_device(instance, verbose);
   vkDestroyDevice(device, NULL);
   vkDestroyInstance(instance, NULL);

   if (verbose) {
      printf("worker done\n");
      fflush(stdout);
   }
   return NULL;
}

static void
run_worker(int verbose)
{
   pthread_t t;
   CHECK(pthread_create(&t, NULL, worker, verbose ? "v" : NULL) == 0);
   CHECK(pthread_join(t, NULL) == 0);
}

static int
driver_mapped(void)
{
   FILE *maps = fopen("/proc/self/maps", "r");
   CHECK(maps);
   char line[4096];
   int found = 0;
   while (fgets(line, sizeof(line), maps))
      found |= strstr(line, "libvulkan_virtio.so") != NULL;
   fclose(maps);
   return found;
}

static void
report_unload(void)
{
   vkDestroyInstance(create_instance(), NULL);
   printf("driver mapped after the last instance: %s\n",
          driver_mapped() ? "yes" : "no");
}

int
main(int argc, char **argv)
{
   const char *mode = argc > 1 ? argv[1] : "thread";

   if (!strcmp(mode, "thread")) {
      run_worker(1);
   } else if (!strcmp(mode, "unload")) {
      run_worker(1);
      report_unload();
   } else if (!strcmp(mode, "cycle")) {
      for (int i = 0; i < 100; i++)
         run_worker(0);
      printf("100 workers done\n");
      report_unload();
   } else if (!strcmp(mode, "main-unload")) {
      worker("v");
      printf("driver mapped after the last instance: %s\n",
             driver_mapped() ? "yes" : "no");
      return 0;
   } else if (!strcmp(mode, "main-alive")) {
      VkInstance instance = create_instance();
      create_device(instance, 1);
      printf("returning from main with a live device\n");
      return 0;
   } else {
      fprintf(stderr, "unknown mode %s\n", mode);
      return 2;
   }

   printf("worker joined\n");
   return 0;
}
