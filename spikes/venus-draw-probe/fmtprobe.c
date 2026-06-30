// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// fmtprobe.c — ground-truth probe for the format features wgpu's TEXTURE_FORMAT_16BIT_NORM gates on.
//
// vulkaninfo on venus does NOT emit a per-format "Format Properties" table, so it can't answer
// "does venus/KK advertise STORAGE_IMAGE for the six 16-bit-norm formats?". This calls the exact
// API wgpu-hal uses — vkGetPhysicalDeviceFormatProperties (core) — for those six formats (plus a
// couple of controls) and prints optimalTilingFeatures + a PASS/FAIL against wgpu's rule:
//   adapter advertises Features::TEXTURE_FORMAT_16BIT_NORM IFF all six of
//   R16/RG16/RGBA16 x {UNORM,SNORM} have optimalTilingFeatures containing
//   SAMPLED_IMAGE | STORAGE_IMAGE | TRANSFER_SRC | TRANSFER_DST   (wgpu-hal 29 adapter.rs:3063).
//
// Build (in guest): gcc fmtprobe.c -lvulkan -o fmtprobe
// Run   (in guest): VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json ./fmtprobe
#include <vulkan/vulkan.h>
#include <stdio.h>
#include <stddef.h>

static const struct { VkFormat f; const char *n; int is16; } FMTS[] = {
  { VK_FORMAT_R16_UNORM,          "R16_UNORM",          1 },
  { VK_FORMAT_R16_SNORM,          "R16_SNORM",          1 },
  { VK_FORMAT_R16G16_UNORM,       "R16G16_UNORM",       1 },
  { VK_FORMAT_R16G16_SNORM,       "R16G16_SNORM",       1 },
  { VK_FORMAT_R16G16B16A16_UNORM, "R16G16B16A16_UNORM", 1 },
  { VK_FORMAT_R16G16B16A16_SNORM, "R16G16B16A16_SNORM", 1 },
  { VK_FORMAT_R8G8B8A8_UNORM,     "R8G8B8A8_UNORM[ctl]",   0 },
  { VK_FORMAT_R16G16B16A16_SFLOAT,"R16G16B16A16_SFLOAT[ctl]",0 },
};

static const struct { VkFormatFeatureFlags b; const char *n; } BITS[] = {
  { VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT,               "SAMPLED" },
  { VK_FORMAT_FEATURE_STORAGE_IMAGE_BIT,               "STORAGE" },
  { VK_FORMAT_FEATURE_STORAGE_IMAGE_ATOMIC_BIT,        "STORAGE_ATOMIC" },
  { VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT,            "COLOR_ATT" },
  { VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BLEND_BIT,      "COLOR_BLEND" },
  { VK_FORMAT_FEATURE_SAMPLED_IMAGE_FILTER_LINEAR_BIT, "FILTER_LINEAR" },
  { VK_FORMAT_FEATURE_TRANSFER_SRC_BIT,                "TRANSFER_SRC" },
  { VK_FORMAT_FEATURE_TRANSFER_DST_BIT,                "TRANSFER_DST" },
  { VK_FORMAT_FEATURE_BLIT_SRC_BIT,                    "BLIT_SRC" },
  { VK_FORMAT_FEATURE_BLIT_DST_BIT,                    "BLIT_DST" },
};

#define WGPU_REQ (VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT | VK_FORMAT_FEATURE_STORAGE_IMAGE_BIT | \
                  VK_FORMAT_FEATURE_TRANSFER_SRC_BIT  | VK_FORMAT_FEATURE_TRANSFER_DST_BIT)

int main(void) {
  VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_1 };
  VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
  VkInstance inst;
  if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) { printf("vkCreateInstance failed\n"); return 1; }
  uint32_t n = 0; vkEnumeratePhysicalDevices(inst, &n, NULL);
  if (!n) { printf("no physical devices\n"); return 1; }
  VkPhysicalDevice pds[8]; if (n > 8) n = 8; vkEnumeratePhysicalDevices(inst, &n, pds);
  VkPhysicalDevice pd = pds[0];
  VkPhysicalDeviceProperties pp; vkGetPhysicalDeviceProperties(pd, &pp);
  printf("device: %s (api %u.%u.%u)\n\n", pp.deviceName,
         VK_VERSION_MAJOR(pp.apiVersion), VK_VERSION_MINOR(pp.apiVersion), VK_VERSION_PATCH(pp.apiVersion));

  int all16 = 1;
  for (size_t i = 0; i < sizeof(FMTS)/sizeof(FMTS[0]); i++) {
    VkFormatProperties fp; vkGetPhysicalDeviceFormatProperties(pd, FMTS[i].f, &fp);
    VkFormatFeatureFlags o = fp.optimalTilingFeatures;
    int pass = (o & WGPU_REQ) == WGPU_REQ;
    if (FMTS[i].is16 && !pass) all16 = 0;
    printf("%-26s optimal=0x%08x  %s\n    ", FMTS[i].n, o,
           FMTS[i].is16 ? (pass ? "[16bit-norm req: PASS]" : "[16bit-norm req: FAIL]") : "");
    for (size_t b = 0; b < sizeof(BITS)/sizeof(BITS[0]); b++) if (o & BITS[b].b) printf("%s ", BITS[b].n);
    if ((o & WGPU_REQ) != WGPU_REQ) {
      printf(" <<MISSING:");
      if (!(o & VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT)) printf(" SAMPLED");
      if (!(o & VK_FORMAT_FEATURE_STORAGE_IMAGE_BIT)) printf(" STORAGE");
      if (!(o & VK_FORMAT_FEATURE_TRANSFER_SRC_BIT))  printf(" TRANSFER_SRC");
      if (!(o & VK_FORMAT_FEATURE_TRANSFER_DST_BIT))  printf(" TRANSFER_DST");
      printf(">>");
    }
    printf("\n");
  }
  printf("\n==> wgpu TEXTURE_FORMAT_16BIT_NORM on this adapter: %s\n",
         all16 ? "ENABLED (all six pass)" : "DISABLED (>=1 of the six is missing a required flag)");
  return 0;
}
