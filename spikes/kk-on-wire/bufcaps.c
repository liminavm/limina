/* What every Vulkan device in the guest says about sharing a VkBuffer as a dma-buf: per device,
 * the external-memory features for a storage + transfer buffer, and its memory types. Run with
 * VK_DRIVER_FILES naming the ICDs to compare (venus's and KosmicKrisp's). */
#include <stdio.h>
#include <stdlib.h>
#include <vulkan/vulkan.h>

int
main(void)
{
   VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3};
   VkInstanceCreateInfo ii = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
   VkInstance inst;
   if (vkCreateInstance(&ii, NULL, &inst) != VK_SUCCESS)
      return 1;
   uint32_t n = 0;
   vkEnumeratePhysicalDevices(inst, &n, NULL);
   VkPhysicalDevice *pd = calloc(n, sizeof(*pd));
   vkEnumeratePhysicalDevices(inst, &n, pd);
   const struct {
      VkExternalMemoryHandleTypeFlagBits t;
      const char *name;
   } types[] = {{VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, "dma-buf"},
                {VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT, "opaque-fd"},
                {VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT, "host-alloc"}};
   for (uint32_t i = 0; i < n; i++) {
      VkPhysicalDeviceProperties p;
      vkGetPhysicalDeviceProperties(pd[i], &p);
      printf("%s\n", p.deviceName);
      for (unsigned t = 0; t < sizeof(types) / sizeof(types[0]); t++) {
         VkPhysicalDeviceExternalBufferInfo bi = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_BUFFER_INFO,
            .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
                     VK_BUFFER_USAGE_TRANSFER_DST_BIT,
            .handleType = types[t].t};
         VkExternalBufferProperties ep = {.sType = VK_STRUCTURE_TYPE_EXTERNAL_BUFFER_PROPERTIES};
         vkGetPhysicalDeviceExternalBufferProperties(pd[i], &bi, &ep);
         const VkExternalMemoryFeatureFlags f = ep.externalMemoryProperties.externalMemoryFeatures;
         printf("  %-10s export=%d import=%d dedicated-only=%d\n", types[t].name,
                !!(f & VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT),
                !!(f & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT),
                !!(f & VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT));
      }
      VkPhysicalDeviceMemoryProperties mp;
      vkGetPhysicalDeviceMemoryProperties(pd[i], &mp);
      for (uint32_t m = 0; m < mp.memoryTypeCount; m++)
         printf("  memory type %u: flags 0x%x\n", m, mp.memoryTypes[m].propertyFlags);
   }
   vkDestroyInstance(inst, NULL);
   return 0;
}
