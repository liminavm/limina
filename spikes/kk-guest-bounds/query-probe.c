// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Query pools a venus guest can create on KosmicKrisp (companion to probe.c). KK advertises
// VK_EXT_primitives_generated_query and VK_EXT_transform_feedback's queries, so a guest that
// enables them makes PRIMITIVES_GENERATED and TRANSFORM_FEEDBACK_STREAM pools, and every pool
// entry point sizes the pool's reports from kk_reports_per_query. Each case runs in its own
// process; PASS = KK handles (or refuses) the input cleanly, CRASH/FAIL = it does not.
//
// No draws, so every count is 0: the cases check that each entry point survives and that the
// result layout is right (a TRANSFORM_FEEDBACK_STREAM query is two values, written and needed,
// so its availability word comes third).
//
// Build:
//   cc -Wall -I/opt/homebrew/include query-probe.c -L/opt/homebrew/lib -lvulkan -o qp
//   VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./qp [case]
// Each case re-executes the binary: a bare fork() cannot reach Metal's XPC services.
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/wait.h>
#include <unistd.h>

static VkInstance inst; static VkPhysicalDevice pd; static VkDevice dev; static VkQueue q;
static VkCommandPool cpool;
static PFN_vkCmdBeginQueryIndexedEXT begin_indexed;
static PFN_vkCmdEndQueryIndexedEXT end_indexed;

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
  VkPhysicalDevicePrimitivesGeneratedQueryFeaturesEXT pg = {
    .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PRIMITIVES_GENERATED_QUERY_FEATURES_EXT, .primitivesGeneratedQuery = VK_TRUE};
  VkPhysicalDeviceTransformFeedbackFeaturesEXT tf = {
    .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TRANSFORM_FEEDBACK_FEATURES_EXT, .pNext = &pg, .transformFeedback = VK_TRUE};
  VkPhysicalDeviceVulkan12Features f12 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .pNext = &tf, .hostQueryReset = VK_TRUE};
  const char *de[] = {"VK_EXT_transform_feedback", "VK_EXT_primitives_generated_query"};
  VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f12, .queueCreateInfoCount = 1,
                           .pQueueCreateInfos = &qci, .enabledExtensionCount = 2, .ppEnabledExtensionNames = de};
  if (vkCreateDevice(pd, &dc, NULL, &dev)) { fprintf(stderr, "device\n"); exit(2); }
  vkGetDeviceQueue(dev, 0, 0, &q);
  VkCommandPoolCreateInfo cpi = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
  vkCreateCommandPool(dev, &cpi, NULL, &cpool);
  begin_indexed = (PFN_vkCmdBeginQueryIndexedEXT)vkGetDeviceProcAddr(dev, "vkCmdBeginQueryIndexedEXT");
  end_indexed = (PFN_vkCmdEndQueryIndexedEXT)vkGetDeviceProcAddr(dev, "vkCmdEndQueryIndexedEXT");
}

static VkQueryPool make_pool(VkQueryType type, uint32_t count, VkResult *r) {
  VkQueryPoolCreateInfo ci = {.sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO, .queryType = type, .queryCount = count};
  if (type == VK_QUERY_TYPE_PIPELINE_STATISTICS)
    ci.pipelineStatistics = VK_QUERY_PIPELINE_STATISTIC_INPUT_ASSEMBLY_VERTICES_BIT;
  VkQueryPool p = VK_NULL_HANDLE;
  *r = vkCreateQueryPool(dev, &ci, NULL, &p);
  return p;
}

static VkCommandBuffer begin_cmd(void) {
  VkCommandBufferAllocateInfo ai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool,
                                    .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
  VkCommandBuffer cb; vkAllocateCommandBuffers(dev, &ai, &cb);
  VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  vkBeginCommandBuffer(cb, &bi);
  return cb;
}

static int submit(VkCommandBuffer cb) {
  if (vkEndCommandBuffer(cb)) return 0;
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
  if (vkQueueSubmit(q, 1, &si, VK_NULL_HANDLE)) return 0;
  return vkQueueWaitIdle(q) == VK_SUCCESS;
}

// Begin/End queries 0..count-1 (stream 0), then read every query back with availability.
// `values` is the number of result values one query of this type has.
static int run_queries(VkQueryType type, uint32_t values) {
  enum { COUNT = 4 };
  VkResult r; VkQueryPool p = make_pool(type, COUNT, &r);
  if (r) { printf("  create: %d\n", r); return 0; }
  vkResetQueryPool(dev, p, 0, COUNT);
  uint64_t res[COUNT][4];
  memset(res, 0xab, sizeof(res));
  VkResult g = vkGetQueryPoolResults(dev, p, 0, COUNT, sizeof(res), res, sizeof(res[0]),
                                     VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WITH_AVAILABILITY_BIT);
  if (g != VK_NOT_READY) { printf("  unreset pool read %d, want VK_NOT_READY\n", g); return 0; }
  VkCommandBuffer cb = begin_cmd();
  vkCmdResetQueryPool(cb, p, 0, COUNT);
  for (uint32_t i = 0; i < COUNT; i++) { vkCmdBeginQuery(cb, p, i, 0); vkCmdEndQuery(cb, p, i); }
  if (!submit(cb)) { printf("  submit failed\n"); return 0; }
  memset(res, 0xab, sizeof(res));
  g = vkGetQueryPoolResults(dev, p, 0, COUNT, sizeof(res), res, sizeof(res[0]),
                            VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WITH_AVAILABILITY_BIT | VK_QUERY_RESULT_WAIT_BIT);
  if (g) { printf("  get results: %d\n", g); return 0; }
  for (uint32_t i = 0; i < COUNT; i++) {
    for (uint32_t v = 0; v < values; v++)
      if (res[i][v] != 0) { printf("  query %u value %u = %#llx, want 0\n", i, v, (unsigned long long)res[i][v]); return 0; }
    if (res[i][values] != 1) { printf("  query %u availability (word %u) = %#llx, want 1\n", i, values, (unsigned long long)res[i][values]); return 0; }
    if (res[i][values + 1] != 0xabababababababab) { printf("  query %u wrote past its %u values + availability\n", i, values); return 0; }
  }
  vkDestroyQueryPool(dev, p, NULL);
  return 1;
}

static int pg_queries(void) { return run_queries(VK_QUERY_TYPE_PRIMITIVES_GENERATED_EXT, 1); }
static int xfb_queries(void) { return run_queries(VK_QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM_EXT, 2); }
static int oq_queries(void) { return run_queries(VK_QUERY_TYPE_OCCLUSION, 1); }  // control

// Control: timestamp pools share the report indexing the fix touches. Write one timestamp per
// query and read them back: each nonzero and available, nothing written past the availability.
static int ts_queries(void) {
  enum { COUNT = 4 };
  VkResult r; VkQueryPool p = make_pool(VK_QUERY_TYPE_TIMESTAMP, COUNT, &r);
  if (r) { printf("  create: %d\n", r); return 0; }
  VkCommandBuffer cb = begin_cmd();
  vkCmdResetQueryPool(cb, p, 0, COUNT);
  for (uint32_t i = 0; i < COUNT; i++) vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, p, i);
  if (!submit(cb)) { printf("  submit failed\n"); return 0; }
  uint64_t res[COUNT][3];
  memset(res, 0xab, sizeof(res));
  VkResult g = vkGetQueryPoolResults(dev, p, 0, COUNT, sizeof(res), res, sizeof(res[0]),
                                     VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WITH_AVAILABILITY_BIT | VK_QUERY_RESULT_WAIT_BIT);
  if (g) { printf("  get results: %d\n", g); return 0; }
  for (uint32_t i = 0; i < COUNT; i++)
    if (res[i][0] == 0 || res[i][1] != 1 || res[i][2] != 0xabababababababab) {
      printf("  query %u: value %#llx avail %#llx next %#llx\n", i, (unsigned long long)res[i][0],
             (unsigned long long)res[i][1], (unsigned long long)res[i][2]);
      return 0;
    }
  vkDestroyQueryPool(dev, p, NULL);
  return 1;
}

// A stream past the one KK has (maxTransformFeedbackStreams = 1): invalid, so refusing it or
// ignoring it are both fine; aborting the host is not.
static int xfb_stream1(void) {
  VkResult r; VkQueryPool p = make_pool(VK_QUERY_TYPE_TRANSFORM_FEEDBACK_STREAM_EXT, 1, &r);
  if (r) { printf("  create: %d\n", r); return 0; }
  VkCommandBuffer cb = begin_cmd();
  vkCmdResetQueryPool(cb, p, 0, 1);
  begin_indexed(cb, p, 0, 0, 1);
  end_indexed(cb, p, 0, 1);
  submit(cb);
  return 1;
}

// A query type KK does not advertise. Refusing the pool is right; reaching UNREACHABLE is not.
static int pipeline_stats(void) {
  VkResult r; VkQueryPool p = make_pool(VK_QUERY_TYPE_PIPELINE_STATISTICS, 1, &r);
  if (r == VK_SUCCESS) {
    vkResetQueryPool(dev, p, 0, 1);
    vkDestroyQueryPool(dev, p, NULL);
  }
  return 1;
}

static const struct { const char *name; int (*fn)(void); } tests[] = {
  {"oq-queries", oq_queries},
  {"ts-queries", ts_queries},
  {"pg-queries", pg_queries},
  {"xfb-queries", xfb_queries},
  {"xfb-stream1", xfb_stream1},
  {"pipeline-stats", pipeline_stats},
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
