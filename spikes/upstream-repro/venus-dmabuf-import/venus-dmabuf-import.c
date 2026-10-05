/* venus: a dma-buf import the host refuses is reported as success.
 *
 * vn_device_memory_import_dma_buf allocates the imported memory through
 * the asynchronous path, so vkAllocateMemory returns VK_SUCCESS as soon as
 * the command is queued. If the renderer then fails the import, the
 * application holds a VkDeviceMemory the host never created, and the next
 * command naming it fails renderer-side and takes the whole context down.
 *
 * A buffer allocated through GBM on virtio-gpu is a virgl (GL) resource,
 * which virglrenderer's venus context cannot import, so importing its
 * dma-buf into venus is a host-side refusal on any virglrenderer host.
 *
 * Build: cc -o venus-dmabuf-import venus-dmabuf-import.c -lvulkan -lgbm
 * Run:   VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json \
 *            ./venus-dmabuf-import
 *
 * Unfixed: vkAllocateMemory returns VK_SUCCESS, and the following
 *          vkBindBufferMemory/submit loses the device.
 * Fixed:   vkAllocateMemory returns an error and the device keeps working.
 */
#include <fcntl.h>
#include <gbm.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#define CHECK(x)                                                               \
   do {                                                                        \
      if (!(x)) {                                                              \
         fprintf(stderr, "FAILED: %s\n", #x);                                  \
         exit(2);                                                              \
      }                                                                        \
   } while (0)

static VkDevice device;
static VkQueue queue;

/* A trivial submission with a fence: shows whether the device still works. */
static VkResult
probe_device(void)
{
   const VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   VkResult r = vkCreateFence(device, &fci, NULL, &fence);
   if (r != VK_SUCCESS)
      return r;
   r = vkQueueSubmit(queue, 0, NULL, fence);
   if (r == VK_SUCCESS)
      r = vkWaitForFences(device, 1, &fence, VK_TRUE, 5000000000ull);
   vkDestroyFence(device, fence, NULL);
   return r;
}

int
main(int argc, char **argv)
{
   setvbuf(stdout, NULL, _IONBF, 0);
   /* The renderer fails vkGetMemoryFdPropertiesKHR on such a buffer as a
    * command-stream error, which also loses the device; pass --query to see
    * that, otherwise memory type 0 is used directly. */
   const int query = argc > 1;
   (void)argv;
   int drm_fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
   CHECK(drm_fd >= 0);
   struct gbm_device *gbm = gbm_create_device(drm_fd);
   CHECK(gbm);
   struct gbm_bo *gbo = gbm_bo_create(gbm, 256, 256, GBM_FORMAT_ARGB8888,
                                      GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
   CHECK(gbo);
   const int dmabuf = gbm_bo_get_fd(gbo);
   CHECK(dmabuf >= 0);
   const VkDeviceSize size = lseek(dmabuf, 0, SEEK_END);
   printf("gbm bo: %s, dma-buf %lld bytes\n", gbm_device_get_backend_name(gbm),
          (long long)size);

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

   const char *exts[] = {
      VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
      VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
   };
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
      .enabledExtensionCount = 2,
      .ppEnabledExtensionNames = exts,
   };
   CHECK(vkCreateDevice(pdev, &dci, NULL, &device) == VK_SUCCESS);
   vkGetDeviceQueue(device, 0, 0, &queue);
   printf("device check before import: %d\n", probe_device());

   uint32_t type = 0;
   VkResult r;
   if (query) {
      PFN_vkGetMemoryFdPropertiesKHR get_fd_props =
         (PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(
            device, "vkGetMemoryFdPropertiesKHR");
      CHECK(get_fd_props);
      VkMemoryFdPropertiesKHR fd_props = {
         .sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR,
      };
      r = get_fd_props(device,
                                VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                dmabuf, &fd_props);
      printf("vkGetMemoryFdPropertiesKHR: %d, memoryTypeBits 0x%x\n", r,
             fd_props.memoryTypeBits);
      while (fd_props.memoryTypeBits &&
             !(fd_props.memoryTypeBits & (1u << type)))
         type++;
   }

   const VkExternalMemoryBufferCreateInfo ext_buf = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
   };
   const VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .pNext = &ext_buf,
      .size = 4096,
      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
   };
   VkBuffer buffer;
   CHECK(vkCreateBuffer(device, &bci, NULL, &buffer) == VK_SUCCESS);

   const VkImportMemoryFdInfoKHR import = {
      .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
      .fd = dup(dmabuf),
   };
   const VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &import,
      .allocationSize = size,
      .memoryTypeIndex = type,
   };
   VkDeviceMemory mem;
   r = vkAllocateMemory(device, &mai, NULL, &mem);
   printf("vkAllocateMemory(import, type %u): %d\n", type, r);

   if (r == VK_SUCCESS) {
      r = vkBindBufferMemory(device, buffer, mem, 0);
      printf("vkBindBufferMemory: %d\n", r);
   }

   const VkResult after = probe_device();
   printf("device check after import: %d\n", after);
   return after == VK_SUCCESS ? 0 : 1;
}
