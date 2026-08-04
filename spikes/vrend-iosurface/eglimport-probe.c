// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* eglimport-probe — standalone oracle for the EGL_IOSURFACE_LIMINA import chain.
 *
 * Exercises exactly what vrend does when compositing a venus client's buffer:
 *   IOSurfaceCreate → eglCreateImageKHR(EGL_IOSURFACE_LIMINA) →
 *   glEGLImageTargetTexture2DOES → sample it back (FBO blit + glReadPixels)
 * on the surfaceless zink-on-KK EGL stack, without booting a VM.
 *
 * Run under the boot script's host GL env (see run-probe.sh next to this file).
 * PASS = the readback returns the pattern written into the IOSurface via CPU.
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

#define EGL_IOSURFACE_LIMINA 0x3B9A

#define W 64
#define H 64

static const char *
eglerr(void)
{
   static char buf[32];
   snprintf(buf, sizeof(buf), "0x%04x", eglGetError());
   return buf;
}

int
main(void)
{
   /* --- IOSurface with a known pattern ------------------------------- */
   CFMutableDictionaryRef props = CFDictionaryCreateMutable(
      NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
   int32_t w = W, h = H, bpe = 4;
   uint32_t fmt = 'BGRA';
   CFNumberRef nw = CFNumberCreate(NULL, kCFNumberSInt32Type, &w);
   CFNumberRef nh = CFNumberCreate(NULL, kCFNumberSInt32Type, &h);
   CFNumberRef nb = CFNumberCreate(NULL, kCFNumberSInt32Type, &bpe);
   CFNumberRef nf = CFNumberCreate(NULL, kCFNumberSInt32Type, (int32_t *)&fmt);
   CFDictionarySetValue(props, kIOSurfaceWidth, nw);
   CFDictionarySetValue(props, kIOSurfaceHeight, nh);
   CFDictionarySetValue(props, kIOSurfaceBytesPerElement, nb);
   CFDictionarySetValue(props, kIOSurfacePixelFormat, nf);
   IOSurfaceRef surf = IOSurfaceCreate(props);
   if (!surf) {
      printf("FAIL: IOSurfaceCreate\n");
      return 1;
   }
   /* pure red: BGRA bytes = 00 00 FF FF. R/B-asymmetric on purpose — a
    * channel swap anywhere in the chain reads back blue, not red. (v1 used
    * magenta, which is R/B-symmetric and silently passed a swapped chain.) */
   IOSurfaceLock(surf, 0, NULL);
   uint8_t *base = IOSurfaceGetBaseAddress(surf);
   size_t pitch = IOSurfaceGetBytesPerRow(surf);
   for (int y = 0; y < H; y++)
      for (int x = 0; x < W; x++) {
         uint8_t *p = base + y * pitch + x * 4;
         p[0] = 0x00; p[1] = 0x00; p[2] = 0xff; p[3] = 0xff;
      }
   IOSurfaceUnlock(surf, 0, NULL);
   printf("iosurface: %dx%d pitch=%zu id=%u\n", W, H, pitch, IOSurfaceGetID(surf));

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

   /* --- the import ---------------------------------------------------- */
   PFNEGLCREATEIMAGEKHRPROC eglCreateImageKHR =
      (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
   PFNGLEGLIMAGETARGETTEXTURE2DOESPROC glEGLImageTargetTexture2DOES =
      (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress(
         "glEGLImageTargetTexture2DOES");
   if (!eglCreateImageKHR || !glEGLImageTargetTexture2DOES) {
      printf("FAIL: KHR/OES entry points missing\n");
      return 1;
   }
   EGLImageKHR img = eglCreateImageKHR(dpy, EGL_NO_CONTEXT, EGL_IOSURFACE_LIMINA,
                                       (EGLClientBuffer)surf, NULL);
   if (img == EGL_NO_IMAGE_KHR) {
      printf("FAIL: eglCreateImageKHR(EGL_IOSURFACE_LIMINA) (%s)\n", eglerr());
      return 1;
   }
   printf("eglimage: OK\n");

   GLuint tex;
   glGenTextures(1, &tex);
   glBindTexture(GL_TEXTURE_2D, tex);
   glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, (GLeglImageOES)img);
   GLenum err = glGetError();
   if (err != GL_NO_ERROR) {
      printf("FAIL: glEGLImageTargetTexture2DOES gl err 0x%x\n", err);
      return 1;
   }
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
   printf("target-texture: OK\n");

   /* --- prove the pixels: attach as FBO color and read back ----------- */
   GLuint fbo;
   glGenFramebuffers(1, &fbo);
   glBindFramebuffer(GL_FRAMEBUFFER, fbo);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                          tex, 0);
   GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
   if (st != GL_FRAMEBUFFER_COMPLETE) {
      printf("FAIL: fbo status 0x%x\n", st);
      return 1;
   }
   uint8_t px[4] = { 0 };
   glReadPixels(W / 2, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
   err = glGetError();
   printf("readback: rgba %02x %02x %02x %02x (gl err 0x%x)\n", px[0], px[1],
          px[2], px[3], err);
   /* CPU wrote BGRA pure red (B=00 G=00 R=ff A=ff) → RGBA readback ff 00 00 ff */
   int fails = 0;
   if (px[0] == 0xff && px[1] == 0x00 && px[2] == 0x00 && px[3] == 0xff) {
      printf("import-read: PASS (native channel order)\n");
   } else if (px[0] == 0x00 && px[1] == 0x00 && px[2] == 0xff && px[3] == 0xff) {
      printf("import-read: FAIL — R/B SWAPPED readback\n");
      fails++;
   } else {
      printf("import-read: FAIL — wrong pixels\n");
      fails++;
   }

   /* --- phase 2 contract #1: GPU render INTO the texture reaches the ------
    * IOSurface bytes. Clear to pure green (R/B-asymmetric would not catch a
    * swap here — the swap axis is R/B, so use pure BLUE: rgba 00 00 ff ff →
    * IOSurface BGRA bytes ff 00 00 ff). */
   glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
   glClear(GL_COLOR_BUFFER_BIT);
   glFinish();
   IOSurfaceLock(surf, kIOSurfaceLockReadOnly, NULL);
   base = IOSurfaceGetBaseAddress(surf);
   uint8_t *q = base + (H / 2) * pitch + (W / 2) * 4;
   printf("render-into: iosurface bytes B=%02x G=%02x R=%02x A=%02x\n", q[0],
          q[1], q[2], q[3]);
   int render_ok = (q[0] == 0xff && q[1] == 0x00 && q[2] == 0x00 && q[3] == 0xff);
   IOSurfaceUnlock(surf, kIOSurfaceLockReadOnly, NULL);
   if (render_ok) {
      printf("render-into: PASS (GPU clear landed in the IOSurface, native order)\n");
   } else {
      printf("render-into: FAIL\n");
      fails++;
   }

   /* --- phase 2 contract #2: vrend's GLES transfer upload. vrend pre-swizzles
    * guest BGRA data to RGBA byte order and uploads with GL_RGBA/UNSIGNED_BYTE
    * (vrend_renderer.c "manually swizzling bgra->rgba on upload since
    * gles+bgra"). Against our native-BGRA EGLImage texture that must be
    * accepted and converted so the final IOSurface bytes equal the guest's
    * original BGRA. Simulate: guest color = orange (R=ff G=80 B=00), so
    * vrend-swizzled RGBA-order upload bytes are ff 80 00 ff. */
   {
      uint8_t up[4] = { 0xff, 0x80, 0x00, 0xff }; /* R G B A byte order */
      uint8_t *buf = malloc((size_t)W * H * 4);
      for (int i = 0; i < W * H; i++)
         memcpy(buf + i * 4, up, 4);
      glBindTexture(GL_TEXTURE_2D, tex);
      glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE,
                      buf);
      err = glGetError();
      free(buf);
      glFinish();
      IOSurfaceLock(surf, kIOSurfaceLockReadOnly, NULL);
      base = IOSurfaceGetBaseAddress(surf);
      q = base + (H / 2) * pitch + (W / 2) * 4;
      printf("texsubimage: gl err 0x%x, iosurface bytes B=%02x G=%02x R=%02x A=%02x\n",
             err, q[0], q[1], q[2], q[3]);
      /* guest orange in BGRA byte order: 00 80 ff ff */
      int up_ok = (err == GL_NO_ERROR && q[0] == 0x00 && q[1] == 0x80 &&
                   q[2] == 0xff && q[3] == 0xff);
      IOSurfaceUnlock(surf, kIOSurfaceLockReadOnly, NULL);
      if (up_ok) {
         printf("texsubimage: PASS (GL_RGBA upload converted into BGRA storage)\n");
      } else {
         printf("texsubimage: FAIL\n");
         fails++;
      }
   }

   printf(fails ? "FAIL (%d)\n" : "PASS\n", fails);
   return fails ? 1 : 0;
}
