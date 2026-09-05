// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* A host-side stand-in for the WebGL page that loses the Vulkan device.
 *
 * The guest vehicle costs a two-minute boot and then 60-120 s of waiting, which
 * is why this bug has been chased in single-arm-per-hour increments. zink and
 * KosmicKrisp are the same code on both sides of the VM boundary — the guest
 * runs zink-on-venus and the host runs zink-on-KK, and both end in the same
 * KosmicKrisp — so if the loss is in that pair it should be reachable here, with
 * no VM, no browser and no venus, in seconds per iteration.
 *
 * What the page does, that this reproduces:
 *   - a MULTISAMPLED default-framebuffer analogue via
 *     EXT_multisampled_render_to_texture. That specific extension is the point:
 *     KosmicKrisp implements no VK_EXT_multisampled_render_to_single_sampled, so
 *     zink emulates it with a hidden MSAA "transient" image and a replicate blit
 *     (zink_render_attachment_shadow). A plain MSAA renderbuffer takes a
 *     different path and is NOT this bug.
 *   - a TEXTURED draw into it every frame, so the pass samples as well as
 *     renders. The page draws checker-textured cubes.
 *   - a composite pass that samples the resolved texture, which is what the
 *     browser's compositor does with the canvas.
 *   - all of it at the size that matters. The loss is display-size dependent:
 *     2560x1440 kills the VM in 60-120 s and 1280x800 survives past four
 *     minutes, so a probe at a toy size proves nothing. WIDTHxHEIGHT default to
 *     2560x1440 for that reason; --size overrides.
 *
 *   - optionally, reallocation churn (--churn N: rebuild the multisampled
 *     canvas, its depth buffer and its framebuffer every N frames). A browser
 *     reallocates a canvas constantly and the guest run's descriptor log is full
 *     of freshly created image views; a loop that allocates once at startup and
 *     then only draws is not the same workload, however many frames it runs.
 *
 * Exit status: 0 = survived the run (no reproduction), 1 = a GL error or a
 * context reset was observed, 77 = cannot test here (no EGL/GLES, or the driver
 * lacks EXT_multisampled_render_to_texture).
 *
 * Host build+run: ./run-host-loop.sh [--seconds N] [--size WxH] [--samples N]
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define SKIP 77

typedef void(GL_APIENTRY *FBTEX2DMS)(GLenum, GLenum, GLenum, GLuint, GLint,
                                     GLsizei);
typedef void(GL_APIENTRY *RBSTORAGEMS)(GLenum, GLsizei, GLenum, GLsizei,
                                       GLsizei);
typedef GLenum(GL_APIENTRY *GETRESETSTATUS)(void);

static const char *vs_src =
    "attribute vec2 pos;\n"
    "attribute vec2 uv;\n"
    "uniform vec2 off;\n"
    "varying vec2 v;\n"
    "void main() { v = uv; gl_Position = vec4(pos + off, 0.0, 1.0); }\n";

static const char *fs_src =
    "precision mediump float;\n"
    "uniform sampler2D t;\n"
    "varying vec2 v;\n"
    "void main() { gl_FragColor = texture2D(t, v); }\n";

static double now_s(void) {
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec + ts.tv_nsec / 1e9;
}

static GLuint compile(GLenum stage, const char *src) {
   GLuint s = glCreateShader(stage);
   glShaderSource(s, 1, &src, NULL);
   glCompileShader(s);
   GLint ok = 0;
   glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
   if (!ok) {
      char log[1024] = {0};
      glGetShaderInfoLog(s, sizeof(log) - 1, NULL, log);
      fprintf(stderr, "shader compile failed: %s\n", log);
      exit(1);
   }
   return s;
}

/* A checkerboard, so a wrong sample is visible rather than plausible. */
static GLuint checker_texture(void) {
   enum { N = 256 };
   static unsigned char px[N * N * 4];
   for (int y = 0; y < N; y++) {
      for (int x = 0; x < N; x++) {
         bool on = ((x >> 4) ^ (y >> 4)) & 1;
         unsigned char *p = px + 4 * (y * N + x);
         p[0] = on ? 255 : 40;
         p[1] = on ? 110 : 60;
         p[2] = on ? 20 : 220;
         p[3] = 255;
      }
   }
   GLuint t;
   glGenTextures(1, &t);
   glBindTexture(GL_TEXTURE_2D, t);
   glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, N, N, 0, GL_RGBA, GL_UNSIGNED_BYTE,
                px);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
   return t;
}

/* The canvas: a normal 2D texture rendered into with an implicit MSAA
 * attachment, which is exactly what a WebGL antialias:true backbuffer is. Kept
 * as a function because --churn rebuilds it mid-run. */
static void make_canvas(FBTEX2DMS fbtex2dms, RBSTORAGEMS rbstorage_ms, int width,
                        int height, int samples, GLuint *canvas, GLuint *depth,
                        GLuint *fb) {
   glGenTextures(1, canvas);
   glBindTexture(GL_TEXTURE_2D, *canvas);
   glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, width, height, 0, GL_RGBA,
                GL_UNSIGNED_BYTE, NULL);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

   glGenRenderbuffers(1, depth);
   glBindRenderbuffer(GL_RENDERBUFFER, *depth);
   rbstorage_ms(GL_RENDERBUFFER, samples, GL_DEPTH_COMPONENT16, width, height);

   glGenFramebuffers(1, fb);
   glBindFramebuffer(GL_FRAMEBUFFER, *fb);
   fbtex2dms(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, *canvas, 0,
             samples);
   glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT,
                             GL_RENDERBUFFER, *depth);
}

int main(int argc, char **argv) {
   int width = 2560, height = 1440, samples = 4, seconds = 180, churn = 0;

   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "--seconds") && i + 1 < argc)
         seconds = atoi(argv[++i]);
      else if (!strcmp(argv[i], "--samples") && i + 1 < argc)
         samples = atoi(argv[++i]);
      else if (!strcmp(argv[i], "--size") && i + 1 < argc)
         sscanf(argv[++i], "%dx%d", &width, &height);
      else if (!strcmp(argv[i], "--churn") && i + 1 < argc)
         churn = atoi(argv[++i]);
      else {
         fprintf(stderr,
                 "usage: %s [--seconds N] [--size WxH] [--samples N] "
                 "[--churn FRAMES]\n",
                 argv[0]);
         return 2;
      }
   }

   EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
   if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
      fprintf(stderr, "SKIP: no EGL display (need EGL_PLATFORM=surfaceless)\n");
      return SKIP;
   }
   eglBindAPI(EGL_OPENGL_ES_API);

   EGLint cfg_attrs[] = {EGL_SURFACE_TYPE,    EGL_PBUFFER_BIT,
                         EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                         EGL_RED_SIZE,        8,
                         EGL_GREEN_SIZE,      8,
                         EGL_BLUE_SIZE,       8,
                         EGL_ALPHA_SIZE,      8,
                         EGL_NONE};
   EGLConfig cfg;
   EGLint n = 0;
   if (!eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n) || n < 1) {
      fprintf(stderr, "SKIP: no usable EGLConfig\n");
      return SKIP;
   }
   EGLint ctx_attrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
   EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
   if (ctx == EGL_NO_CONTEXT) {
      fprintf(stderr, "SKIP: eglCreateContext failed\n");
      return SKIP;
   }
   if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
      fprintf(stderr, "SKIP: surfaceless eglMakeCurrent failed\n");
      return SKIP;
   }

   printf("GL_RENDERER: %s\n", (const char *)glGetString(GL_RENDERER));
   printf("target: %dx%d, %d samples, %d s, churn every %d frames\n", width,
          height, samples, seconds, churn);

   const char *exts = (const char *)glGetString(GL_EXTENSIONS);
   if (!exts || !strstr(exts, "GL_EXT_multisampled_render_to_texture")) {
      fprintf(stderr, "SKIP: no GL_EXT_multisampled_render_to_texture — this "
                      "driver cannot take the shadow-attachment path\n");
      return SKIP;
   }
   FBTEX2DMS fbtex2dms =
       (FBTEX2DMS)eglGetProcAddress("glFramebufferTexture2DMultisampleEXT");
   RBSTORAGEMS rbstorage_ms =
       (RBSTORAGEMS)eglGetProcAddress("glRenderbufferStorageMultisampleEXT");
   if (!fbtex2dms || !rbstorage_ms) {
      fprintf(stderr, "SKIP: MSRTT entry points missing\n");
      return SKIP;
   }
   /* Optional: a robustness context reports a reset instead of only faulting. */
   GETRESETSTATUS get_reset =
       (GETRESETSTATUS)eglGetProcAddress("glGetGraphicsResetStatusEXT");

   GLuint canvas = 0, depth = 0, fb = 0;
   make_canvas(fbtex2dms, rbstorage_ms, width, height, samples, &canvas, &depth,
               &fb);
   GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
   if (st != GL_FRAMEBUFFER_COMPLETE) {
      fprintf(stderr, "SKIP: MSRTT framebuffer incomplete: 0x%x\n", st);
      return SKIP;
   }

   /* Ask what was GRANTED, never what was requested. A stack that quietly
    * declines the multisampled attachment reports a complete framebuffer and
    * renders at full speed, and every "survived" from such a run is a statement
    * about a workload that never took the path under test. */
   GLint got_samples = 0, got_sample_buffers = 0;
   glGetIntegerv(GL_SAMPLES, &got_samples);
   glGetIntegerv(GL_SAMPLE_BUFFERS, &got_sample_buffers);
   printf("granted: SAMPLES=%d SAMPLE_BUFFERS=%d\n", got_samples,
          got_sample_buffers);
   if (got_samples < 2 || got_sample_buffers < 1) {
      fprintf(stderr,
              "SKIP: multisampling not granted (SAMPLES=%d SAMPLE_BUFFERS=%d) — "
              "this run would not exercise the shadow-attachment path\n",
              got_samples, got_sample_buffers);
      return SKIP;
   }

   /* The compositor's target: single-sample, sampling the canvas. */
   GLuint composite;
   glGenTextures(1, &composite);
   glBindTexture(GL_TEXTURE_2D, composite);
   glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, width, height, 0, GL_RGBA,
                GL_UNSIGNED_BYTE, NULL);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
   GLuint cfb;
   glGenFramebuffers(1, &cfb);
   glBindFramebuffer(GL_FRAMEBUFFER, cfb);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                          composite, 0);
   if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
      fprintf(stderr, "SKIP: composite framebuffer incomplete\n");
      return SKIP;
   }

   GLuint prog = glCreateProgram();
   glAttachShader(prog, compile(GL_VERTEX_SHADER, vs_src));
   glAttachShader(prog, compile(GL_FRAGMENT_SHADER, fs_src));
   glBindAttribLocation(prog, 0, "pos");
   glBindAttribLocation(prog, 1, "uv");
   glLinkProgram(prog);
   glUseProgram(prog);
   glUniform1i(glGetUniformLocation(prog, "t"), 0);
   GLint u_off = glGetUniformLocation(prog, "off");

   GLuint tex = checker_texture();

   static const GLfloat quad[] = {
       -0.4f, -0.4f, 0.0f, 0.0f, 0.4f, -0.4f, 1.0f, 0.0f,
       -0.4f, 0.4f,  0.0f, 1.0f, 0.4f, 0.4f,  1.0f, 1.0f,
   };
   GLuint vbo;
   glGenBuffers(1, &vbo);
   glBindBuffer(GL_ARRAY_BUFFER, vbo);
   glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);
   glEnableVertexAttribArray(0);
   glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 16, (void *)0);
   glEnableVertexAttribArray(1);
   glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 16, (void *)8);
   glEnable(GL_DEPTH_TEST);

   const double t0 = now_s();
   double next_report = t0 + 5.0;
   unsigned long frames = 0;
   int rc = 0;

   while (now_s() - t0 < seconds) {
      /* Pass 1: render into the implicitly-multisampled canvas. The scissored
       * clear is deliberate — a full clear takes zink's "skip the replicate
       * blit" branch and never enters the shadow path at all. */
      glBindFramebuffer(GL_FRAMEBUFFER, fb);
      glViewport(0, 0, width, height);
      glDisable(GL_SCISSOR_TEST);
      glClearColor(0.05f, 0.05f, 0.09f, 1.0f);
      glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
      glEnable(GL_SCISSOR_TEST);
      glScissor(0, 0, width / 3, height / 3);
      glClearColor(0.0f, 0.2f, 0.0f, 1.0f);
      glClear(GL_COLOR_BUFFER_BIT);
      glDisable(GL_SCISSOR_TEST);

      glActiveTexture(GL_TEXTURE0);
      glBindTexture(GL_TEXTURE_2D, tex);
      for (int i = 0; i < 3; i++) {
         float a = (float)((frames + i * 40) % 240) / 240.0f;
         glUniform2f(u_off, -0.5f + i * 0.5f, -0.3f + 0.6f * a);
         glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
      }

      /* Pass 2: composite, sampling the resolved canvas. */
      glBindFramebuffer(GL_FRAMEBUFFER, cfb);
      glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
      glClear(GL_COLOR_BUFFER_BIT);
      glBindTexture(GL_TEXTURE_2D, canvas);
      glUniform2f(u_off, 0.0f, 0.0f);
      glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

      glFlush();
      frames++;

      if (churn > 0 && frames % (unsigned long)churn == 0) {
         glBindFramebuffer(GL_FRAMEBUFFER, 0);
         glDeleteFramebuffers(1, &fb);
         glDeleteRenderbuffers(1, &depth);
         glDeleteTextures(1, &canvas);
         make_canvas(fbtex2dms, rbstorage_ms, width, height, samples, &canvas,
                     &depth, &fb);
      }

      GLenum err = glGetError();
      if (err != GL_NO_ERROR) {
         fprintf(stderr, "REPRO: GL error 0x%x after %lu frames, %.1f s\n", err,
                 frames, now_s() - t0);
         rc = 1;
         break;
      }
      if (get_reset) {
         GLenum reset = get_reset();
         if (reset != GL_NO_ERROR) {
            fprintf(stderr,
                    "REPRO: context reset 0x%x after %lu frames, %.1f s\n",
                    reset, frames, now_s() - t0);
            rc = 1;
            break;
         }
      }

      if (now_s() >= next_report) {
         double el = now_s() - t0;
         printf("  %6.1f s  %8lu frames  %.1f fps\n", el, frames, frames / el);
         fflush(stdout);
         next_report += 5.0;
      }
   }

   double el = now_s() - t0;
   printf("%s after %lu frames in %.1f s (%.1f fps)\n",
          rc ? "REPRODUCED" : "survived", frames, el, frames / el);
   return rc;
}
