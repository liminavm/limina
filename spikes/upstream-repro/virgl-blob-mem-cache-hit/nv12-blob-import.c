/*
 * virgl: a multi-planar dma-buf import of an untyped blob never sends
 * SET_TYPE, so the host resource stays untyped and cannot be sampled.
 *
 * Allocates exportable, host-visible memory with Vulkan (venus: the dma-buf
 * is an untyped virtio-gpu blob), fills it with a solid NV12 colour, exports
 * it, and imports it into EGL/GLES (virgl). Mode "nv12" imports both planes
 * from the one fd and samples it through GL_TEXTURE_EXTERNAL_OES; mode "r8"
 * imports only the luma plane as R8, a single-plane control that works
 * unfixed. ioctl() is interposed to log every SET_TYPE the driver submits.
 *
 * Build: cc -o nv12-blob-import nv12-blob-import.c -I/usr/include/libdrm \
 *           -lvulkan -lEGL -lGLESv2 -ldl
 * Run:   ./nv12-blob-import nv12     (and ./nv12-blob-import r8 as control)
 *        Needs a virtio-gpu guest with venus and virgl (blob=true).
 * Unfixed: nv12 sends no SET_TYPE (exit 1); the host rejects the sampler view.
 * Fixed:   nv12 sends SET_TYPE for the 2-plane resource (exit 0). Whether
 *          the pixel then reads ~(255,0,0) depends on the host accepting it.
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <dlfcn.h>
#include <drm_fourcc.h>
#include <stdarg.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <virtgpu_drm.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#define W 64
#define H 64
/* BT.601 limited-range red. */
#define Y_VAL 81
#define U_VAL 90
#define V_VAL 240

#define VK_CHECK(x)                                                         \
   do {                                                                     \
      VkResult r_ = (x);                                                    \
      if (r_ != VK_SUCCESS) {                                               \
         fprintf(stderr, "%s:%d %s = %d\n", __FILE__, __LINE__, #x, r_);    \
         exit(2);                                                           \
      }                                                                     \
   } while (0)

/* virgl protocol: VIRGL_CCMD_PIPE_RESOURCE_SET_TYPE and its layout. */
#define CCMD_PIPE_RESOURCE_SET_TYPE 49
static int set_type_count;

/* Interpose ioctl() to log every SET_TYPE command the driver submits. */
int
ioctl(int fd, unsigned long req, ...)
{
   static int (*real_ioctl)(int, unsigned long, ...);
   va_list ap;
   va_start(ap, req);
   void *arg = va_arg(ap, void *);
   va_end(ap);
   if (!real_ioctl)
      real_ioctl = dlsym(RTLD_NEXT, "ioctl");

   if (req == DRM_IOCTL_VIRTGPU_EXECBUFFER) {
      const struct drm_virtgpu_execbuffer *eb = arg;
      const uint32_t *cmd = (const uint32_t *)(uintptr_t)eb->command;
      for (uint32_t i = 0; i < eb->size / 4;) {
         uint32_t len = cmd[i] >> 16;
         if ((cmd[i] & 0xff) == CCMD_PIPE_RESOURCE_SET_TYPE) {
            uint32_t planes = (len - 8) / 2;
            printf("SET_TYPE res %u format %u %ux%u planes %u:", cmd[i + 1], cmd[i + 2],
                   cmd[i + 4], cmd[i + 5], planes);
            for (uint32_t p = 0; p < planes; p++)
               printf(" [stride %u offset %u]", cmd[i + 9 + 2 * p], cmd[i + 10 + 2 * p]);
            printf("\n");
            set_type_count++;
         }
         i += len + 1;
      }
   }
   return real_ioctl(fd, req, arg);
}

/* Returns a dma-buf fd holding an NV12 W x H image (Y at 0, UV at W * H). */
static int
make_nv12_dmabuf(void)
{
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .apiVersion = VK_API_VERSION_1_1 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
   VkInstance inst;
   VK_CHECK(vkCreateInstance(&ici, NULL, &inst));

   uint32_t n = 1;
   VkPhysicalDevice pd;
   vkEnumeratePhysicalDevices(inst, &n, &pd);
   if (!n) {
      fprintf(stderr, "no Vulkan device\n");
      exit(2);
   }
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pd, &props);
   printf("vulkan: %s\n", props.deviceName);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                   .queueCount = 1, .pQueuePriorities = &prio };
   const char *exts[] = { "VK_KHR_external_memory_fd", "VK_EXT_external_memory_dma_buf" };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                              .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                              .enabledExtensionCount = 2, .ppEnabledExtensionNames = exts };
   VkDevice dev;
   VK_CHECK(vkCreateDevice(pd, &dci, NULL, &dev));

   const VkDeviceSize size = 2 * W * H;
   VkExternalMemoryBufferCreateInfo ebci = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &ebci,
                              .size = size, .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT };
   VkBuffer buf;
   VK_CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(dev, buf, &req);

   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pd, &mp);
   const VkMemoryPropertyFlags want =
      VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
   uint32_t type = UINT32_MAX;
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      if ((req.memoryTypeBits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) {
         type = i;
         break;
      }
   if (type == UINT32_MAX) {
      fprintf(stderr, "no host-visible exportable memory type\n");
      exit(2);
   }

   VkExportMemoryAllocateInfo emai = { .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
                                       .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &emai,
                                .allocationSize = req.size, .memoryTypeIndex = type };
   VkDeviceMemory mem;
   VK_CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
   VK_CHECK(vkBindBufferMemory(dev, buf, mem, 0));

   uint8_t *p;
   VK_CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, (void **)&p));
   memset(p, Y_VAL, W * H);
   for (int i = 0; i < W * H / 2; i += 2) {
      p[W * H + i] = U_VAL;
      p[W * H + i + 1] = V_VAL;
   }
   vkUnmapMemory(dev, mem);

   PFN_vkGetMemoryFdKHR get_fd = (void *)vkGetDeviceProcAddr(dev, "vkGetMemoryFdKHR");
   VkMemoryGetFdInfoKHR gfi = { .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR, .memory = mem,
                                .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
   int fd;
   VK_CHECK(get_fd(dev, &gfi, &fd));
   /* The Vulkan objects are left alive: the fd keeps the memory anyway. */
   return fd;
}

static GLuint
compile(GLenum type, const char *src)
{
   GLuint s = glCreateShader(type);
   glShaderSource(s, 1, &src, NULL);
   glCompileShader(s);
   GLint ok;
   glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
   if (!ok) {
      char log[1024];
      glGetShaderInfoLog(s, sizeof(log), NULL, log);
      fprintf(stderr, "shader: %s\n", log);
      exit(2);
   }
   return s;
}

int
main(int argc, char **argv)
{
   int nv12 = !(argc > 1 && strcmp(argv[1], "r8") == 0);
   int fd = make_nv12_dmabuf();

   PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display =
      (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
   EGLDisplay dpy = get_platform_display(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
   if (!eglInitialize(dpy, NULL, NULL))
      return 2;
   eglBindAPI(EGL_OPENGL_ES_API);
   static const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
   EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attrs);
   if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
      return 2;
   printf("gles: %s\n", glGetString(GL_RENDERER));

   EGLint nv12_attrs[] = {
      EGL_WIDTH, W, EGL_HEIGHT, H,
      EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_NV12,
      EGL_DMA_BUF_PLANE0_FD_EXT, fd, EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
      EGL_DMA_BUF_PLANE0_PITCH_EXT, W,
      EGL_DMA_BUF_PLANE1_FD_EXT, fd, EGL_DMA_BUF_PLANE1_OFFSET_EXT, W * H,
      EGL_DMA_BUF_PLANE1_PITCH_EXT, W,
      EGL_YUV_COLOR_SPACE_HINT_EXT, EGL_ITU_REC601_EXT,
      EGL_SAMPLE_RANGE_HINT_EXT, EGL_YUV_NARROW_RANGE_EXT,
      EGL_NONE };
   EGLint r8_attrs[] = {
      EGL_WIDTH, W, EGL_HEIGHT, H,
      EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_R8,
      EGL_DMA_BUF_PLANE0_FD_EXT, fd, EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
      EGL_DMA_BUF_PLANE0_PITCH_EXT, W,
      EGL_NONE };
   PFNEGLCREATEIMAGEKHRPROC create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
   PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target =
      (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
   EGLImageKHR img = create_image(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL,
                                  nv12 ? nv12_attrs : r8_attrs);
   if (img == EGL_NO_IMAGE_KHR) {
      fprintf(stderr, "eglCreateImageKHR failed: 0x%x\n", eglGetError());
      return 2;
   }

   GLuint src;
   glGenTextures(1, &src);
   glBindTexture(GL_TEXTURE_EXTERNAL_OES, src);
   glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
   glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
   image_target(GL_TEXTURE_EXTERNAL_OES, img);

   GLuint dst, fbo;
   glGenTextures(1, &dst);
   glBindTexture(GL_TEXTURE_2D, dst);
   glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
   glGenFramebuffers(1, &fbo);
   glBindFramebuffer(GL_FRAMEBUFFER, fbo);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, dst, 0);

   GLuint prog = glCreateProgram();
   glAttachShader(prog, compile(GL_VERTEX_SHADER,
                                "attribute vec2 pos; varying vec2 tc;\n"
                                "void main() { tc = pos * 0.5 + 0.5; gl_Position = vec4(pos, 0.0, 1.0); }\n"));
   glAttachShader(prog, compile(GL_FRAGMENT_SHADER,
                                "#extension GL_OES_EGL_image_external : require\n"
                                "precision mediump float; varying vec2 tc;\n"
                                "uniform samplerExternalOES s;\n"
                                "void main() { gl_FragColor = texture2D(s, tc); }\n"));
   glBindAttribLocation(prog, 0, "pos");
   glLinkProgram(prog);
   glUseProgram(prog);
   static const float quad[] = { -1, -1, 1, -1, -1, 1, 1, 1 };
   glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, quad);
   glEnableVertexAttribArray(0);
   glViewport(0, 0, W, H);
   glClearColor(0, 0, 1, 1);
   glClear(GL_COLOR_BUFFER_BIT);
   glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

   uint8_t px[4];
   glReadPixels(W / 2, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
   printf("%s import: centre pixel rgba(%d,%d,%d,%d)", nv12 ? "NV12" : "R8 (luma plane)",
          px[0], px[1], px[2], px[3]);

   int ok;
   if (nv12) {
      ok = px[0] >= 250 && px[1] <= 5 && px[2] <= 5;
      printf(", want ~(255,0,0)\n");
   } else {
      ok = abs(px[0] - Y_VAL) <= 1;
      printf(", want r=%d\n", Y_VAL);
   }
   printf("pixel %s\n", ok ? "matches" : "does not match");
   printf("SET_TYPE commands sent: %d\n", set_type_count);
   printf("%s\n", set_type_count ? "OK: SET_TYPE sent" : "FAIL: no SET_TYPE for the import");
   return !set_type_count;
}
