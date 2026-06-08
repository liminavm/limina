// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Spike: does MoltenVK HANG (or just error) on the exact vkGetPhysicalDeviceImageFormatProperties2
// query that makes GNOME-on-venus abort (#30 ring stall)?
//
// The venus host dispatch (vkr_physical_device.c:758) forwards IFP2 straight to MoltenVK. If
// MoltenVK blocks inside that call, the vkr ring thread never advances the ring head -> the guest
// vn_ring_wait_seqno spins -> vn_relax -> abort(). This probe reproduces the call HOST-SIDE with no
// VM, isolating hypothesis (a) "MoltenVK hangs" from (b) "errors but returns".
//
// Captured guest params (memory limina-tier2-venus PHASE 3): fmt=130 (D32_SFLOAT_S8_UINT),
// usage=0x400027 (TRANSFER_SRC|TRANSFER_DST|SAMPLED|DEPTH_STENCIL_ATTACHMENT|HOST_TRANSFER_EXT),
// flags=0x1000 (SAMPLE_LOCATIONS_COMPATIBLE_DEPTH_EXT), tiling=OPTIMAL, type=2D, pNext=[].
//
// A SIGALRM watchdog fires if any single IFP2 call takes too long -> proves the hang.

#define VK_USE_PLATFORM_METAL_EXT
#include <vulkan/vulkan.h>
#import <Foundation/Foundation.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <setjmp.h>

#define CHECK(x) do { VkResult _r = (x); if (_r != VK_SUCCESS) { \
  fprintf(stderr, "FAIL %s -> %d (line %d)\n", #x, _r, __LINE__); return 2; } } while (0)

static const char *g_label = "";
static void on_alarm(int sig) {
  (void)sig;
  fprintf(stderr, "\n*** WATCHDOG: IFP2 call [%s] DID NOT RETURN within timeout -> MoltenVK HANG (hypothesis a) ***\n", g_label);
  _exit(42);
}

static const char *res_str(VkResult r) {
  switch (r) {
    case VK_SUCCESS: return "VK_SUCCESS";
    case VK_ERROR_FORMAT_NOT_SUPPORTED: return "VK_ERROR_FORMAT_NOT_SUPPORTED";
    case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
    case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
    case VK_ERROR_FEATURE_NOT_PRESENT: return "VK_ERROR_FEATURE_NOT_PRESENT";
    default: return "VK_ERROR_<other>";
  }
}

// Run one IFP2 call, with a watchdog. Returns the VkResult (or _exit(42) on hang).
static VkResult try_ifp2(VkPhysicalDevice gpu, const char *label,
                         VkFormat fmt, VkImageType type, VkImageTiling tiling,
                         VkImageUsageFlags usage, VkImageCreateFlags flags,
                         const void *pNext) {
  g_label = label;
  VkPhysicalDeviceImageFormatInfo2 info = {
    .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
    .pNext = pNext, .format = fmt, .type = type, .tiling = tiling,
    .usage = usage, .flags = flags,
  };
  VkImageFormatProperties2 props = { .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2 };
  alarm(6);
  VkResult r = vkGetPhysicalDeviceImageFormatProperties2(gpu, &info, &props);
  alarm(0);
  printf("  IFP2 [%-28s] fmt=%d usage=0x%x flags=0x%x -> %s (%d)",
         label, fmt, usage, flags, res_str(r), r);
  if (r == VK_SUCCESS)
    printf("  maxExtent=%ux%u maxMip=%u maxArray=%u",
           props.imageFormatProperties.maxExtent.width,
           props.imageFormatProperties.maxExtent.height,
           props.imageFormatProperties.maxMipLevels,
           props.imageFormatProperties.maxArrayLayers);
  printf("\n");
  return r;
}

int main(void) {
@autoreleasepool {
  signal(SIGALRM, on_alarm);

  const char *instExts[] = { "VK_KHR_portability_enumeration" };
  VkApplicationInfo app = { .sType=VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion=VK_API_VERSION_1_2 };
  VkInstanceCreateInfo ici = { .sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                               .flags=VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
                               .pApplicationInfo=&app, .enabledExtensionCount=1,
                               .ppEnabledExtensionNames=instExts };
  VkInstance inst; CHECK(vkCreateInstance(&ici, NULL, &inst));
  uint32_t ndev=0; CHECK(vkEnumeratePhysicalDevices(inst,&ndev,NULL));
  if(!ndev){fprintf(stderr,"no devices\n");return 2;}
  VkPhysicalDevice phys[8]; if(ndev>8)ndev=8; CHECK(vkEnumeratePhysicalDevices(inst,&ndev,phys));
  VkPhysicalDevice gpu=phys[0];
  VkPhysicalDeviceProperties pp; vkGetPhysicalDeviceProperties(gpu,&pp);
  printf("== device: %s (Vulkan %u.%u.%u) ==\n", pp.deviceName,
         VK_VERSION_MAJOR(pp.apiVersion),VK_VERSION_MINOR(pp.apiVersion),VK_VERSION_PATCH(pp.apiVersion));

  // Survey the relevant extensions so we know whether the usage/flags are even valid on the host.
  uint32_t next=0; vkEnumerateDeviceExtensionProperties(gpu,NULL,&next,NULL);
  VkExtensionProperties *ep=calloc(next,sizeof(*ep)); vkEnumerateDeviceExtensionProperties(gpu,NULL,&next,ep);
  const char *survey[]={"VK_EXT_sample_locations","VK_EXT_host_image_copy"};
  printf("== relevant host (MoltenVK) extensions ==\n");
  for(size_t i=0;i<sizeof(survey)/sizeof(*survey);i++){
    int found=0; for(uint32_t j=0;j<next;j++) if(!strcmp(ep[j].extensionName,survey[i])){found=1;break;}
    printf("  [%s] %s\n", found?"YES":" no", survey[i]);
  }

  printf("\n== IFP2 probes ==\n");
  const VkFormat D32S8 = VK_FORMAT_D32_SFLOAT_S8_UINT; // 130
  // 1. The EXACT captured failing query.
  try_ifp2(gpu, "EXACT (D32S8 0x400027 0x1000)", D32S8, VK_IMAGE_TYPE_2D,
           VK_IMAGE_TILING_OPTIMAL, 0x400027, 0x1000, NULL);
  // 2. Same but drop HOST_TRANSFER usage bit (0x400000) -> isolates that bit.
  try_ifp2(gpu, "no HOST_TRANSFER (0x27)", D32S8, VK_IMAGE_TYPE_2D,
           VK_IMAGE_TILING_OPTIMAL, 0x27, 0x1000, NULL);
  // 3. Same but drop SAMPLE_LOCATIONS flag -> isolates that flag.
  try_ifp2(gpu, "no SAMPLE_LOCATIONS flag", D32S8, VK_IMAGE_TYPE_2D,
           VK_IMAGE_TILING_OPTIMAL, 0x400027, 0x0, NULL);
  // 4. Plainest depth query (sanity: should always work).
  try_ifp2(gpu, "plain depth (0x20 attach)", D32S8, VK_IMAGE_TYPE_2D,
           VK_IMAGE_TILING_OPTIMAL, VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT, 0x0, NULL);
  // 5. Plain color (full sanity baseline).
  try_ifp2(gpu, "plain color BGRA", VK_FORMAT_B8G8R8A8_UNORM, VK_IMAGE_TYPE_2D,
           VK_IMAGE_TILING_OPTIMAL, VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, 0x0, NULL);

  // ----- EXTERNAL-memory input chains: what kopper/zink check_ici uses for a swapchain image.
  // venus advertises these handle types (for guest capability detection) but MoltenVK lacks the
  // backing exts (we strip them at device-create). IFP2 is physical-device level -> NOT filtered.
  printf("\n== IFP2 with VkPhysicalDeviceExternalImageFormatInfo (swapchain/scanout path) ==\n");
  const VkExternalMemoryHandleTypeFlagBits htypes[] = {
    VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,        // 0x1
    VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,      // 0x200
  };
  const char *hnames[] = { "OPAQUE_FD(0x1)", "DMA_BUF(0x200)" };
  for (size_t i = 0; i < sizeof(htypes)/sizeof(*htypes); i++) {
    VkPhysicalDeviceExternalImageFormatInfo ext = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
      .handleType = htypes[i],
    };
    char lbl[64]; snprintf(lbl, sizeof(lbl), "color ext=%s", hnames[i]);
    try_ifp2(gpu, lbl, VK_FORMAT_B8G8R8A8_UNORM, VK_IMAGE_TYPE_2D,
             VK_IMAGE_TILING_OPTIMAL, VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, 0x0, &ext);
    char lbl2[64]; snprintf(lbl2, sizeof(lbl2), "depth ext=%s", hnames[i]);
    try_ifp2(gpu, lbl2, D32S8, VK_IMAGE_TYPE_2D, VK_IMAGE_TILING_OPTIMAL,
             0x400027, 0x1000, &ext);
  }

  printf("\n==== VERDICT ====\n");
  printf("  If the EXACT probe printed a result line, MoltenVK does NOT hang on it ->\n");
  printf("  the venus ring stall is NOT a MoltenVK IFP2 hang (rule out hypothesis a);\n");
  printf("  look host-side at vkr ring-head advance / guest-side coherency instead.\n");
  printf("  If the watchdog fired (exit 42), MoltenVK HANGS -> hypothesis (a) confirmed.\n");
  return 0;
}
}
