/*
 * Measurement aid, not part of the reproducer: LD_PRELOAD shim that lets the
 * gfx vl_compositor run on virglrenderer's vrend.
 *
 * Since 210e557f7e0 ("vl: Support blending with gfx compositor") the
 * compositor's video-buffer fragment shader contains
 *     TEX TEMP[0].w, IN[0], SAMP[0].wwww, 2D_ARRAY
 * vrend 1.3.0 cannot translate a swizzled sampler operand, the shader fails to
 * compile, the context is put in error, and every VA-API post-processing
 * result reads back as zeros. The swizzle is redundant (the .w writemask
 * already selects alpha), so this shim blanks it out of the TGSI text in each
 * EXECBUFFER before it reaches the host, keeping every length and offset.
 *
 * Build: cc -shared -fPIC -o vrend-swizzle-shim.so vrend-swizzle-shim.c -ldl
 * Use:   LD_PRELOAD=./vrend-swizzle-shim.so ./va-vpp-csc nv12
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdarg.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <libdrm/virtgpu_drm.h>

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
      struct drm_virtgpu_execbuffer *eb = arg;
      char *buf = (char *)(uintptr_t)eb->command;
      static const char needle[] = "SAMP[0].wwww";
      const size_t n = sizeof(needle) - 1;
      for (size_t i = 0; i + n <= eb->size; i++)
         if (memcmp(buf + i, needle, n) == 0)
            memset(buf + i + 7, ' ', 5);
   }
   return real_ioctl(fd, req, arg);
}
