// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* pbotest — minimal oracle for the zink PBO texture-upload staleness
 * (found by crossmark -S upload on host zink-on-KK, 2026-07-28).
 *
 * Each frame: fill a deterministic pattern, upload it into a 64x64 RGBA
 * texture, draw the texture 1:1 into a 64x64 FBO, read the FBO back, and
 * compare against the expected pattern. NEAREST 1:1 makes the readback a
 * faithful proxy for the texel content, and per-frame verification shows
 * exactly WHEN content goes stale and WHAT it holds instead (previous
 * frame? first frame? constant?).
 *
 * Variants (env):
 *   PT_NO_PBO=1      client-pointer glTexSubImage2D (no PBO)
 *   PT_SUBDATA=1     glBufferSubData into a pre-sized PBO (no orphan)
 *   PT_NEW_TEX=1     new texture object every frame (isolates texture reuse)
 *   PT_NEW_PBO=1     new PBO object every frame (isolates buffer reuse)
 *   PT_NO_DRAW=1     upload every frame, draw+verify only the last frame
 *   PT_CHECK_BUF=1   after each upload, GPU-copy the PBO to a scratch buffer
 *                    and map-read it back — does the GPU see the CPU write?
 *   PT_FRAMES=N      frame count (default 4)
 */

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <string.h>

#define DIM 64
#define BYTES (DIM * DIM * 4)

static const char *vs_src =
   "#version 300 es\n"
   "out vec2 uv;\n"
   "void main() {\n"
   "   vec2 c[6] = vec2[6](vec2(0.,0.), vec2(1.,0.), vec2(1.,1.),\n"
   "                       vec2(0.,0.), vec2(1.,1.), vec2(0.,1.));\n"
   "   uv = c[gl_VertexID];\n"
   "   gl_Position = vec4(c[gl_VertexID] * 2.0 - 1.0, 0.0, 1.0);\n"
   "}\n";
static const char *fs_src =
   "#version 300 es\n"
   "precision highp float;\n"
   "uniform sampler2D t;\n"
   "in vec2 uv;\n"
   "out vec4 o;\n"
   "void main() { o = texture(t, uv); }\n";

static GLuint
compile(GLenum type, const char *src)
{
   GLuint sh = glCreateShader(type);
   glShaderSource(sh, 1, &src, NULL);
   glCompileShader(sh);
   GLint ok;
   glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
   if (!ok)
      exit(2);
   return sh;
}

static void
fill(uint8_t *p, int frame)
{
   uint32_t *px = (uint32_t *)p;
   for (int i = 0; i < DIM * DIM; i++)
      px[i] = 0x9e3779b9u * (uint32_t)(frame + 1) + (uint32_t)i * 2654435761u;
}

static GLuint
make_tex(void)
{
   GLuint t;
   glGenTextures(1, &t);
   glBindTexture(GL_TEXTURE_2D, t);
   glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, DIM, DIM);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
   return t;
}

int
main(void)
{
   int frames = getenv("PT_FRAMES") ? atoi(getenv("PT_FRAMES")) : 4;
   int no_pbo = getenv("PT_NO_PBO") != NULL;
   int subdata = getenv("PT_SUBDATA") != NULL;
   int new_tex = getenv("PT_NEW_TEX") != NULL;
   int new_pbo = getenv("PT_NEW_PBO") != NULL;
   int no_draw = getenv("PT_NO_DRAW") != NULL;

   PFNEGLGETPLATFORMDISPLAYEXTPROC gpd =
      (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
   EGLDisplay dpy = gpd ? gpd(0x31DD /* SURFACELESS_MESA */, NULL, NULL)
                        : eglGetDisplay(EGL_DEFAULT_DISPLAY);
   if (!eglInitialize(dpy, NULL, NULL))
      return 2;
   eglBindAPI(EGL_OPENGL_ES_API);
   const EGLint cfg_attrs[] = { EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT,
                                EGL_SURFACE_TYPE, 0, EGL_NONE };
   EGLConfig cfg;
   EGLint ncfg = 0;
   eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &ncfg);
   const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE };
   EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
   eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
   fprintf(stderr, "renderer: %s  variant: %s%s%s%s%s\n",
           glGetString(GL_RENDERER), no_pbo ? "no-pbo " : "pbo ",
           subdata ? "subdata " : "", new_tex ? "new-tex " : "",
           new_pbo ? "new-pbo " : "", no_draw ? "no-draw " : "");

   GLuint prog = glCreateProgram();
   glAttachShader(prog, compile(GL_VERTEX_SHADER, vs_src));
   glAttachShader(prog, compile(GL_FRAGMENT_SHADER, fs_src));
   glLinkProgram(prog);
   glUseProgram(prog);

   GLuint fbt, fbo;
   glGenTextures(1, &fbt);
   glBindTexture(GL_TEXTURE_2D, fbt);
   glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, DIM, DIM);
   glGenFramebuffers(1, &fbo);
   glBindFramebuffer(GL_FRAMEBUFFER, fbo);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, fbt, 0);
   glViewport(0, 0, DIM, DIM);

   GLuint vao;
   glGenVertexArrays(1, &vao);
   glBindVertexArray(vao);

   GLuint tex = make_tex();
   GLuint pbo = 0;
   if (!no_pbo) {
      glGenBuffers(1, &pbo);
      if (subdata) {
         glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
         glBufferData(GL_PIXEL_UNPACK_BUFFER, BYTES, NULL, GL_STREAM_DRAW);
         glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
      }
   }

   uint8_t *src = malloc(BYTES);
   uint8_t *rb = malloc(BYTES);
   uint8_t *expected = malloc(BYTES);
   int failures = 0;

   for (int frame = 0; frame < frames; frame++) {
      fill(src, frame);

      if (new_tex && frame) {
         glDeleteTextures(1, &tex);
         tex = make_tex();
      }
      glBindTexture(GL_TEXTURE_2D, tex);

      /* PT_HALF: frame 0 uploads the full texture, later frames only the
       * bottom half — the untouched top half tells load-op destruction
       * (whole level zeroed) apart from a zero-reading texel fetch. */
      int half = getenv("PT_HALF") != NULL;
      int uh = (half && frame > 0) ? DIM / 2 : DIM;
      if (no_pbo) {
         glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, DIM, uh, GL_RGBA,
                         GL_UNSIGNED_BYTE, src);
      } else {
         if (new_pbo && frame) {
            glDeleteBuffers(1, &pbo);
            glGenBuffers(1, &pbo);
         }
         glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
         if (subdata) {
            glBufferSubData(GL_PIXEL_UNPACK_BUFFER, 0, BYTES, src);
         } else {
            glBufferData(GL_PIXEL_UNPACK_BUFFER, BYTES, src, GL_STREAM_DRAW);
         }
         glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, DIM, uh, GL_RGBA,
                         GL_UNSIGNED_BYTE, (const void *)0);
         glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
      }

      if (getenv("PT_CHECK_BUF") && !no_pbo) {
         static GLuint chk = 0;
         if (!chk) {
            glGenBuffers(1, &chk);
            glBindBuffer(GL_COPY_WRITE_BUFFER, chk);
            glBufferData(GL_COPY_WRITE_BUFFER, BYTES, NULL, GL_STREAM_READ);
         }
         glBindBuffer(GL_COPY_READ_BUFFER, pbo);
         glBindBuffer(GL_COPY_WRITE_BUFFER, chk);
         glCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER, 0, 0, BYTES);
         void *m = glMapBufferRange(GL_COPY_WRITE_BUFFER, 0, BYTES, GL_MAP_READ_BIT);
         printf("frame %d: gpu-copy of PBO %s (first u32 %08x want %08x)\n", frame,
                m && !memcmp(m, src, BYTES) ? "MATCHES cpu write" : "DIFFERS",
                m ? *(uint32_t *)m : 0, *(uint32_t *)src);
         glUnmapBuffer(GL_COPY_WRITE_BUFFER);
         glBindBuffer(GL_COPY_READ_BUFFER, 0);
         glBindBuffer(GL_COPY_WRITE_BUFFER, 0);
      }

      /* PT_FLUSH / PT_FINISH: force a submit boundary between the upload and
       * the sampling draw. PT_DRAW_NOREAD: draw every frame but read back and
       * verify only the last one — splits draw-poisoning from readback-
       * poisoning. */
      if (getenv("PT_FLUSH"))
         glFlush();
      if (getenv("PT_FINISH"))
         glFinish();

      int draw_noread = getenv("PT_DRAW_NOREAD") != NULL;
      int do_draw = !no_draw || frame == frames - 1;
      int verify = ((no_draw || draw_noread) ? frame == frames - 1 : 1);
      if (do_draw) {
         /* Non-zero clear: tells "sampled zeros" (rb=0x00000000) apart from
          * "the quad never rasterized" (rb=clear color). */
         glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
         glClear(GL_COLOR_BUFFER_BIT);
         glDrawArrays(GL_TRIANGLES, 0, 6);
      }
      if (verify) {
         glFinish();
         /* PT_SLEEP_MS: if a hard sleep after glFinish changes the result, the
          * fence signaled before the GPU work landed. */
         if (getenv("PT_SLEEP_MS"))
            usleep(atoi(getenv("PT_SLEEP_MS")) * 1000);
         glReadPixels(0, 0, DIM, DIM, GL_RGBA, GL_UNSIGNED_BYTE, rb);
         if (memcmp(rb, src, BYTES)) {
            failures++;
            /* what IS it? compare against every prior frame's pattern */
            int matched = -1;
            for (int f = 0; f <= frame; f++) {
               fill(expected, f);
               if (!memcmp(rb, expected, BYTES)) {
                  matched = f;
                  break;
               }
            }
            int firstdiff = -1;
            for (int i = 0; i < BYTES; i++)
               if (rb[i] != src[i]) {
                  firstdiff = i;
                  break;
               }
            /* mix analysis: per-pixel, which frame's pattern does it hold? */
            int cur = 0, prev = 0, zero = 0, other = 0;
            uint32_t *rp = (uint32_t *)rb;
            fill(expected, frame > 0 ? frame - 1 : 0);
            uint32_t *pp = (uint32_t *)expected;
            for (int i = 0; i < DIM * DIM; i++) {
               uint32_t want = 0x9e3779b9u * (uint32_t)(frame + 1) +
                               (uint32_t)i * 2654435761u;
               if (rp[i] == want)
                  cur++;
               else if (rp[i] == pp[i])
                  prev++;
               else if (rp[i] == 0)
                  zero++;
               else
                  other++;
            }
            printf("frame %d: STALE (match=%s%d 1stdiff@%d mix: cur=%d prev=%d "
                   "zero=%d other=%d) rb[0..3]=%08x %08x %08x %08x\n",
                   frame, matched >= 0 ? "frame" : "none", matched, firstdiff, cur,
                   prev, zero, other, rp[0], rp[1], rp[2], rp[3]);
         } else {
            printf("frame %d: ok\n", frame);
         }
      }
   }
   printf("pbotest: %s (%d failures / %d frames)\n", failures ? "FAIL" : "PASS",
          failures, frames);
   return failures ? 1 : 0;
}
