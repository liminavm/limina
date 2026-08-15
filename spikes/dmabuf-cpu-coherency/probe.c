// Minimal vehicle for the "CPU write to a mapped LINEAR dmabuf stops being visible
// to the GPU" report (docs/hardening-backlog.md, synoik 2026-08-14).
//
// Sequence, N times:
//   gbm_bo_map(WRITE) -> fill the whole buffer with pattern[i] -> gbm_bo_unmap
//   GPU: vkCmdCopyImageToBuffer from the imported dmabuf into a host-visible buffer
//   read back texel (0,0) and compare against pattern[i]
//
// The Vulkan import is done ONCE by default (matching the reporter's import cache);
// --reimport re-creates the VkImage every pass, --gpu-writer uses a GPU clear as the
// producer instead of the CPU map (the reporter's control, expected to always pass).
//
// Build in-guest:
//   gcc -O1 -g -o probe probe.c $(pkg-config --cflags --libs gbm) -lvulkan
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <time.h>

#include <gbm.h>
#include <vulkan/vulkan.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>

#define W 256
#define H 64
#define DRM_FORMAT_MOD_LINEAR 0ull

#define VKOK(x)                                                                \
  do {                                                                         \
    VkResult _r = (x);                                                         \
    if (_r != VK_SUCCESS) {                                                    \
      fprintf(stderr, "%s:%d: %s = %d\n", __FILE__, __LINE__, #x, _r);         \
      exit(1);                                                                 \
    }                                                                          \
  } while (0)

static VkInstance inst;
static VkPhysicalDevice phys;
static VkDevice dev;
static VkQueue queue;
static uint32_t qfam;
static VkCommandPool pool;
static VkCommandBuffer cmd;
static VkFence fence;
static VkBuffer readback;
static VkDeviceMemory readback_mem;
static void *readback_ptr;

static PFN_vkGetMemoryFdPropertiesKHR p_getMemoryFdProperties;

struct import {
  VkImage image;
  VkDeviceMemory mem;
  bool first_use; // next barrier must come out of UNDEFINED/PREINITIALIZED
};

static void vk_init(void) {
  const char *iexts[] = {
      VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME,
      VK_KHR_EXTERNAL_MEMORY_CAPABILITIES_EXTENSION_NAME,
  };
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                           .apiVersion = VK_API_VERSION_1_1};
  VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                              .pApplicationInfo = &app,
                              .enabledExtensionCount = 2,
                              .ppEnabledExtensionNames = iexts};
  VKOK(vkCreateInstance(&ici, NULL, &inst));

  uint32_t n = 1;
  VkResult r = vkEnumeratePhysicalDevices(inst, &n, &phys);
  if (r != VK_SUCCESS || n == 0) {
    fprintf(stderr, "no vulkan device (r=%d n=%u)\n", r, n);
    exit(1);
  }
  VkPhysicalDeviceProperties props;
  vkGetPhysicalDeviceProperties(phys, &props);
  printf("gpu: %s\n", props.deviceName);

  uint32_t nq = 0;
  vkGetPhysicalDeviceQueueFamilyProperties(phys, &nq, NULL);
  VkQueueFamilyProperties *qp = calloc(nq, sizeof(*qp));
  vkGetPhysicalDeviceQueueFamilyProperties(phys, &nq, qp);
  qfam = UINT32_MAX;
  for (uint32_t i = 0; i < nq; i++)
    if (qp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
      qfam = i;
      break;
    }
  free(qp);

  const char *dexts[] = {
      VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
      VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
      VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
      VK_KHR_IMAGE_FORMAT_LIST_EXTENSION_NAME,
      VK_EXT_QUEUE_FAMILY_FOREIGN_EXTENSION_NAME,
  };
  float prio = 1.0f;
  VkDeviceQueueCreateInfo qci = {.sType =
                                     VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                 .queueFamilyIndex = qfam,
                                 .queueCount = 1,
                                 .pQueuePriorities = &prio};
  VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                            .queueCreateInfoCount = 1,
                            .pQueueCreateInfos = &qci,
                            .enabledExtensionCount =
                                sizeof(dexts) / sizeof(dexts[0]),
                            .ppEnabledExtensionNames = dexts};
  VKOK(vkCreateDevice(phys, &dci, NULL, &dev));
  vkGetDeviceQueue(dev, qfam, 0, &queue);

  p_getMemoryFdProperties = (PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(
      dev, "vkGetMemoryFdPropertiesKHR");

  VkCommandPoolCreateInfo pci = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = qfam};
  VKOK(vkCreateCommandPool(dev, &pci, NULL, &pool));
  VkCommandBufferAllocateInfo cbai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1};
  VKOK(vkAllocateCommandBuffers(dev, &cbai, &cmd));
  VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
  VKOK(vkCreateFence(dev, &fci, NULL, &fence));
}

static uint32_t find_mem(uint32_t bits, VkMemoryPropertyFlags want) {
  VkPhysicalDeviceMemoryProperties mp;
  vkGetPhysicalDeviceMemoryProperties(phys, &mp);
  for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
    if ((bits & (1u << i)) &&
        (mp.memoryTypes[i].propertyFlags & want) == want)
      return i;
  fprintf(stderr, "no memory type for bits=%#x want=%#x\n", bits, want);
  exit(1);
}

static void make_readback(void) {
  VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                            .size = (VkDeviceSize)W * H * 4,
                            .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                            .sharingMode = VK_SHARING_MODE_EXCLUSIVE};
  VKOK(vkCreateBuffer(dev, &bci, NULL, &readback));
  VkMemoryRequirements mr;
  vkGetBufferMemoryRequirements(dev, readback, &mr);
  VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = mr.size,
      .memoryTypeIndex =
          find_mem(mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                          VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)};
  VKOK(vkAllocateMemory(dev, &mai, NULL, &readback_mem));
  VKOK(vkBindBufferMemory(dev, readback, readback_mem, 0));
  VKOK(vkMapMemory(dev, readback_mem, 0, VK_WHOLE_SIZE, 0, &readback_ptr));
}

// Import (or re-import) the dmabuf as a LINEAR VkImage.
static void import_bo(struct gbm_bo *bo, struct import *im, bool for_write) {
  int fd = gbm_bo_get_fd(bo);
  if (fd < 0) {
    fprintf(stderr, "gbm_bo_get_fd failed\n");
    exit(1);
  }
  uint32_t stride = gbm_bo_get_stride(bo);

  VkSubresourceLayout plane = {.offset = gbm_bo_get_offset(bo, 0),
                               .rowPitch = stride};
  VkImageDrmFormatModifierExplicitCreateInfoEXT modinfo = {
      .sType =
          VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
      .drmFormatModifier = DRM_FORMAT_MOD_LINEAR,
      .drmFormatModifierPlaneCount = 1,
      .pPlaneLayouts = &plane};
  VkExternalMemoryImageCreateInfo eici = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .pNext = &modinfo,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
  VkImageCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .pNext = &eici,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = VK_FORMAT_B8G8R8A8_UNORM, // ARGB8888 little-endian
      .extent = {W, H, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
      .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT |
               VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
      .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .initialLayout = for_write ? VK_IMAGE_LAYOUT_UNDEFINED
                                 : VK_IMAGE_LAYOUT_PREINITIALIZED};
  VKOK(vkCreateImage(dev, &ici, NULL, &im->image));

  VkMemoryRequirements mr;
  vkGetImageMemoryRequirements(dev, im->image, &mr);
  VkMemoryFdPropertiesKHR fdprops = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
  VKOK(p_getMemoryFdProperties(
      dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fdprops));

  VkMemoryDedicatedAllocateInfo dedicated = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .image = im->image};
  VkImportMemoryFdInfoKHR imfd = {
      .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
      .pNext = &dedicated,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
      .fd = fd};
  VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &imfd,
      .allocationSize = mr.size,
      .memoryTypeIndex = find_mem(mr.memoryTypeBits & fdprops.memoryTypeBits, 0)};
  VKOK(vkAllocateMemory(dev, &mai, NULL, &im->mem)); // consumes fd
  VKOK(vkBindImageMemory(dev, im->image, im->mem, 0));
  im->first_use = true;
}

static void drop_import(struct import *im) {
  vkDestroyImage(dev, im->image, NULL);
  vkFreeMemory(dev, im->mem, NULL);
}

// Copy the imported image into the readback buffer and wait.
static void gpu_read(struct import *im) {
  VKOK(vkResetCommandBuffer(cmd, 0));
  VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
  VKOK(vkBeginCommandBuffer(cmd, &bi));

  VkImageMemoryBarrier acquire = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_HOST_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
      .oldLayout = im->first_use ? VK_IMAGE_LAYOUT_PREINITIALIZED
                                 : VK_IMAGE_LAYOUT_GENERAL,
      .newLayout = VK_IMAGE_LAYOUT_GENERAL,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
      .dstQueueFamilyIndex = qfam,
      .image = im->image,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_HOST_BIT,
                       VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                       &acquire);
  im->first_use = false;

  VkBufferImageCopy copy = {
      .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
      .imageExtent = {W, H, 1}};
  vkCmdCopyImageToBuffer(cmd, im->image, VK_IMAGE_LAYOUT_GENERAL, readback, 1,
                         &copy);

  VkImageMemoryBarrier release = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
      .newLayout = VK_IMAGE_LAYOUT_GENERAL,
      .srcQueueFamilyIndex = qfam,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
      .image = im->image,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                       VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0,
                       NULL, 1, &release);
  VKOK(vkEndCommandBuffer(cmd));

  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                     .commandBufferCount = 1,
                     .pCommandBuffers = &cmd};
  VKOK(vkResetFences(dev, 1, &fence));
  VKOK(vkQueueSubmit(queue, 1, &si, fence));
  VKOK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
}

// GPU producer control: clear the image to `color` on the GPU instead of the CPU.
static void gpu_write(struct import *im, uint32_t color) {
  VKOK(vkResetCommandBuffer(cmd, 0));
  VkCommandBufferBeginInfo bi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
  VKOK(vkBeginCommandBuffer(cmd, &bi));
  VkImageMemoryBarrier to_dst = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .oldLayout = im->first_use ? VK_IMAGE_LAYOUT_PREINITIALIZED
                                 : VK_IMAGE_LAYOUT_GENERAL,
      .newLayout = VK_IMAGE_LAYOUT_GENERAL,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
      .dstQueueFamilyIndex = qfam,
      .image = im->image,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                       VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                       &to_dst);
  im->first_use = false;
  VkClearColorValue cc = {.float32 = {((color >> 16) & 0xff) / 255.0f,
                                      ((color >> 8) & 0xff) / 255.0f,
                                      (color & 0xff) / 255.0f,
                                      ((color >> 24) & 0xff) / 255.0f}};
  VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
  vkCmdClearColorImage(cmd, im->image, VK_IMAGE_LAYOUT_GENERAL, &cc, 1, &range);
  VKOK(vkEndCommandBuffer(cmd));
  VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                     .commandBufferCount = 1,
                     .pCommandBuffers = &cmd};
  VKOK(vkResetFences(dev, 1, &fence));
  VKOK(vkQueueSubmit(queue, 1, &si, fence));
  VKOK(vkWaitForFences(dev, 1, &fence, VK_TRUE, UINT64_MAX));
}

// ---------------------------------------------------------------------------
// GL producer: render into the SAME shared bo through vrend, then let venus read
// it. Same control-queue-vs-ring seam as the CPU map, minus the transfer -- the
// question is whether the hazard is broader than the host-visible mapping.
// ---------------------------------------------------------------------------
static EGLDisplay egl_dpy = EGL_NO_DISPLAY;
static EGLContext egl_ctx = EGL_NO_CONTEXT;
static GLuint gl_tex, gl_fbo;

static void gl_init(struct gbm_device *gbm, struct gbm_bo *bo) {
  PFNEGLGETPLATFORMDISPLAYEXTPROC get_dpy =
      (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress(
          "eglGetPlatformDisplayEXT");
  egl_dpy = get_dpy ? get_dpy(EGL_PLATFORM_GBM_KHR, gbm, NULL)
                    : eglGetDisplay((EGLNativeDisplayType)gbm);
  if (egl_dpy == EGL_NO_DISPLAY || !eglInitialize(egl_dpy, NULL, NULL)) {
    fprintf(stderr, "eglInitialize failed\n");
    exit(1);
  }
  eglBindAPI(EGL_OPENGL_ES_API);
  EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
  egl_ctx = eglCreateContext(egl_dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
  if (egl_ctx == EGL_NO_CONTEXT) {
    fprintf(stderr, "eglCreateContext failed (0x%x)\n", eglGetError());
    exit(1);
  }
  if (!eglMakeCurrent(egl_dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, egl_ctx)) {
    fprintf(stderr, "eglMakeCurrent failed (0x%x)\n", eglGetError());
    exit(1);
  }

  int fd = gbm_bo_get_fd(bo);
  EGLint iattr[] = {EGL_WIDTH,
                    W,
                    EGL_HEIGHT,
                    H,
                    EGL_LINUX_DRM_FOURCC_EXT,
                    (EGLint)GBM_FORMAT_ARGB8888,
                    EGL_DMA_BUF_PLANE0_FD_EXT,
                    fd,
                    EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                    (EGLint)gbm_bo_get_offset(bo, 0),
                    EGL_DMA_BUF_PLANE0_PITCH_EXT,
                    (EGLint)gbm_bo_get_stride(bo),
                    EGL_NONE};
  PFNEGLCREATEIMAGEKHRPROC create_image =
      (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
  EGLImageKHR img = create_image(egl_dpy, EGL_NO_CONTEXT,
                                 EGL_LINUX_DMA_BUF_EXT, NULL, iattr);
  close(fd);
  if (img == EGL_NO_IMAGE_KHR) {
    fprintf(stderr, "eglCreateImageKHR failed (0x%x)\n", eglGetError());
    exit(1);
  }

  PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target =
      (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress(
          "glEGLImageTargetTexture2DOES");
  glGenTextures(1, &gl_tex);
  glBindTexture(GL_TEXTURE_2D, gl_tex);
  image_target(GL_TEXTURE_2D, img);
  glGenFramebuffers(1, &gl_fbo);
  glBindFramebuffer(GL_FRAMEBUFFER, gl_fbo);
  glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                         gl_tex, 0);
  GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
  if (st != GL_FRAMEBUFFER_COMPLETE) {
    fprintf(stderr, "FBO incomplete (0x%x)\n", st);
    exit(1);
  }
}

// `finish` picks the hand-off a producer performs: glFlush is the cheap one a
// compositor does before handing the buffer over; glFinish waits for the virgl
// fence, i.e. the same wait the CPU path needs.
static void gl_write(uint32_t color, bool finish) {
  glBindFramebuffer(GL_FRAMEBUFFER, gl_fbo);
  glViewport(0, 0, W, H);
  glClearColor(((color >> 16) & 0xff) / 255.0f, ((color >> 8) & 0xff) / 255.0f,
               (color & 0xff) / 255.0f, ((color >> 24) & 0xff) / 255.0f);
  glClear(GL_COLOR_BUFFER_BIT);
  if (finish)
    glFinish();
  else
    glFlush();
}

static void cpu_write(struct gbm_bo *bo, uint32_t color) {
  void *map_data = NULL;
  uint32_t stride = 0;
  void *p = gbm_bo_map(bo, 0, 0, W, H, GBM_BO_TRANSFER_WRITE, &stride,
                       &map_data);
  if (!p) {
    fprintf(stderr, "gbm_bo_map failed\n");
    exit(1);
  }
  for (uint32_t y = 0; y < H; y++) {
    uint32_t *row = (uint32_t *)((char *)p + (size_t)y * stride);
    for (uint32_t x = 0; x < W; x++)
      row[x] = color;
  }
  gbm_bo_unmap(bo, map_data);
}

// CLOCK_REALTIME is anchored to the host's, so guest and host [COH] stamps are
// directly comparable (see the limina-guest-clock memory).
static void stamp(const char *what, int pass) {
  struct timespec ts;
  clock_gettime(CLOCK_REALTIME, &ts);
  printf("[COH-guest] %ld.%06ld %s pass=%d\n", (long)ts.tv_sec, ts.tv_nsec / 1000,
         what, pass);
}

int main(int argc, char **argv) {
  int passes = 6, sleep_ms = 0;
  bool reimport = false, gpu_producer = false, read_between = true;
  bool touch_after_write = false;
  bool gl_producer = false, gl_finish = false;
  bool trace = getenv("LIMINA_COH_TRACE") != NULL;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "--reimport"))
      reimport = true;
    else if (!strcmp(argv[i], "--gpu-writer"))
      gpu_producer = true;
    else if (!strcmp(argv[i], "--no-read-between"))
      read_between = false;
    else if (!strcmp(argv[i], "--touch-after-write"))
      touch_after_write = true;
    else if (!strcmp(argv[i], "--gl-writer"))
      gl_producer = true;
    else if (!strcmp(argv[i], "--gl-writer-finish"))
      gl_producer = gl_finish = true;
    else if (!strcmp(argv[i], "--sleep-ms") && i + 1 < argc)
      sleep_ms = atoi(argv[++i]);
    else if (!strcmp(argv[i], "--passes") && i + 1 < argc)
      passes = atoi(argv[++i]);
    else {
      fprintf(stderr,
              "usage: %s [--passes N] [--reimport] [--gpu-writer] "
              "[--no-read-between] [--touch-after-write] [--sleep-ms N] "
              "[--gl-writer] [--gl-writer-finish]\n",
              argv[0]);
      return 2;
    }
  }

  int drmfd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
  if (drmfd < 0) {
    perror("open renderD128");
    return 1;
  }
  struct gbm_device *gbm = gbm_create_device(drmfd);
  if (!gbm) {
    fprintf(stderr, "gbm_create_device failed\n");
    return 1;
  }
  uint64_t mod = DRM_FORMAT_MOD_LINEAR;
  struct gbm_bo *bo = gbm_bo_create_with_modifiers2(
      gbm, W, H, GBM_FORMAT_ARGB8888, &mod, 1,
      GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);
  if (!bo)
    bo = gbm_bo_create(gbm, W, H, GBM_FORMAT_ARGB8888,
                       GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);
  if (!bo) {
    fprintf(stderr, "gbm_bo_create failed\n");
    return 1;
  }
  printf("bo: %ux%u stride=%u modifier=%#llx\n", W, H, gbm_bo_get_stride(bo),
         (unsigned long long)gbm_bo_get_modifier(bo));

  if (gl_producer)
    gl_init(gbm, bo);

  vk_init();
  make_readback();

  struct import im;
  if (!reimport)
    import_bo(bo, &im, gpu_producer || gl_producer);

  int failures = 0;
  for (int i = 0; i < passes; i++) {
    uint32_t color = 0xff000000u | ((uint32_t)(i * 37 + 1) << 16) |
                     ((uint32_t)(i * 53 + 7) << 8) | (uint32_t)(i * 11 + 3);
    if (reimport)
      import_bo(bo, &im, gpu_producer || gl_producer);

    if (gpu_producer)
      gpu_write(&im, color);
    else if (gl_producer)
      gl_write(color, gl_finish);
    else
      cpu_write(bo, color);
    if (trace)
      stamp("unmap-returned", i);

    // Discriminators for "the guest-side transfer is merely LATE":
    //   --touch-after-write maps the bo again for read, which makes the guest
    //     wait for the resource to go idle (VIRTGPU_WAIT) before we read.
    //   --sleep-ms just gives the host wall-clock time.
    if (touch_after_write) {
      void *md = NULL;
      uint32_t st = 0;
      void *p = gbm_bo_map(bo, 0, 0, W, H, GBM_BO_TRANSFER_READ, &st, &md);
      if (p)
        gbm_bo_unmap(bo, md);
    }
    if (sleep_ms)
      usleep((useconds_t)sleep_ms * 1000);

    if (read_between || i == passes - 1) {
      memset(readback_ptr, 0xcd, (size_t)W * H * 4);
      if (trace)
        stamp("gpu-read-submit", i);
      gpu_read(&im);
      if (trace)
        stamp("gpu-read-done", i);
      uint32_t got = ((uint32_t *)readback_ptr)[0];
      uint32_t got_mid = ((uint32_t *)readback_ptr)[(H / 2) * W + W / 2];
      bool ok = got == color && got_mid == color;
      if (!ok)
        failures++;
      printf("pass %d: want %08x got %08x (mid %08x) %s\n", i, color, got,
             got_mid, ok ? "OK" : "STALE");
    }
    if (reimport)
      drop_import(&im);
  }
  if (!reimport)
    drop_import(&im);

  printf("%s: %d/%d passes stale\n", failures ? "FAIL" : "PASS", failures,
         passes);
  return failures ? 1 : 0;
}
