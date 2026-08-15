// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* rpcombo: sweep render-pass attachment combinations against the HOST KosmicKrisp driver,
 * hunting the combination that makes Apple's shader compiler abort.
 *
 * The bug (spikes/agx-compiler-abort/NOTES.md): MTLCompilerService asserts
 *   AGCLLVMObject::readBitcode(...) "bitcode_url is NULL ... extension 'ds'"
 * and AGXMetalG13X then os_crash()es the whole process. Apple ships background-object
 * (fast-clear) bitcode for exactly six sizes -- blit_fast_clear_gen2_{1,2,4,5,8,16} -- so the
 * driver asked for a size that does not exist. The hypothesis under test is that the size is the
 * per-pixel BYTE total of the render pass's tile layout, which a multi-attachment pass can easily
 * push outside that set (RGBA8 + D32S8 = 9).
 *
 * So: enumerate combinations, print each one BEFORE executing it and flush, and let the abort
 * name its own trigger -- the last line printed is the combination that killed us. Each combo
 * begins a rendering pass with the given load op and submits it, which is what makes the driver
 * build a background-object program.
 *
 * Runs host-side against the KK ICD (VK_ICD_FILENAMES, see run.sh) -- no VM involved, so
 * iteration is seconds rather than boots. A guest can only reach this through venus, but the
 * trigger is a Metal-level render pass configuration, which is reproducible here directly.
 *
 * RED:   aborts. The last "TRY" line names the combination; its `total` column tests the
 *        hypothesis (expect a value outside {1,2,4,5,8,16}).
 * GREEN: prints "swept N combinations, no abort" and exits 0 -- the hypothesis is wrong, or the
 *        trigger needs something this sweep does not vary (see the ideas at the bottom).
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <vulkan/vulkan.h>

#define CHECK(expr)                                                            \
   do {                                                                        \
      VkResult _r = (expr);                                                    \
      if (_r != VK_SUCCESS) {                                                  \
         fprintf(stderr, "%s:%d: %s -> %d\n", __FILE__, __LINE__, #expr, _r);  \
         exit(1);                                                              \
      }                                                                        \
   } while (0)

#define W 64
#define H 64
#define MAX_COLOR 4

static VkInstance inst;
static VkPhysicalDevice phys;
static VkDevice dev;
static VkQueue queue;
static uint32_t qfam;
static VkCommandPool pool;
static VkCommandBuffer cmd;

struct fmt {
   VkFormat vk;
   const char *name;
   uint32_t bytes; /* per-pixel bytes, the quantity the hypothesis is about */
};

/* Colour formats spanning the standard sizes, plus the odd ones a GL guest can ask for. */
static const struct fmt COLOR[] = {
   {VK_FORMAT_R8_UNORM, "R8", 1},
   {VK_FORMAT_R8G8_UNORM, "RG8", 2},
   {VK_FORMAT_R8G8B8A8_UNORM, "RGBA8", 4},
   {VK_FORMAT_B8G8R8A8_UNORM, "BGRA8", 4},
   {VK_FORMAT_R16G16_SFLOAT, "RG16F", 4},
   {VK_FORMAT_R16G16B16A16_SFLOAT, "RGBA16F", 8},
   {VK_FORMAT_R32G32B32A32_SFLOAT, "RGBA32F", 16},
   {VK_FORMAT_A2B10G10R10_UNORM_PACK32, "RGB10A2", 4},
   {VK_FORMAT_R5G6B5_UNORM_PACK16, "RGB565", 2},
};
#define NCOLOR (sizeof(COLOR) / sizeof(COLOR[0]))

/* Depth/stencil, including the 5-byte D32S8 that is almost certainly why "5" is in Apple's set. */
static const struct fmt DEPTH[] = {
   {VK_FORMAT_UNDEFINED, "none", 0},
   {VK_FORMAT_D16_UNORM, "D16", 2},
   {VK_FORMAT_D32_SFLOAT, "D32", 4},
   {VK_FORMAT_D32_SFLOAT_S8_UINT, "D32S8", 5},
   {VK_FORMAT_D24_UNORM_S8_UINT, "D24S8", 4},
   {VK_FORMAT_S8_UINT, "S8", 1},
};
#define NDEPTH (sizeof(DEPTH) / sizeof(DEPTH[0]))

struct image {
   VkImage img;
   VkDeviceMemory mem;
   VkImageView view;
};

static uint32_t find_mem(uint32_t bits, VkMemoryPropertyFlags want)
{
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(phys, &mp);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
         return i;
   fprintf(stderr, "no memory type for bits=%#x\n", bits);
   exit(1);
}

static bool make_image(VkFormat format, bool depth, uint32_t samples, struct image *out)
{
   VkImageCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = format,
      .extent = {W, H, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = samples,
      .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = depth ? VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT
                     : VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   /* An unsupported format is a skip, not a failure -- the sweep is deliberately broad. */
   VkImageFormatProperties ifp;
   if (vkGetPhysicalDeviceImageFormatProperties(phys, format, ici.imageType, ici.tiling, ici.usage,
                                                0, &ifp) != VK_SUCCESS)
      return false;
   if (vkCreateImage(dev, &ici, NULL, &out->img) != VK_SUCCESS)
      return false;

   VkMemoryRequirements mr;
   vkGetImageMemoryRequirements(dev, out->img, &mr);
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
   };
   CHECK(vkAllocateMemory(dev, &mai, NULL, &out->mem));
   CHECK(vkBindImageMemory(dev, out->img, out->mem, 0));

   VkImageAspectFlags aspect = VK_IMAGE_ASPECT_COLOR_BIT;
   if (depth) {
      aspect = 0;
      if (format != VK_FORMAT_S8_UINT)
         aspect |= VK_IMAGE_ASPECT_DEPTH_BIT;
      if (format == VK_FORMAT_D32_SFLOAT_S8_UINT || format == VK_FORMAT_D24_UNORM_S8_UINT ||
          format == VK_FORMAT_S8_UINT)
         aspect |= VK_IMAGE_ASPECT_STENCIL_BIT;
   }
   VkImageViewCreateInfo vci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = out->img,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = format,
      .subresourceRange = {aspect, 0, 1, 0, 1},
   };
   CHECK(vkCreateImageView(dev, &vci, NULL, &out->view));
   return true;
}

static void drop_image(struct image *im)
{
   vkDestroyImageView(dev, im->view, NULL);
   vkDestroyImage(dev, im->img, NULL);
   vkFreeMemory(dev, im->mem, NULL);
}

/* Execute one render pass over the given attachments. This is what makes the driver build a
 * background-object program for the tile layout, which is where Apple's compiler asserts. */
static void run_pass(struct image *colors, const struct fmt **cfmt, uint32_t ncolor,
                     struct image *depth, const struct fmt *dfmt, VkAttachmentLoadOp load_op,
                     uint32_t samples)
{
   VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   CHECK(vkResetCommandBuffer(cmd, 0));
   CHECK(vkBeginCommandBuffer(cmd, &bi));

   /* LOAD reads the tile from memory, so the attachments must be in a readable layout and hold
    * defined contents; UNDEFINED would make the load meaningless (and is a validation error). */
   VkImageMemoryBarrier barriers[MAX_COLOR + 1];
   uint32_t nbar = 0;
   for (uint32_t i = 0; i < ncolor; i++)
      barriers[nbar++] = (VkImageMemoryBarrier){
         .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
         .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = colors[i].img,
         .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
      };
   if (dfmt->bytes) {
      VkImageAspectFlags aspect = 0;
      if (dfmt->vk != VK_FORMAT_S8_UINT)
         aspect |= VK_IMAGE_ASPECT_DEPTH_BIT;
      if (dfmt->vk == VK_FORMAT_D32_SFLOAT_S8_UINT || dfmt->vk == VK_FORMAT_D24_UNORM_S8_UINT ||
          dfmt->vk == VK_FORMAT_S8_UINT)
         aspect |= VK_IMAGE_ASPECT_STENCIL_BIT;
      barriers[nbar++] = (VkImageMemoryBarrier){
         .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .dstAccessMask = VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
         .newLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = depth->img,
         .subresourceRange = {aspect, 0, 1, 0, 1},
      };
   }
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT |
                           VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT,
                        0, 0, NULL, 0, NULL, nbar, barriers);

   VkRenderingAttachmentInfo cattach[MAX_COLOR] = {0};
   for (uint32_t i = 0; i < ncolor; i++)
      cattach[i] = (VkRenderingAttachmentInfo){
         .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
         .imageView = colors[i].view,
         .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
         /* A LOAD of never-written contents is undefined but legal; we only need the driver to
          * build the load program, not to read anything meaningful. */
         .loadOp = load_op,
         .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
         .clearValue = {.color = {.float32 = {0.25f, 0.5f, 0.75f, 1.0f}}},
      };
   VkRenderingAttachmentInfo dattach = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
      .imageView = depth->view,
      .imageLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
      .loadOp = load_op,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .clearValue = {.depthStencil = {1.0f, 0}},
   };

   bool has_depth = dfmt->bytes && dfmt->vk != VK_FORMAT_S8_UINT;
   bool has_stencil = dfmt->vk == VK_FORMAT_D32_SFLOAT_S8_UINT ||
                      dfmt->vk == VK_FORMAT_D24_UNORM_S8_UINT || dfmt->vk == VK_FORMAT_S8_UINT;
   VkRenderingInfo ri = {
      .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
      .renderArea = {{0, 0}, {W, H}},
      .layerCount = 1,
      .colorAttachmentCount = ncolor,
      .pColorAttachments = ncolor ? cattach : NULL,
      .pDepthAttachment = has_depth ? &dattach : NULL,
      .pStencilAttachment = has_stencil ? &dattach : NULL,
   };
   vkCmdBeginRendering(cmd, &ri);
   vkCmdEndRendering(cmd);
   CHECK(vkEndCommandBuffer(cmd));

   VkSubmitInfo si = {
      .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
      .commandBufferCount = 1,
      .pCommandBuffers = &cmd,
   };
   CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
   CHECK(vkQueueWaitIdle(queue));
   (void)cfmt;
   (void)samples;
}

int main(int argc, char **argv)
{
   uint32_t max_color = MAX_COLOR;
   bool do_msaa = false;
   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "--msaa"))
         do_msaa = true;
      else if (!strcmp(argv[i], "--max-color") && i + 1 < argc)
         max_color = (uint32_t)atoi(argv[++i]);
      else {
         fprintf(stderr, "usage: %s [--max-color N] [--msaa]\n", argv[0]);
         return 2;
      }
   }
   if (max_color > MAX_COLOR)
      max_color = MAX_COLOR;

   VkApplicationInfo app = {
      .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
      .apiVersion = VK_API_VERSION_1_3,
   };
   VkInstanceCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
      .pApplicationInfo = &app,
   };
   CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t n = 1;
   if (vkEnumeratePhysicalDevices(inst, &n, &phys) != VK_SUCCESS || n == 0) {
      fprintf(stderr, "no vulkan device\n");
      return 1;
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(phys, &props);
   printf("gpu: %s\n", props.deviceName);

   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &nq, NULL);
   VkQueueFamilyProperties *qp = calloc(nq, sizeof(*qp));
   vkGetPhysicalDeviceQueueFamilyProperties(phys, &nq, qp);
   qfam = 0;
   for (uint32_t i = 0; i < nq; i++)
      if (qp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
         qfam = i;
         break;
      }
   free(qp);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
      .queueFamilyIndex = qfam,
      .queueCount = 1,
      .pQueuePriorities = &prio,
   };
   VkPhysicalDeviceDynamicRenderingFeatures dynren = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DYNAMIC_RENDERING_FEATURES,
      .dynamicRendering = VK_TRUE,
   };
   VkDeviceCreateInfo dci = {
      .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
      .pNext = &dynren,
      .queueCreateInfoCount = 1,
      .pQueueCreateInfos = &qci,
   };
   CHECK(vkCreateDevice(phys, &dci, NULL, &dev));
   vkGetDeviceQueue(dev, qfam, 0, &queue);

   VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = qfam,
   };
   CHECK(vkCreateCommandPool(dev, &pci, NULL, &pool));
   VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   CHECK(vkAllocateCommandBuffers(dev, &cbai, &cmd));

   const VkAttachmentLoadOp OPS[] = {VK_ATTACHMENT_LOAD_OP_CLEAR, VK_ATTACHMENT_LOAD_OP_LOAD};
   const char *OPNAME[] = {"CLEAR", "LOAD"};
   const uint32_t SAMPLES[] = {1, 4};

   uint32_t tried = 0, skipped = 0;
   for (uint32_t si = 0; si < (do_msaa ? 2u : 1u); si++) {
      uint32_t samples = SAMPLES[si];
      for (uint32_t op = 0; op < 2; op++) {
         for (uint32_t ncolor = 1; ncolor <= max_color; ncolor++) {
            for (uint32_t ci = 0; ci < NCOLOR; ci++) {
               for (uint32_t di = 0; di < NDEPTH; di++) {
                  const struct fmt *cf = &COLOR[ci];
                  const struct fmt *df = &DEPTH[di];
                  uint32_t total = cf->bytes * ncolor + df->bytes;

                  /* The line goes out BEFORE the work, flushed, so an abort names its trigger. */
                  printf("TRY  samples=%u %-5s ncolor=%u %-8s depth=%-5s total=%2u bytes\n",
                         samples, OPNAME[op], ncolor, cf->name, df->name, total);
                  fflush(stdout);

                  struct image colors[MAX_COLOR];
                  struct image depth = {0};
                  const struct fmt *cfmts[MAX_COLOR];
                  bool ok = true;
                  uint32_t made = 0;
                  for (uint32_t i = 0; i < ncolor && ok; i++) {
                     ok = make_image(cf->vk, false, samples, &colors[i]);
                     cfmts[i] = cf;
                     if (ok)
                        made++;
                  }
                  if (ok && df->bytes)
                     ok = make_image(df->vk, true, samples, &depth);
                  if (!ok) {
                     for (uint32_t i = 0; i < made; i++)
                        drop_image(&colors[i]);
                     skipped++;
                     continue;
                  }

                  run_pass(colors, cfmts, ncolor, &depth, df, OPS[op], samples);
                  tried++;

                  for (uint32_t i = 0; i < made; i++)
                     drop_image(&colors[i]);
                  if (df->bytes)
                     drop_image(&depth);
               }
            }
         }
      }
   }

   printf("swept %u combinations (%u skipped as unsupported), no abort\n", tried, skipped);
   /* If this exits green, the trigger needs something not varied here. Next knobs to try:
    * a real draw (a pipeline with a fragment shader) inside the pass; resolve attachments;
    * layered/multiview rendering; DONT_CARE load ops; and formats only reachable through venus,
    * such as the 24bpp ones vrend special-cases. */
   return 0;
}
