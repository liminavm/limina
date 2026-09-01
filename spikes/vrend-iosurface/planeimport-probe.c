// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* planeimport-probe — oracle for the PER-PLANE EGL_IOSURFACE_LIMINA import.
 *
 * eglimport-probe's sibling. That one proves the whole-surface BGRA import;
 * this one proves the chain blob-backed decode targets need: one biplanar
 * (NV12) IOSurface imported TWICE, luma as R8 and chroma as RG8, each a
 * separate GL texture over the same allocation.
 *
 *   IOSurfaceCreate(biplanar '420f') → CPU-fill both planes →
 *   eglCreateImageKHR(EGL_IOSURFACE_LIMINA, {PLANE, FOURCC}) ×2 →
 *   sample both in one shader → render into a BGRA output IOSurface →
 *   read the output's bytes on the CPU and compare against the fill.
 *
 * The two planes carry deliberately different patterns, so binding the wrong
 * plane, or letting the chain default to plane 0, fails rather than passing on
 * a coincidence. Reading the OUTPUT SURFACE's bytes rather than glReadPixels
 * keeps the verdict on real memory (#28).
 *
 * This is also the host-CPU-write → host-GPU-sample-read coherency arm that
 * spikes/hv-iosurface-map/RESULTS.md books as untested: every expected byte
 * here was written by the CPU and read by the GPU.
 *
 * Run via run-planeprobe.sh, which supplies the host GL env.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>

#include <CoreFoundation/CoreFoundation.h>
#include <IOSurface/IOSurfaceRef.h>

#define EGL_IOSURFACE_LIMINA        0x3B9A
#define EGL_IOSURFACE_PLANE_LIMINA  0x3B9B
#define EGL_IOSURFACE_FOURCC_LIMINA 0x3B9C

#define FOURCC(a, b, c, d) ((uint32_t)(a) | ((uint32_t)(b) << 8) | \
                            ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
#define DRM_FORMAT_R8   FOURCC('R', '8', ' ', ' ')
#define DRM_FORMAT_GR88 FOURCC('G', 'R', '8', '8')

#define W  64
#define H  64
#define CW (W / 2)
#define CH (H / 2)

/* Distinct per plane on purpose: a plane mix-up cannot look like a pass. */
static uint8_t luma_at(int x, int y)  { return (uint8_t)(x * 3 + y * 5 + 17); }
static uint8_t cb_at(int x, int y)    { (void)y; return (uint8_t)(x * 11 + 40); }
static uint8_t cr_at(int x, int y)    { (void)x; return (uint8_t)(y * 9 + 90); }

static int fails;

static const char *
eglerr(void)
{
   static char buf[32];
   snprintf(buf, sizeof(buf), "0x%04x", eglGetError());
   return buf;
}

static void
dict_set_int(CFMutableDictionaryRef d, CFStringRef key, int32_t v)
{
   CFNumberRef n = CFNumberCreate(NULL, kCFNumberSInt32Type, &v);
   CFDictionarySetValue(d, key, n);
   CFRelease(n);
}

static CFMutableDictionaryRef
plane_dict(int32_t w, int32_t h, int32_t bpe, int32_t bpr, int32_t off)
{
   CFMutableDictionaryRef d = CFDictionaryCreateMutable(
      NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
   dict_set_int(d, kIOSurfacePlaneWidth, w);
   dict_set_int(d, kIOSurfacePlaneHeight, h);
   dict_set_int(d, kIOSurfacePlaneBytesPerElement, bpe);
   dict_set_int(d, kIOSurfacePlaneBytesPerRow, bpr);
   dict_set_int(d, kIOSurfacePlaneOffset, off);
   return d;
}

static IOSurfaceRef
make_biplanar(void)
{
   /* Explicit offsets and pitches: slice 2's host allocator must dictate the
    * same layout to the guest, so the probe pins it rather than discovering
    * whatever IOSurface would have picked. */
   const int32_t bpr0 = (W + 63) & ~63;
   const int32_t bpr1 = (W + 63) & ~63; /* CW elements x 2 bytes = W bytes */
   const int32_t off0 = 0;
   const int32_t off1 = bpr0 * H;
   const int32_t total = off1 + bpr1 * CH;

   CFMutableDictionaryRef p0 = plane_dict(W, H, 1, bpr0, off0);
   CFMutableDictionaryRef p1 = plane_dict(CW, CH, 2, bpr1, off1);
   const void *planes[2] = { p0, p1 };
   CFArrayRef arr = CFArrayCreate(NULL, planes, 2, &kCFTypeArrayCallBacks);

   CFMutableDictionaryRef props = CFDictionaryCreateMutable(
      NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
   dict_set_int(props, kIOSurfaceWidth, W);
   dict_set_int(props, kIOSurfaceHeight, H);
   dict_set_int(props, kIOSurfacePixelFormat, (int32_t)FOURCC('4', '2', '0', 'f'));
   dict_set_int(props, kIOSurfaceAllocSize, total);
   CFDictionarySetValue(props, kIOSurfacePlaneInfo, arr);

   IOSurfaceRef s = IOSurfaceCreate(props);
   CFRelease(props);
   CFRelease(arr);
   CFRelease(p0);
   CFRelease(p1);
   return s;
}

static IOSurfaceRef
make_bgra(int w, int h)
{
   CFMutableDictionaryRef props = CFDictionaryCreateMutable(
      NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
   dict_set_int(props, kIOSurfaceWidth, w);
   dict_set_int(props, kIOSurfaceHeight, h);
   dict_set_int(props, kIOSurfaceBytesPerElement, 4);
   dict_set_int(props, kIOSurfacePixelFormat, (int32_t)FOURCC('B', 'G', 'R', 'A'));
   IOSurfaceRef s = IOSurfaceCreate(props);
   CFRelease(props);
   return s;
}

static GLuint
compile(GLenum stage, const char *src)
{
   GLuint s = glCreateShader(stage);
   glShaderSource(s, 1, &src, NULL);
   glCompileShader(s);
   GLint ok = 0;
   glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
   if (!ok) {
      char log[1024] = { 0 };
      glGetShaderInfoLog(s, sizeof(log) - 1, NULL, log);
      printf("FAIL: shader compile: %s\n", log);
      exit(1);
   }
   return s;
}

int
main(void)
{
   PFNEGLCREATEIMAGEKHRPROC eglCreateImageKHR;
   PFNGLEGLIMAGETARGETTEXTURE2DOESPROC glEGLImageTargetTexture2DOES;

   IOSurfaceRef surf = make_biplanar();
   if (!surf) {
      printf("FAIL: IOSurfaceCreate(biplanar)\n");
      return 1;
   }
   size_t nplanes = IOSurfaceGetPlaneCount(surf);
   printf("iosurface: %dx%d '420f' planes=%zu id=%u\n", W, H, nplanes,
          IOSurfaceGetID(surf));
   if (nplanes != 2) {
      printf("FAIL: expected 2 planes\n");
      return 1;
   }

   IOSurfaceLock(surf, 0, NULL);
   {
      uint8_t *y = IOSurfaceGetBaseAddressOfPlane(surf, 0);
      size_t yp = IOSurfaceGetBytesPerRowOfPlane(surf, 0);
      uint8_t *c = IOSurfaceGetBaseAddressOfPlane(surf, 1);
      size_t cp = IOSurfaceGetBytesPerRowOfPlane(surf, 1);
      printf("planes: luma %ux%u pitch %zu / chroma %ux%u pitch %zu\n", W, H, yp,
             CW, CH, cp);
      for (int j = 0; j < H; j++)
         for (int i = 0; i < W; i++)
            y[j * yp + i] = luma_at(i, j);
      for (int j = 0; j < CH; j++)
         for (int i = 0; i < CW; i++) {
            c[j * cp + i * 2 + 0] = cb_at(i, j);
            c[j * cp + i * 2 + 1] = cr_at(i, j);
         }
   }
   IOSurfaceUnlock(surf, 0, NULL);

   IOSurfaceRef out = make_bgra(CW, CH);
   if (!out) {
      printf("FAIL: IOSurfaceCreate(out)\n");
      return 1;
   }

   /* --- surfaceless EGL + GLES context ------------------------------- */
   EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
   if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
      printf("FAIL: eglInitialize (%s)\n", eglerr());
      return 1;
   }
   eglBindAPI(EGL_OPENGL_ES_API);
   static const EGLint cfg_attrs[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                                       EGL_NONE };
   EGLConfig cfg;
   EGLint ncfg = 0;
   eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &ncfg);
   static const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
   EGLContext ctx =
      eglCreateContext(dpy, ncfg ? cfg : NULL, EGL_NO_CONTEXT, ctx_attrs);
   if (ctx == EGL_NO_CONTEXT) {
      printf("FAIL: eglCreateContext (%s)\n", eglerr());
      return 1;
   }
   eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
   printf("gl: %s / %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION));

   eglCreateImageKHR =
      (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
   glEGLImageTargetTexture2DOES =
      (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress(
         "glEGLImageTargetTexture2DOES");
   if (!eglCreateImageKHR || !glEGLImageTargetTexture2DOES) {
      printf("FAIL: KHR/OES entry points missing\n");
      return 1;
   }

   /* --- the two per-plane imports ------------------------------------- */
   GLuint tex[2];
   glGenTextures(2, tex);
   const uint32_t fourcc[2] = { DRM_FORMAT_R8, DRM_FORMAT_GR88 };
   const char *pname[2] = { "luma R8", "chroma GR88" };
   for (int p = 0; p < 2; p++) {
      const EGLint attrs[] = { EGL_IOSURFACE_PLANE_LIMINA, p,
                               EGL_IOSURFACE_FOURCC_LIMINA, (EGLint)fourcc[p],
                               EGL_NONE };
      EGLImageKHR img = eglCreateImageKHR(dpy, EGL_NO_CONTEXT,
                                          EGL_IOSURFACE_LIMINA,
                                          (EGLClientBuffer)surf, attrs);
      if (img == EGL_NO_IMAGE_KHR) {
         printf("FAIL: eglCreateImageKHR plane %d (%s) (%s)\n", p, pname[p],
                eglerr());
         return 1;
      }
      glBindTexture(GL_TEXTURE_2D, tex[p]);
      glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, (GLeglImageOES)img);
      GLenum err = glGetError();
      if (err != GL_NO_ERROR) {
         printf("FAIL: TargetTexture2D plane %d (%s) gl err 0x%x\n", p, pname[p],
                err);
         return 1;
      }
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
      printf("import: plane %d (%s) OK\n", p, pname[p]);
   }

   /* --- output FBO over the BGRA surface (the proven whole-surface door) */
   EGLImageKHR oimg = eglCreateImageKHR(dpy, EGL_NO_CONTEXT,
                                        EGL_IOSURFACE_LIMINA,
                                        (EGLClientBuffer)out, NULL);
   if (oimg == EGL_NO_IMAGE_KHR) {
      printf("FAIL: eglCreateImageKHR(out) (%s)\n", eglerr());
      return 1;
   }
   GLuint otex;
   glGenTextures(1, &otex);
   glBindTexture(GL_TEXTURE_2D, otex);
   glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, (GLeglImageOES)oimg);
   GLuint fbo;
   glGenFramebuffers(1, &fbo);
   glBindFramebuffer(GL_FRAMEBUFFER, fbo);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                          otex, 0);
   GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
   if (st != GL_FRAMEBUFFER_COMPLETE) {
      printf("FAIL: fbo status 0x%x\n", st);
      return 1;
   }

   /* --- sample both planes, raw (no colour conversion: the bytes ARE the
    * verdict). Chroma is 1:1 with the output; luma is sampled at the exact
    * even texel, so NEAREST makes every expected value exact. */
   static const char *vs =
      "attribute vec2 pos;\n"
      "varying vec2 uv;\n"
      "void main() {\n"
      "  uv = pos * 0.5 + 0.5;\n"
      "  gl_Position = vec4(pos, 0.0, 1.0);\n"
      "}\n";
   static const char *fs =
      "precision highp float;\n"
      "varying vec2 uv;\n"
      "uniform sampler2D luma;\n"
      "uniform sampler2D chroma;\n"
      "void main() {\n"
      "  vec2 c = vec2(floor(uv.x * 32.0), floor(uv.y * 32.0));\n"
      "  vec2 cuv = (c + 0.5) / 32.0;\n"
      "  vec2 luv = (c * 2.0 + 0.5) / 64.0;\n"
      "  float y  = texture2D(luma, luv).r;\n"
      "  vec2  cc = texture2D(chroma, cuv).rg;\n"
      "  gl_FragColor = vec4(y, cc.r, cc.g, 1.0);\n"
      "}\n";
   GLuint prog = glCreateProgram();
   glAttachShader(prog, compile(GL_VERTEX_SHADER, vs));
   glAttachShader(prog, compile(GL_FRAGMENT_SHADER, fs));
   glBindAttribLocation(prog, 0, "pos");
   glLinkProgram(prog);
   GLint linked = 0;
   glGetProgramiv(prog, GL_LINK_STATUS, &linked);
   if (!linked) {
      char log[1024] = { 0 };
      glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
      printf("FAIL: link: %s\n", log);
      return 1;
   }
   glUseProgram(prog);
   glActiveTexture(GL_TEXTURE0);
   glBindTexture(GL_TEXTURE_2D, tex[0]);
   glUniform1i(glGetUniformLocation(prog, "luma"), 0);
   glActiveTexture(GL_TEXTURE1);
   glBindTexture(GL_TEXTURE_2D, tex[1]);
   glUniform1i(glGetUniformLocation(prog, "chroma"), 1);

   static const GLfloat quad[] = { -1, -1, 3, -1, -1, 3 };
   GLuint vbo;
   glGenBuffers(1, &vbo);
   glBindBuffer(GL_ARRAY_BUFFER, vbo);
   glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);
   glEnableVertexAttribArray(0);
   glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

   glViewport(0, 0, CW, CH);
   glClearColor(0, 0, 0, 1);
   glClear(GL_COLOR_BUFFER_BIT);
   glDrawArrays(GL_TRIANGLES, 0, 3);
   GLenum err = glGetError();
   glFinish();
   if (err != GL_NO_ERROR) {
      printf("FAIL: draw gl err 0x%x\n", err);
      return 1;
   }

   /* --- the verdict, on the output surface's real bytes ---------------- */
   IOSurfaceLock(out, kIOSurfaceLockReadOnly, NULL);
   {
      const uint8_t *base = IOSurfaceGetBaseAddress(out);
      size_t pitch = IOSurfaceGetBytesPerRow(out);
      int bad_y = 0, bad_cb = 0, bad_cr = 0, first = 1;
      for (int j = 0; j < CH; j++) {
         for (int i = 0; i < CW; i++) {
            /* BGRA storage: B=cr, G=cb, R=y (we wrote rgba = y, cb, cr, 1) */
            const uint8_t *p = base + j * pitch + i * 4;
            uint8_t got_y = p[2], got_cb = p[1], got_cr = p[0];
            uint8_t exp_y = luma_at(i * 2, j * 2);
            uint8_t exp_cb = cb_at(i, j), exp_cr = cr_at(i, j);
            if (got_y != exp_y || got_cb != exp_cb || got_cr != exp_cr) {
               if (first) {
                  printf("first mismatch at (%d,%d): "
                         "y %02x/%02x cb %02x/%02x cr %02x/%02x (got/exp)\n",
                         i, j, got_y, exp_y, got_cb, exp_cb, got_cr, exp_cr);
                  first = 0;
               }
               bad_y += got_y != exp_y;
               bad_cb += got_cb != exp_cb;
               bad_cr += got_cr != exp_cr;
            }
         }
      }
      printf("compare: %d texels, mismatches y=%d cb=%d cr=%d\n", CW * CH, bad_y,
             bad_cb, bad_cr);
      if (bad_y || bad_cb || bad_cr)
         fails++;
      else
         printf("plane-import: PASS (both planes sampled exactly, "
                "CPU-written bytes read by the GPU)\n");
   }
   IOSurfaceUnlock(out, kIOSurfaceLockReadOnly, NULL);

   /* --- the guard: no attribs must still mean the whole surface. A biplanar
    * surface has no single self-describing format, so this must be REFUSED,
    * not silently imported as plane 0. */
   {
      EGLImageKHR bad = eglCreateImageKHR(dpy, EGL_NO_CONTEXT,
                                          EGL_IOSURFACE_LIMINA,
                                          (EGLClientBuffer)surf, NULL);
      if (bad == EGL_NO_IMAGE_KHR) {
         printf("attribless-planar: PASS (refused, as it must be)\n");
      } else {
         printf("attribless-planar: FAIL — imported a '420f' surface with no "
                "plane named\n");
         fails++;
      }
   }

   printf(fails ? "FAIL (%d)\n" : "PASS\n", fails);
   return fails ? 1 : 0;
}
