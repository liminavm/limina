// SPDX-License-Identifier: MIT
// Copyright © 2026 Gustavo Noronha Silva
//
// VK_LAYER_LIMINA_transport_select: show the application one device per host GPU, limina's own.
//
// Up to three drivers enumerate the same host GPU: the Metal transport's (KosmicKrisp on the
// limina bridge), limina's venus, and the system Mesa's venus. When the transport's driver
// enumerates a device, every venus device is hidden; otherwise, when limina's venus does (it marks
// its driverInfo "(limina)"), the system's venus is. An application taking the first device, as
// most do, so reaches the best driver the guest has. Where only the system's venus enumerates,
// this layer changes nothing. An application that names its driver with VK_DRIVER_FILES sees
// only that driver and is left alone too.
//
// Implicit, installed by guest/gpu-drivers/install.sh. LIMINA_VK_ALL_DEVICES=1 disables it.

#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>

#include <vulkan/vulkan.h>
#include <vulkan/vk_layer.h>

#define EXPORT __attribute__((visibility("default")))

/* The loader's dispatch table pointer, the first word of every dispatchable handle. */
static inline void *
dispatch_key(const void *handle)
{
   return *(void *const *)handle;
}

struct instance {
   void *key;
   VkInstance handle;
   PFN_vkGetInstanceProcAddr gipa;
   PFN_vkDestroyInstance destroy;
   PFN_vkEnumeratePhysicalDevices enumerate;
   PFN_vkEnumeratePhysicalDeviceGroups enumerate_groups;
   PFN_vkGetPhysicalDeviceProperties properties;
   PFN_vkGetPhysicalDeviceProperties2 properties2;
   struct instance *next;
};

struct device {
   void *key;
   PFN_vkGetDeviceProcAddr gdpa;
   PFN_vkDestroyDevice destroy;
   struct device *next;
};

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct instance *instances;
static struct device *devices;

static struct instance *
find_instance(const void *handle)
{
   void *key = dispatch_key(handle);
   pthread_mutex_lock(&lock);
   struct instance *i = instances;
   while (i && i->key != key)
      i = i->next;
   pthread_mutex_unlock(&lock);
   return i;
}

static struct device *
find_device(const void *handle)
{
   void *key = dispatch_key(handle);
   pthread_mutex_lock(&lock);
   struct device *d = devices;
   while (d && d->key != key)
      d = d->next;
   pthread_mutex_unlock(&lock);
   return d;
}

enum kind { OTHER, VENUS, LIMINA_VENUS, KOSMICKRISP };

static enum kind
kind_of(struct instance *inst, VkPhysicalDevice pd)
{
   if (inst->properties2) {
      VkPhysicalDeviceDriverProperties driver = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
      };
      VkPhysicalDeviceProperties2 props = {
         .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
         .pNext = &driver,
      };
      inst->properties2(pd, &props);
      if (driver.driverID == VK_DRIVER_ID_MESA_VENUS)
         return strstr(driver.driverInfo, "(limina)") ? LIMINA_VENUS : VENUS;
      if (driver.driverID == VK_DRIVER_ID_MESA_KOSMICKRISP)
         return KOSMICKRISP;
      if (driver.driverID != 0)
         return OTHER;
   }
   /* A 1.0 instance without the properties2 path: venus says who it is in the device name, but
    * not whose it is, so nothing is hidden. */
   VkPhysicalDeviceProperties props;
   inst->properties(pd, &props);
   return strncmp(props.deviceName, "Virtio-GPU Venus", 16) == 0 ? VENUS : OTHER;
}

/* Which devices this layer hides: every venus behind the transport, the system's behind ours. */
struct policy {
   bool transport;
   bool limina_venus;
};

static bool
hidden(const struct policy *p, enum kind k)
{
   if (p->transport)
      return k == VENUS || k == LIMINA_VENUS;
   return p->limina_venus && k == VENUS;
}

/* Every physical device the drivers below enumerate, and what this layer hides of them. */
static VkResult
all_devices(struct instance *inst, VkPhysicalDevice **out, uint32_t *count, struct policy *policy)
{
   uint32_t n = 0;
   VkResult r = inst->enumerate(inst->handle, &n, NULL);
   if (r != VK_SUCCESS)
      return r;
   VkPhysicalDevice *all = calloc(n ? n : 1, sizeof(*all));
   if (!all)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   r = inst->enumerate(inst->handle, &n, all);
   if (r != VK_SUCCESS && r != VK_INCOMPLETE) {
      free(all);
      return r;
   }
   *policy = (struct policy){0};
   for (uint32_t i = 0; i < n; i++) {
      enum kind k = kind_of(inst, all[i]);
      policy->transport |= k == KOSMICKRISP;
      policy->limina_venus |= k == LIMINA_VENUS;
   }
   *out = all;
   *count = n;
   return VK_SUCCESS;
}

static VKAPI_ATTR VkResult VKAPI_CALL
EnumeratePhysicalDevices(VkInstance instance, uint32_t *count, VkPhysicalDevice *devs)
{
   struct instance *inst = find_instance(instance);
   VkPhysicalDevice *all;
   uint32_t n;
   struct policy policy;
   VkResult r = all_devices(inst, &all, &n, &policy);
   if (r != VK_SUCCESS)
      return r;
   uint32_t kept = 0;
   for (uint32_t i = 0; i < n; i++) {
      if (hidden(&policy, kind_of(inst, all[i])))
         continue;
      all[kept++] = all[i];
   }
   if (!devs) {
      *count = kept;
      free(all);
      return VK_SUCCESS;
   }
   uint32_t copied = kept < *count ? kept : *count;
   memcpy(devs, all, copied * sizeof(*devs));
   *count = copied;
   free(all);
   return copied < kept ? VK_INCOMPLETE : VK_SUCCESS;
}

static VKAPI_ATTR VkResult VKAPI_CALL
EnumeratePhysicalDeviceGroups(VkInstance instance, uint32_t *count,
                              VkPhysicalDeviceGroupProperties *groups)
{
   struct instance *inst = find_instance(instance);
   VkPhysicalDevice *all;
   uint32_t n;
   struct policy policy;
   VkResult r = all_devices(inst, &all, &n, &policy);
   free(all);
   if (r != VK_SUCCESS)
      return r;
   if (!policy.transport && !policy.limina_venus)
      return inst->enumerate_groups(instance, count, groups);

   uint32_t ng = 0;
   r = inst->enumerate_groups(instance, &ng, NULL);
   if (r != VK_SUCCESS)
      return r;
   VkPhysicalDeviceGroupProperties *g = calloc(ng ? ng : 1, sizeof(*g));
   if (!g)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   for (uint32_t i = 0; i < ng; i++)
      g[i].sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_GROUP_PROPERTIES;
   r = inst->enumerate_groups(instance, &ng, g);
   if (r != VK_SUCCESS && r != VK_INCOMPLETE) {
      free(g);
      return r;
   }
   /* A group is a driver's when its devices are; groups never mix drivers. */
   uint32_t kept = 0;
   for (uint32_t i = 0; i < ng; i++) {
      if (g[i].physicalDeviceCount && hidden(&policy, kind_of(inst, g[i].physicalDevices[0])))
         continue;
      g[kept++] = g[i];
   }
   if (!groups) {
      *count = kept;
      free(g);
      return VK_SUCCESS;
   }
   uint32_t copied = kept < *count ? kept : *count;
   for (uint32_t i = 0; i < copied; i++) {
      void *pnext = groups[i].pNext;
      groups[i] = g[i];
      groups[i].pNext = pnext;
   }
   *count = copied;
   free(g);
   return copied < kept ? VK_INCOMPLETE : VK_SUCCESS;
}

static VKAPI_ATTR VkResult VKAPI_CALL
CreateInstance(const VkInstanceCreateInfo *info, const VkAllocationCallbacks *alloc,
               VkInstance *out)
{
   VkLayerInstanceCreateInfo *chain = (VkLayerInstanceCreateInfo *)info->pNext;
   while (chain && !(chain->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO &&
                     chain->function == VK_LAYER_LINK_INFO))
      chain = (VkLayerInstanceCreateInfo *)chain->pNext;
   if (!chain)
      return VK_ERROR_INITIALIZATION_FAILED;
   PFN_vkGetInstanceProcAddr gipa = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
   chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;

   /* Telling the drivers apart needs their driver IDs, which a 1.0 instance only reaches through
    * VK_KHR_get_physical_device_properties2: the loader hands out no 1.1 entry point to it. Every
    * Mesa driver has the extension, so enable it for such an instance. */
   uint32_t api = info->pApplicationInfo ? info->pApplicationInfo->apiVersion : 0;
   bool add_props2 = api < VK_API_VERSION_1_1;
   for (uint32_t i = 0; add_props2 && i < info->enabledExtensionCount; i++)
      if (!strcmp(info->ppEnabledExtensionNames[i],
                  VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME))
         add_props2 = false;
   VkInstanceCreateInfo ours = *info;
   const char **exts = NULL;
   if (add_props2) {
      exts = calloc(info->enabledExtensionCount + 1, sizeof(*exts));
      if (!exts)
         return VK_ERROR_OUT_OF_HOST_MEMORY;
      if (info->enabledExtensionCount)
         memcpy(exts, info->ppEnabledExtensionNames,
                info->enabledExtensionCount * sizeof(*exts));
      exts[info->enabledExtensionCount] = VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME;
      ours.enabledExtensionCount = info->enabledExtensionCount + 1;
      ours.ppEnabledExtensionNames = exts;
   }

   PFN_vkCreateInstance create = (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
   VkResult r = create(&ours, alloc, out);
   /* A driver list without the extension: create the instance as asked, and tell venus apart by
    * name alone, which leaves it unhidden. */
   if (r == VK_ERROR_EXTENSION_NOT_PRESENT && add_props2)
      r = create(info, alloc, out);
   free(exts);
   if (r != VK_SUCCESS)
      return r;

   struct instance *inst = calloc(1, sizeof(*inst));
   if (!inst)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   inst->key = dispatch_key(*out);
   inst->handle = *out;
   inst->gipa = gipa;
   inst->destroy = (PFN_vkDestroyInstance)gipa(*out, "vkDestroyInstance");
   inst->enumerate = (PFN_vkEnumeratePhysicalDevices)gipa(*out, "vkEnumeratePhysicalDevices");
   inst->enumerate_groups =
      (PFN_vkEnumeratePhysicalDeviceGroups)gipa(*out, "vkEnumeratePhysicalDeviceGroups");
   inst->properties = (PFN_vkGetPhysicalDeviceProperties)gipa(*out, "vkGetPhysicalDeviceProperties");
   inst->properties2 =
      (PFN_vkGetPhysicalDeviceProperties2)gipa(*out, "vkGetPhysicalDeviceProperties2");
   if (!inst->properties2)
      inst->properties2 =
         (PFN_vkGetPhysicalDeviceProperties2)gipa(*out, "vkGetPhysicalDeviceProperties2KHR");
   pthread_mutex_lock(&lock);
   inst->next = instances;
   instances = inst;
   pthread_mutex_unlock(&lock);
   return VK_SUCCESS;
}

static VKAPI_ATTR void VKAPI_CALL
DestroyInstance(VkInstance instance, const VkAllocationCallbacks *alloc)
{
   if (!instance)
      return;
   void *key = dispatch_key(instance);
   pthread_mutex_lock(&lock);
   struct instance **p = &instances, *inst = NULL;
   while (*p && (*p)->key != key)
      p = &(*p)->next;
   if (*p) {
      inst = *p;
      *p = inst->next;
   }
   pthread_mutex_unlock(&lock);
   if (inst) {
      inst->destroy(instance, alloc);
      free(inst);
   }
}

static VKAPI_ATTR VkResult VKAPI_CALL
CreateDevice(VkPhysicalDevice pd, const VkDeviceCreateInfo *info,
             const VkAllocationCallbacks *alloc, VkDevice *out)
{
   VkLayerDeviceCreateInfo *chain = (VkLayerDeviceCreateInfo *)info->pNext;
   while (chain && !(chain->sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO &&
                     chain->function == VK_LAYER_LINK_INFO))
      chain = (VkLayerDeviceCreateInfo *)chain->pNext;
   if (!chain)
      return VK_ERROR_INITIALIZATION_FAILED;
   PFN_vkGetInstanceProcAddr gipa = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
   PFN_vkGetDeviceProcAddr gdpa = chain->u.pLayerInfo->pfnNextGetDeviceProcAddr;
   chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;

   struct instance *inst = find_instance(pd);
   PFN_vkCreateDevice create =
      (PFN_vkCreateDevice)gipa(inst ? inst->handle : NULL, "vkCreateDevice");
   VkResult r = create(pd, info, alloc, out);
   if (r != VK_SUCCESS)
      return r;

   struct device *dev = calloc(1, sizeof(*dev));
   if (!dev)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   dev->key = dispatch_key(*out);
   dev->gdpa = gdpa;
   dev->destroy = (PFN_vkDestroyDevice)gdpa(*out, "vkDestroyDevice");
   pthread_mutex_lock(&lock);
   dev->next = devices;
   devices = dev;
   pthread_mutex_unlock(&lock);
   return VK_SUCCESS;
}

static VKAPI_ATTR void VKAPI_CALL
DestroyDevice(VkDevice device, const VkAllocationCallbacks *alloc)
{
   if (!device)
      return;
   void *key = dispatch_key(device);
   pthread_mutex_lock(&lock);
   struct device **p = &devices, *dev = NULL;
   while (*p && (*p)->key != key)
      p = &(*p)->next;
   if (*p) {
      dev = *p;
      *p = dev->next;
   }
   pthread_mutex_unlock(&lock);
   if (dev) {
      dev->destroy(device, alloc);
      free(dev);
   }
}

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
transport_select_GetDeviceProcAddr(VkDevice device, const char *name)
{
   if (!strcmp(name, "vkGetDeviceProcAddr"))
      return (PFN_vkVoidFunction)transport_select_GetDeviceProcAddr;
   if (!strcmp(name, "vkDestroyDevice"))
      return (PFN_vkVoidFunction)DestroyDevice;
   struct device *dev = find_device(device);
   return dev ? dev->gdpa(device, name) : NULL;
}

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
transport_select_GetInstanceProcAddr(VkInstance instance, const char *name)
{
   static const struct {
      const char *name;
      PFN_vkVoidFunction fn;
   } ours[] = {
      {"vkGetInstanceProcAddr", (PFN_vkVoidFunction)transport_select_GetInstanceProcAddr},
      {"vkGetDeviceProcAddr", (PFN_vkVoidFunction)transport_select_GetDeviceProcAddr},
      {"vkCreateInstance", (PFN_vkVoidFunction)CreateInstance},
      {"vkDestroyInstance", (PFN_vkVoidFunction)DestroyInstance},
      {"vkEnumeratePhysicalDevices", (PFN_vkVoidFunction)EnumeratePhysicalDevices},
      {"vkEnumeratePhysicalDeviceGroups", (PFN_vkVoidFunction)EnumeratePhysicalDeviceGroups},
      {"vkEnumeratePhysicalDeviceGroupsKHR", (PFN_vkVoidFunction)EnumeratePhysicalDeviceGroups},
      {"vkCreateDevice", (PFN_vkVoidFunction)CreateDevice},
      {"vkDestroyDevice", (PFN_vkVoidFunction)DestroyDevice},
   };
   for (size_t i = 0; i < sizeof(ours) / sizeof(ours[0]); i++)
      if (!strcmp(name, ours[i].name))
         return ours[i].fn;
   if (!instance)
      return NULL;
   struct instance *inst = find_instance(instance);
   return inst ? inst->gipa(instance, name) : NULL;
}

EXPORT VKAPI_ATTR VkResult VKAPI_CALL
vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *iface)
{
   if (iface->loaderLayerInterfaceVersion < 2)
      return VK_ERROR_INITIALIZATION_FAILED;
   iface->loaderLayerInterfaceVersion = 2;
   iface->pfnGetInstanceProcAddr = transport_select_GetInstanceProcAddr;
   iface->pfnGetDeviceProcAddr = transport_select_GetDeviceProcAddr;
   iface->pfnGetPhysicalDeviceProcAddr = NULL;
   return VK_SUCCESS;
}
