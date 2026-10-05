/*
 * eglExportDMABUFImageMESA returns EGL_TRUE when the driver fails to export
 * an fd, leaving the caller's fds[] untouched.
 *
 * Creates a GLES texture and an EGLImage from it, then fills the process fd
 * table (RLIMIT_NOFILE lowered and the free slots taken with /dev/null) so
 * that exporting a dma-buf fd must fail, and calls eglExportDMABUFImageMESA
 * with fds[] preset to a sentinel.
 *
 * Build: cc -o egl-export-emfile egl-export-emfile.c -lEGL -lGLESv2
 * Run:   ./egl-export-emfile [/dev/dri/renderD128]   (any driver that can
 *        export dma-bufs, on the surfaceless platform)
 * Unfixed: "returned EGL_TRUE, fds[0]=0x7f7f7f7f" (exit 1).
 * Fixed:   "returned EGL_FALSE, fds[0]=-1" (exit 0).
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

#define SENTINEL 0x7f7f7f7f

int
main(void)
{
   PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display =
      (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
   EGLDisplay dpy = get_platform_display(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
   if (!eglInitialize(dpy, NULL, NULL)) {
      fprintf(stderr, "eglInitialize failed\n");
      return 2;
   }
   const char *exts = eglQueryString(dpy, EGL_EXTENSIONS);
   if (!strstr(exts, "EGL_MESA_image_dma_buf_export")) {
      fprintf(stderr, "no EGL_MESA_image_dma_buf_export\n");
      return 2;
   }

   eglBindAPI(EGL_OPENGL_ES_API);
   static const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
   EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attrs);
   if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
      fprintf(stderr, "context setup failed\n");
      return 2;
   }
   printf("renderer: %s\n", glGetString(GL_RENDERER));

   GLuint tex;
   glGenTextures(1, &tex);
   glBindTexture(GL_TEXTURE_2D, tex);
   glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
   glFinish();

   PFNEGLCREATEIMAGEKHRPROC create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
   PFNEGLEXPORTDMABUFIMAGEQUERYMESAPROC export_query =
      (void *)eglGetProcAddress("eglExportDMABUFImageQueryMESA");
   PFNEGLEXPORTDMABUFIMAGEMESAPROC export_image =
      (void *)eglGetProcAddress("eglExportDMABUFImageMESA");

   EGLImageKHR img = create_image(dpy, ctx, EGL_GL_TEXTURE_2D_KHR,
                                  (EGLClientBuffer)(uintptr_t)tex, NULL);
   if (img == EGL_NO_IMAGE_KHR) {
      fprintf(stderr, "eglCreateImageKHR failed\n");
      return 2;
   }
   int fourcc, nplanes;
   if (!export_query(dpy, img, &fourcc, &nplanes, NULL)) {
      fprintf(stderr, "eglExportDMABUFImageQueryMESA failed\n");
      return 2;
   }
   printf("image: fourcc %.4s, %d plane(s)\n", (char *)&fourcc, nplanes);

   /* Sanity: with free fds the export works. */
   int fds[4] = { SENTINEL, SENTINEL, SENTINEL, SENTINEL };
   EGLint strides[4], offsets[4];
   EGLBoolean ok = export_image(dpy, img, fds, strides, offsets);
   printf("with free fds:   returned %s, fds[0]=%d\n", ok ? "EGL_TRUE" : "EGL_FALSE", fds[0]);
   if (ok && fds[0] >= 0)
      close(fds[0]);

   /* Exhaust the fd table: nothing can dup or PRIME-export an fd now. */
   struct rlimit rl = { 64, 64 };
   setrlimit(RLIMIT_NOFILE, &rl);
   int filled = 0;
   while (open("/dev/null", O_RDONLY) >= 0)
      filled++;
   printf("fd table full (%d filler fds, last errno: %s)\n", filled, strerror(errno));

   for (int i = 0; i < 4; i++)
      fds[i] = SENTINEL;
   ok = export_image(dpy, img, fds, strides, offsets);
   printf("with no free fd: returned %s, fds[0]=%s%x\n", ok ? "EGL_TRUE" : "EGL_FALSE",
          fds[0] == -1 ? "-" : "0x", fds[0] == -1 ? 1 : (unsigned)fds[0]);

   int bad = ok || fds[0] != -1;
   printf("%s\n", bad ? "FAIL: export reported success without an fd" : "OK");
   return bad;
}
