/*
 * Is an exported VA surface's dmabuf big enough for the frame it claims to hold?
 *
 * GStreamer's GL path, when no direct dmabuf-to-texture import is available, falls back to
 * mmapping the exported dmabuf and memcpying the frame out of it. On this stack that copy
 * dies with SIGBUS in __memcpy_generic, which is what a mapping that ends early looks like
 * -- so the question is simply whether the object is as large as the layout says.
 *
 * No decoding here on purpose: a plain vaCreateSurfaces is enough to ask it, which keeps the
 * probe independent of any codec. All three codecs crash the same way.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>

static void die(const char *what, VAStatus st)
{
    fprintf(stderr, "%s: %s\n", what, vaErrorStr(st));
    exit(1);
}

int main(int argc, char **argv)
{
    const unsigned w = argc > 1 ? (unsigned)atoi(argv[1]) : 1920;
    const unsigned h = argc > 2 ? (unsigned)atoi(argv[2]) : 1080;
    int major, minor, fd;
    VADisplay dpy;
    VAStatus st;
    VASurfaceID surf;
    VASurfaceAttrib attr;
    VADRMPRIMESurfaceDescriptor d;

    fd = open("/dev/dri/renderD128", O_RDWR);
    if (fd < 0) { perror("renderD128"); return 1; }
    dpy = vaGetDisplayDRM(fd);
    st = vaInitialize(dpy, &major, &minor);
    if (st != VA_STATUS_SUCCESS) die("vaInitialize", st);
    printf("VA-API %d.%d, driver: %s\n", major, minor, vaQueryVendorString(dpy));

    attr.type = VASurfaceAttribPixelFormat;
    attr.flags = VA_SURFACE_ATTRIB_SETTABLE;
    attr.value.type = VAGenericValueTypeInteger;
    attr.value.value.i = VA_FOURCC_NV12;

    st = vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, w, h, &surf, 1, &attr, 1);
    if (st != VA_STATUS_SUCCESS) die("vaCreateSurfaces", st);

    memset(&d, 0, sizeof(d));
    st = vaExportSurfaceHandle(dpy, surf, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                               VA_EXPORT_SURFACE_READ_WRITE, &d);
    if (st != VA_STATUS_SUCCESS) die("vaExportSurfaceHandle", st);

    printf("surface %ux%u fourcc %.4s: %u object(s), %u layer(s)\n",
           d.width, d.height, (const char *)&d.fourcc, d.num_objects, d.num_layers);

    for (unsigned i = 0; i < d.num_objects; i++) {
        off_t real = lseek(d.objects[i].fd, 0, SEEK_END);
        printf("  object %u: fd %d  declared size %u  ACTUAL dmabuf size %lld  modifier 0x%llx\n",
               i, d.objects[i].fd, d.objects[i].size, (long long)real,
               (unsigned long long)d.objects[i].drm_format_modifier);
    }

    unsigned long needed = 0;
    for (unsigned l = 0; l < d.num_layers; l++) {
        for (unsigned p = 0; p < d.layers[l].num_planes; p++) {
            /* The last byte this plane addresses. NV12's chroma plane is half height. */
            unsigned rows = (l == 0 && d.num_layers > 1) ? d.height : d.height;
            unsigned long end = d.layers[l].offset[p] + (unsigned long)d.layers[l].pitch[p] * rows;
            printf("  layer %u plane %u: offset %u pitch %u  -> ends at %lu\n",
                   l, p, d.layers[l].offset[p], d.layers[l].pitch[p], end);
            if (end > needed) needed = end;
        }
    }

    /* What a consumer copying NV12 out of this actually touches. */
    unsigned long nv12 = (unsigned long)d.layers[0].pitch[0] * d.height * 3 / 2;
    printf("\n  highest byte addressed by the layout: %lu\n", needed);
    printf("  a full NV12 frame at pitch %u:          %lu\n", d.layers[0].pitch[0], nv12);
    for (unsigned i = 0; i < d.num_objects; i++) {
        off_t real = lseek(d.objects[i].fd, 0, SEEK_END);
        if (real >= 0 && (unsigned long)real < needed)
            printf("  >>> object %u IS SHORT: %lld bytes backing a %lu-byte layout\n",
                   i, (long long)real, needed);
        if (real >= 0 && (unsigned long)real < nv12)
            printf("  >>> object %u cannot hold a full NV12 frame (%lld < %lu)\n",
                   i, (long long)real, nv12);
    }
    return 0;
}
