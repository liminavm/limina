/*
 * vaExportSurfaceHandle(DRM_PRIME_2) reports objects[].size = 0 on virgl.
 *
 * Creates an NV12 and a BGRA surface on the VideoProc entrypoint (no decode
 * support needed), exports each, and prints every object's reported size next
 * to the size of the dma-buf itself (lseek(fd, 0, SEEK_END)).
 *
 * Build: cc -o va-export-size va-export-size.c -lva -lva-drm
 * Run:   ./va-export-size [/dev/dri/renderD128]
 * Unfixed: "size=0" for every object, exit 1.  Fixed: size matches the
 * dma-buf size, exit 0.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>

static int
export_one(VADisplay dpy, unsigned rt_format, unsigned fourcc, const char *name)
{
   VASurfaceAttrib attr = {
      .type = VASurfaceAttribPixelFormat,
      .flags = VA_SURFACE_ATTRIB_SETTABLE,
      .value = { .type = VAGenericValueTypeInteger, .value.i = fourcc },
   };
   VASurfaceID surf;
   VAStatus st = vaCreateSurfaces(dpy, rt_format, 256, 256, &surf, 1, &attr, 1);
   if (st != VA_STATUS_SUCCESS) {
      printf("%s: vaCreateSurfaces: %s\n", name, vaErrorStr(st));
      return 2;
   }

   VADRMPRIMESurfaceDescriptor desc = {0};
   st = vaExportSurfaceHandle(dpy, surf, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                              VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS,
                              &desc);
   if (st != VA_STATUS_SUCCESS) {
      printf("%s: vaExportSurfaceHandle: %s\n", name, vaErrorStr(st));
      vaDestroySurfaces(dpy, &surf, 1);
      return 2;
   }

   int bad = 0;
   for (unsigned i = 0; i < desc.num_objects; i++) {
      off_t real = lseek(desc.objects[i].fd, 0, SEEK_END);
      printf("%s: object %u: size=%u dmabuf=%lld\n", name, i,
             desc.objects[i].size, (long long)real);
      if (desc.objects[i].size == 0 || (real > 0 && desc.objects[i].size != (uint32_t)real))
         bad = 1;
      close(desc.objects[i].fd);
   }
   vaDestroySurfaces(dpy, &surf, 1);
   return bad;
}

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
   if (vaInitialize(dpy, &major, &minor) != VA_STATUS_SUCCESS) {
      fprintf(stderr, "vaInitialize failed\n");
      return 2;
   }
   printf("driver: %s\n", vaQueryVendorString(dpy));

   int bad = export_one(dpy, VA_RT_FORMAT_YUV420, VA_FOURCC_NV12, "NV12");
   bad |= export_one(dpy, VA_RT_FORMAT_RGB32, VA_FOURCC_BGRA, "BGRA");

   vaTerminate(dpy);
   close(fd);
   printf("%s\n", bad ? "FAIL: reported size does not match the dma-buf" : "OK");
   return bad;
}
