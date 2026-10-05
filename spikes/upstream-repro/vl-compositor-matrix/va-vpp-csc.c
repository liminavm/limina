/*
 * VA-API video processing on a driver that uses the gfx vl_compositor
 * (virgl, nouveau, r600, ...) converts RGB -> YUV with the wrong matrix.
 *
 * Fills a BGRA surface with a solid colour, runs one VAProcPipeline pass into
 * an NV12 (or BGRA) surface, reads the result back with vaGetImage and compares
 * it with the BT.709 limited-range values the request asked for.
 *
 * Build: cc -o va-vpp-csc va-vpp-csc.c -lva -lva-drm -lm
 * Run:   ./va-vpp-csc [nv12|bgra] [/dev/dri/renderD128]
 * Unfixed (nv12): Y/U/V far from expected for saturated colours, exit 1.
 * Fixed: every colour within +-2 of expected, exit 0.
 */
#include <fcntl.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_vpp.h>

#define W 64
#define H 64

#define CHECK(x)                                                         \
   do {                                                                  \
      VAStatus st_ = (x);                                                \
      if (st_ != VA_STATUS_SUCCESS) {                                    \
         fprintf(stderr, "%s:%d %s: %s\n", __FILE__, __LINE__, #x,       \
                 vaErrorStr(st_));                                       \
         exit(2);                                                        \
      }                                                                  \
   } while (0)

static VASurfaceID
make_surface(VADisplay dpy, unsigned rt, unsigned fourcc)
{
   VASurfaceAttrib attr = {
      .type = VASurfaceAttribPixelFormat,
      .flags = VA_SURFACE_ATTRIB_SETTABLE,
      .value = { .type = VAGenericValueTypeInteger, .value.i = fourcc },
   };
   VASurfaceID s;
   CHECK(vaCreateSurfaces(dpy, rt, W, H, &s, 1, &attr, 1));
   return s;
}

static VAImageFormat
image_format(VADisplay dpy, unsigned fourcc)
{
   int n = vaMaxNumImageFormats(dpy);
   VAImageFormat *f = calloc(n, sizeof(*f));
   CHECK(vaQueryImageFormats(dpy, f, &n));
   for (int i = 0; i < n; i++) {
      if (f[i].fourcc == fourcc) {
         VAImageFormat r = f[i];
         free(f);
         return r;
      }
   }
   fprintf(stderr, "image format %.4s not supported\n", (char *)&fourcc);
   exit(2);
}

/* BT.709, limited range, 8 bit. */
static void
expect_yuv(int r, int g, int b, int out[3])
{
   double R = r / 255.0, G = g / 255.0, B = b / 255.0;
   double Y = 0.2126 * R + 0.7152 * G + 0.0722 * B;
   double Cb = (B - Y) / 1.8556, Cr = (R - Y) / 1.5748;
   out[0] = (int)lround(16 + 219 * Y);
   out[1] = (int)lround(128 + 224 * Cb);
   out[2] = (int)lround(128 + 224 * Cr);
}

int
main(int argc, char **argv)
{
   int to_nv12 = !(argc > 1 && strcmp(argv[1], "bgra") == 0);
   const char *node = argc > 2 ? argv[2] : "/dev/dri/renderD128";
   static const struct { const char *name; int r, g, b; } colours[] = {
      { "red", 255, 0, 0 },     { "green", 0, 255, 0 }, { "blue", 0, 0, 255 },
      { "white", 255, 255, 255 }, { "grey", 128, 128, 128 },
      { "orange", 255, 128, 0 },
   };

   int fd = open(node, O_RDWR);
   if (fd < 0) {
      perror(node);
      return 2;
   }
   VADisplay dpy = vaGetDisplayDRM(fd);
   int major, minor;
   CHECK(vaInitialize(dpy, &major, &minor));
   printf("driver: %s\n", vaQueryVendorString(dpy));
   printf("conversion: BGRA -> %s\n", to_nv12 ? "NV12 (BT.709 limited)" : "BGRA");

   VAConfigID cfg;
   CHECK(vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &cfg));

   VASurfaceID src = make_surface(dpy, VA_RT_FORMAT_RGB32, VA_FOURCC_BGRA);
   VASurfaceID dst = to_nv12 ? make_surface(dpy, VA_RT_FORMAT_YUV420, VA_FOURCC_NV12)
                             : make_surface(dpy, VA_RT_FORMAT_RGB32, VA_FOURCC_BGRA);
   VAContextID ctx;
   CHECK(vaCreateContext(dpy, cfg, W, H, VA_PROGRESSIVE, &dst, 1, &ctx));

   VAImageFormat out_fmt = image_format(dpy, to_nv12 ? VA_FOURCC_NV12 : VA_FOURCC_BGRA);
   VAImage out_img;
   CHECK(vaCreateImage(dpy, &out_fmt, W, H, &out_img));

   int bad = 0;
   for (unsigned c = 0; c < sizeof(colours) / sizeof(colours[0]); c++) {
      /* Fill the source through a derived image, i.e. write the surface directly. */
      uint8_t *p;
      VAImage in_img;
      CHECK(vaDeriveImage(dpy, src, &in_img));
      CHECK(vaMapBuffer(dpy, in_img.buf, (void **)&p));
      for (int y = 0; y < H; y++) {
         uint8_t *row = p + in_img.offsets[0] + y * in_img.pitches[0];
         for (int x = 0; x < W; x++) {
            row[4 * x + 0] = colours[c].b;
            row[4 * x + 1] = colours[c].g;
            row[4 * x + 2] = colours[c].r;
            row[4 * x + 3] = 255;
         }
      }
      CHECK(vaUnmapBuffer(dpy, in_img.buf));
      vaDestroyImage(dpy, in_img.image_id);

      VAProcPipelineParameterBuffer pp = {0};
      pp.surface = src;
      pp.surface_color_standard = VAProcColorStandardBT709;
      pp.output_color_standard = VAProcColorStandardBT709;
      pp.input_color_properties.color_range = VA_SOURCE_RANGE_FULL;
      pp.output_color_properties.color_range =
         to_nv12 ? VA_SOURCE_RANGE_REDUCED : VA_SOURCE_RANGE_FULL;
      VABufferID pbuf;
      CHECK(vaCreateBuffer(dpy, ctx, VAProcPipelineParameterBufferType, sizeof(pp), 1, &pp,
                           &pbuf));
      CHECK(vaBeginPicture(dpy, ctx, dst));
      CHECK(vaRenderPicture(dpy, ctx, &pbuf, 1));
      CHECK(vaEndPicture(dpy, ctx));
      CHECK(vaSyncSurface(dpy, dst));
      vaDestroyBuffer(dpy, pbuf);

      CHECK(vaGetImage(dpy, dst, 0, 0, W, H, out_img.image_id));
      CHECK(vaMapBuffer(dpy, out_img.buf, (void **)&p));
      int got[3], want[3];
      if (to_nv12) {
         got[0] = p[out_img.offsets[0] + (H / 2) * out_img.pitches[0] + W / 2];
         uint8_t *uv = p + out_img.offsets[1] + (H / 4) * out_img.pitches[1] + W / 2;
         got[1] = uv[0];
         got[2] = uv[1];
         expect_yuv(colours[c].r, colours[c].g, colours[c].b, want);
         printf("%-6s rgb(%3d,%3d,%3d) -> YUV got (%3d,%3d,%3d) want (%3d,%3d,%3d)",
                colours[c].name, colours[c].r, colours[c].g, colours[c].b, got[0], got[1],
                got[2], want[0], want[1], want[2]);
      } else {
         uint8_t *px = p + out_img.offsets[0] + (H / 2) * out_img.pitches[0] + 4 * (W / 2);
         got[0] = px[2];
         got[1] = px[1];
         got[2] = px[0];
         want[0] = colours[c].r;
         want[1] = colours[c].g;
         want[2] = colours[c].b;
         printf("%-6s rgb(%3d,%3d,%3d) -> RGB got (%3d,%3d,%3d)", colours[c].name,
                colours[c].r, colours[c].g, colours[c].b, got[0], got[1], got[2]);
      }
      CHECK(vaUnmapBuffer(dpy, out_img.buf));
      int ok = 1;
      for (int i = 0; i < 3; i++)
         if (abs(got[i] - want[i]) > 2)
            ok = 0;
      printf("  %s\n", ok ? "ok" : "WRONG");
      bad |= !ok;
   }

   vaDestroyImage(dpy, out_img.image_id);
   vaDestroyContext(dpy, ctx);
   vaDestroySurfaces(dpy, &src, 1);
   vaDestroySurfaces(dpy, &dst, 1);
   vaDestroyConfig(dpy, cfg);
   vaTerminate(dpy);
   close(fd);
   printf("%s\n", bad ? "FAIL" : "OK");
   return bad;
}
