/*
 * virgl advertises three-plane 4:2:0 (YV12, I420) as a decode target.
 *
 * For every VLD (decode) config the driver exposes, lists the surface pixel
 * formats from vaQuerySurfaceAttributes. A decoder emits NV12 (P010 for
 * 10-bit); YV12/I420 in that list make ffmpeg pick them by exact match for an
 * 8-bit 4:2:0 stream, and consumers that only take NV12 then fall back to
 * software decode.
 *
 * Build: cc -o va-decode-formats va-decode-formats.c -lva -lva-drm
 * Run:   ./va-decode-formats [/dev/dri/renderD128]
 *        Needs a virgl host with video enabled (virglrenderer built with
 *        -Dvideo=true, VIRGL_RENDERER_USE_VIDEO set by the VMM, and a host
 *        VA-API decoder).
 * Unfixed: YV12 and I420 listed for decode profiles (exit 1).
 * Fixed:   only NV12/P010-style formats listed (exit 0).
 * No decode profiles at all: exit 77.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_str.h>

int
main(int argc, char **argv)
{
   const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
   int fd = open(node, O_RDWR);
   if (fd < 0) {
      perror(node);
      return 2;
   }
   VADisplay dpy = vaGetDisplayDRM(fd);
   int major, minor;
   if (vaInitialize(dpy, &major, &minor) != VA_STATUS_SUCCESS)
      return 2;
   printf("driver: %s\n", vaQueryVendorString(dpy));

   int np = vaMaxNumProfiles(dpy);
   VAProfile *profiles = calloc(np, sizeof(*profiles));
   vaQueryConfigProfiles(dpy, profiles, &np);
   int ne_max = vaMaxNumEntrypoints(dpy);
   VAEntrypoint *eps = calloc(ne_max, sizeof(*eps));

   int decode_configs = 0, planar = 0;
   for (int p = 0; p < np; p++) {
      int ne = 0;
      if (vaQueryConfigEntrypoints(dpy, profiles[p], eps, &ne) != VA_STATUS_SUCCESS)
         continue;
      for (int e = 0; e < ne; e++) {
         if (eps[e] != VAEntrypointVLD)
            continue;
         VAConfigID cfg;
         if (vaCreateConfig(dpy, profiles[p], VAEntrypointVLD, NULL, 0, &cfg) != VA_STATUS_SUCCESS)
            continue;
         decode_configs++;
         unsigned na = 0;
         vaQuerySurfaceAttributes(dpy, cfg, NULL, &na);
         VASurfaceAttrib *attrs = calloc(na, sizeof(*attrs));
         vaQuerySurfaceAttributes(dpy, cfg, attrs, &na);
         printf("%-28s:", vaProfileStr(profiles[p]));
         for (unsigned a = 0; a < na; a++) {
            if (attrs[a].type != VASurfaceAttribPixelFormat)
               continue;
            unsigned f = attrs[a].value.value.i;
            printf(" %.4s", (char *)&f);
            if (f == VA_FOURCC_YV12 || f == VA_FOURCC_I420 || f == VA_FOURCC_IYUV)
               planar++;
         }
         printf("\n");
         free(attrs);
         vaDestroyConfig(dpy, cfg);
      }
   }

   vaTerminate(dpy);
   close(fd);
   if (!decode_configs) {
      printf("SKIP: no decode (VLD) profiles on this device\n");
      return 77;
   }
   printf("%s\n", planar ? "FAIL: three-plane 4:2:0 offered as a decode target" : "OK");
   return planar ? 1 : 0;
}
