/* virgl: a CPU write through gbm_bo_map is not visible to another context
 * after gbm_bo_unmap returns.
 *
 * On virgl, unmapping a written texture queues a transfer to the host in the
 * producer's command stream, which is only submitted at that context's next
 * flush. gbm never flushes it, so a consumer in another context that imports
 * the dma-buf reads the buffer's previous contents.
 *
 * The producer writes a new value each iteration through gbm_bo_map; the
 * consumer, on a separate EGL display (a separate virgl context), imports
 * the dma-buf, attaches it to a framebuffer and reads it back.
 *
 * Build: cc -o virgl-shared-unmap virgl-shared-unmap.c -lgbm -lEGL -lGLESv2
 * Run:   ./virgl-shared-unmap
 *
 * Unfixed: the consumer reads stale values.
 * Fixed:   every iteration reads what was just written.
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <drm_fourcc.h>
#include <fcntl.h>
#include <gbm.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHECK(x)                                                               \
   do {                                                                        \
      if (!(x)) {                                                              \
         fprintf(stderr, "FAILED: %s\n", #x);                                  \
         exit(2);                                                              \
      }                                                                        \
   } while (0)

#define W 64
#define H 64
#define ITERATIONS 20

int
main(void)
{
   /* Producer: its own gbm device. */
   int prod_fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
   CHECK(prod_fd >= 0);
   struct gbm_device *prod = gbm_create_device(prod_fd);
   CHECK(prod);
   struct gbm_bo *bo = gbm_bo_create(prod, W, H, GBM_FORMAT_XRGB8888,
                                     GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
   CHECK(bo);
   const int dmabuf = gbm_bo_get_fd(bo);
   CHECK(dmabuf >= 0);

   /* Consumer: a second device, so a second driver context. */
   int cons_fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
   CHECK(cons_fd >= 0);
   struct gbm_device *cons = gbm_create_device(cons_fd);
   CHECK(cons);
   PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display =
      (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress(
         "eglGetPlatformDisplayEXT");
   CHECK(get_platform_display);
   EGLDisplay dpy = get_platform_display(EGL_PLATFORM_GBM_KHR, cons, NULL);
   CHECK(dpy != EGL_NO_DISPLAY);
   CHECK(eglInitialize(dpy, NULL, NULL));
   CHECK(eglBindAPI(EGL_OPENGL_ES_API));
   const EGLint ctx_attribs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
   EGLContext ctx =
      eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attribs);
   CHECK(ctx != EGL_NO_CONTEXT);
   CHECK(eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx));
   printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));

   PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target_texture =
      (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress(
         "glEGLImageTargetTexture2DOES");
   CHECK(image_target_texture);

   int stale = 0;
   for (uint32_t i = 1; i <= ITERATIONS; i++) {
      const uint32_t value = 0xff000000u | (i * 0x0a0b0c);

      uint32_t stride;
      void *map_data = NULL;
      uint32_t *px = gbm_bo_map(bo, 0, 0, W, H, GBM_BO_TRANSFER_WRITE, &stride,
                                &map_data);
      CHECK(px);
      for (uint32_t y = 0; y < H; y++)
         for (uint32_t x = 0; x < W; x++)
            px[y * (stride / 4) + x] = value;
      gbm_bo_unmap(bo, map_data);

      /* The consumer imports the buffer afresh, as a compositor would for a
       * newly attached client buffer. */
      const EGLAttrib img_attribs[] = {
         EGL_WIDTH, W,
         EGL_HEIGHT, H,
         EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_XRGB8888,
         EGL_DMA_BUF_PLANE0_FD_EXT, dmabuf,
         EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
         EGL_DMA_BUF_PLANE0_PITCH_EXT, gbm_bo_get_stride(bo),
         EGL_NONE,
      };
      EGLImage img = eglCreateImage(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT,
                                    NULL, img_attribs);
      CHECK(img != EGL_NO_IMAGE);
      GLuint tex, fb;
      glGenTextures(1, &tex);
      glBindTexture(GL_TEXTURE_2D, tex);
      image_target_texture(GL_TEXTURE_2D, img);
      glGenFramebuffers(1, &fb);
      glBindFramebuffer(GL_FRAMEBUFFER, fb);
      glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                             GL_TEXTURE_2D, tex, 0);
      CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) ==
            GL_FRAMEBUFFER_COMPLETE);

      uint8_t rgba[4];
      glReadPixels(W / 2, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, rgba);
      const uint32_t got = 0xff000000u | (rgba[0] << 16) | (rgba[1] << 8) |
                           rgba[2];
      const int ok = got == value;
      stale += !ok;
      printf("iteration %2u: wrote 0x%08x read 0x%08x %s\n", i, value, got,
             ok ? "" : "STALE");

      glDeleteFramebuffers(1, &fb);
      glDeleteTextures(1, &tex);
      eglDestroyImage(dpy, img);
   }

   printf("%d of %d reads stale\n", stale, ITERATIONS);
   return stale ? 1 : 0;
}
