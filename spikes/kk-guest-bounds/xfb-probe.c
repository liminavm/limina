// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Transform-feedback buffer bindings a venus guest can set on KosmicKrisp (companion to probe.c).
// vkCmdBindTransformFeedbackBuffersEXT takes an offset and size per buffer straight from the
// guest. KK fed them to vk_buffer_range, which only asserts they fit the buffer -- compiled out
// in a release build -- and for VK_WHOLE_SIZE it computed the size as buffer_size - offset, which
// underflows to a huge value for an offset past the buffer. The out-of-range base/size is then
// what the transform-feedback capture shader writes through, a guest-driven out-of-bounds GPU
// write. KK must refuse an out-of-range binding and clamp an oversized one.
//
// Each case runs in its own process; PASS = KK stored an in-range binding (or refused it) and
// survived, CRASH = it did not. No draw is issued: the binding alone is where the offset/size
// are validated, so the cases stay off the GPU.
//
// Build:
//   cc -Wall -I/opt/homebrew/include xfb-probe.c -L/opt/homebrew/lib -lvulkan -o xp
//   VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./xp [case]
// Each case re-executes the binary: a bare fork() cannot reach Metal's XPC services.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/wait.h>
#include <unistd.h>

enum { BUFSIZE = 256 };

static VkInstance inst; static VkPhysicalDevice pd; static VkDevice dev; static VkQueue q;
static VkCommandPool cpool;
static VkBuffer xfb_buf;
static PFN_vkCmdBindTransformFeedbackBuffersEXT bind_xfb;

static uint32_t mem_type(uint32_t bits, VkMemoryPropertyFlags want) {
  VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) return i;
  fprintf(stderr, "no memory type\n"); exit(2);
}

static void setup(void) {
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_3};
  const char *ie[] = {"VK_KHR_portability_enumeration"};
  VkInstanceCreateInfo ic = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
                             .flags = VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
                             .enabledExtensionCount = 1, .ppEnabledExtensionNames = ie};
  if (vkCreateInstance(&ic, NULL, &inst)) { fprintf(stderr, "instance\n"); exit(2); }
  uint32_t n = 1; vkEnumeratePhysicalDevices(inst, &n, &pd);
  if (!n) { fprintf(stderr, "no device\n"); exit(2); }
  float pr = 1;
  VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueCount = 1, .pQueuePriorities = &pr};
  VkPhysicalDeviceTransformFeedbackFeaturesEXT tf = {
    .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TRANSFORM_FEEDBACK_FEATURES_EXT, .transformFeedback = VK_TRUE};
  const char *de[] = {"VK_EXT_transform_feedback"};
  VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &tf, .queueCreateInfoCount = 1,
                           .pQueueCreateInfos = &qci, .enabledExtensionCount = 1, .ppEnabledExtensionNames = de};
  if (vkCreateDevice(pd, &dc, NULL, &dev)) { fprintf(stderr, "device\n"); exit(2); }
  vkGetDeviceQueue(dev, 0, 0, &q);
  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  vkCreateCommandPool(dev, &cpi, NULL, &cpool);
  bind_xfb = (PFN_vkCmdBindTransformFeedbackBuffersEXT)vkGetDeviceProcAddr(dev, "vkCmdBindTransformFeedbackBuffersEXT");
  if (!bind_xfb) { fprintf(stderr, "no bind_xfb\n"); exit(2); }

  VkBufferCreateInfo bi = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = BUFSIZE,
    .usage = VK_BUFFER_USAGE_TRANSFORM_FEEDBACK_BUFFER_BIT_EXT};
  vkCreateBuffer(dev, &bi, NULL, &xfb_buf);
  VkMemoryRequirements r; vkGetBufferMemoryRequirements(dev, xfb_buf, &r);
  VkMemoryAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = r.size,
    .memoryTypeIndex = mem_type(r.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)};
  VkDeviceMemory m; vkAllocateMemory(dev, &ai, NULL, &m);
  vkBindBufferMemory(dev, xfb_buf, m, 0);
}

static VkCommandBuffer begin_cmd(void) {
  VkCommandBufferAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool,
                                    .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
  VkCommandBuffer cb; vkAllocateCommandBuffers(dev, &ai, &cb);
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  vkBeginCommandBuffer(cb, &bi);
  return cb;
}

// Bind the xfb buffer with the given offset and size, then close the command buffer. The binding
// is where the offset/size reach KK; survive = PASS.
static int bind(VkDeviceSize offset, VkDeviceSize size) {
  VkCommandBuffer cb = begin_cmd();
  VkDeviceSize off = offset, sz = size;
  bind_xfb(cb, 0, 1, &xfb_buf, &off, &sz);
  vkEndCommandBuffer(cb);
  vkFreeCommandBuffers(dev, cpool, 1, &cb);
  return 1;
}

static int bind_ok(void) { return bind(0, BUFSIZE); }                 // in range
static int bind_whole(void) { return bind(64, VK_WHOLE_SIZE); }       // in range, WHOLE_SIZE
static int bind_offset_over(void) { return bind(BUFSIZE + 64, VK_WHOLE_SIZE); }  // offset past end
static int bind_size_over(void) { return bind(0, BUFSIZE * 4); }      // size past end

static const struct { const char *name; int (*fn)(void); } tests[] = {
  {"bind-ok", bind_ok},
  {"bind-whole", bind_whole},
  {"bind-offset-over", bind_offset_over},
  {"bind-size-over", bind_size_over},
};

int main(int argc, char **argv) {
  if (argc > 2 && !strcmp(argv[2], "--child")) {
    for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++)
      if (!strcmp(argv[1], tests[i].name)) { setup(); return tests[i].fn() == 1 ? 0 : 1; }
    return 2;
  }
  int fails = 0;
  for (size_t i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
    if (argc > 1 && strcmp(argv[1], tests[i].name)) continue;
    fflush(stdout);
    // A bare fork cannot reach Metal's XPC services, so each case re-executes this binary.
    pid_t p = fork();
    if (p == 0) { execl(argv[0], argv[0], tests[i].name, "--child", (char *)NULL); _exit(3); }
    int st;
    waitpid(p, &st, 0);
    const char *v = WIFSIGNALED(st) ? "CRASH" : WEXITSTATUS(st) == 0 ? "PASS" : "FAIL";
    if (WIFSIGNALED(st)) printf("  (signal %d)\n", WTERMSIG(st));
    printf("%-22s %s\n", tests[i].name, v);
    fails += strcmp(v, "PASS") != 0;
  }
  return fails != 0;
}
