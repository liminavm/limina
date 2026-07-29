// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* crossmark GL backend — EGL (surfaceless) + OpenGL ES 3.0, offscreen FBO.
 * Mirrors crossmark_vk.c's scene exactly; see crossmark.h for the contract.
 * Idiomatic-GL equivalents are deliberate (glUniform4fv per draw vs push
 * constants, glUseProgram vs pipeline bind, PBO orphan+upload vs staging
 * copy): we compare what real apps do on each API, not synthetic parity. */

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#ifdef HAVE_WAYLAND
#include <wayland-egl.h>
#include "cmwin.h"
#endif
#include "crossmark.h"

#define GLCHECK()                                                     \
   do {                                                               \
      GLenum e_ = glGetError();                                       \
      if (e_ != GL_NO_ERROR) {                                        \
         fprintf(stderr, "GL error 0x%x @ %s:%d\n", e_, __FILE__, __LINE__); \
         exit(1);                                                     \
      }                                                               \
   } while (0)

static const char *vert_body =
   "precision highp float;\n"
   "uniform vec4 u_rect;\n"
   "uniform vec4 u_tint;\n"
   "out vec2 uv;\n"
   "void main() {\n"
   "   vec2 corners[6] = vec2[6](vec2(0.,0.), vec2(1.,0.), vec2(1.,1.),\n"
   "                             vec2(0.,0.), vec2(1.,1.), vec2(0.,1.));\n"
   "   vec2 c = corners[gl_VertexID];\n"
   "   uv = c;\n"
   "   gl_Position = vec4(u_rect.xy + c * u_rect.zw, 0.0, 1.0);\n"
   "}\n";

static const char *flat_frag_body =
   "precision highp float;\n"
   "uniform vec4 u_rect;\n"
   "uniform vec4 u_tint;\n"
   "in vec2 uv;\n"
   "out vec4 o;\n"
   "void main() { o = vec4(u_tint.rgb * VARF, u_tint.a); }\n";

static const char *tex_frag_body =
   "precision highp float;\n"
   "uniform sampler2D u_tex;\n"
   "uniform vec4 u_rect;\n"
   "uniform vec4 u_tint;\n"
   "in vec2 uv;\n"
   "out vec4 o;\n"
   "void main() { o = texture(u_tex, uv) * u_tint; }\n";

static GLuint
compile(GLenum type, const char *defines, const char *body)
{
   char src[4096];
   snprintf(src, sizeof(src), "#version 300 es\n%s%s", defines ? defines : "", body);
   GLuint sh = glCreateShader(type);
   const char *p = src;
   glShaderSource(sh, 1, &p, NULL);
   glCompileShader(sh);
   GLint ok;
   glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
   if (!ok) {
      char log[2048];
      glGetShaderInfoLog(sh, sizeof(log), NULL, log);
      fprintf(stderr, "shader compile failed:\n%s\n%s\n", log, src);
      exit(1);
   }
   return sh;
}

static GLuint
link_program(const char *frag_defines, const char *frag_body)
{
   GLuint vs = compile(GL_VERTEX_SHADER, NULL, vert_body);
   GLuint fs = compile(GL_FRAGMENT_SHADER, frag_defines, frag_body);
   GLuint prog = glCreateProgram();
   glAttachShader(prog, vs);
   glAttachShader(prog, fs);
   glLinkProgram(prog);
   GLint ok;
   glGetProgramiv(prog, GL_LINK_STATUS, &ok);
   if (!ok) {
      char log[2048];
      glGetProgramInfoLog(prog, sizeof(log), NULL, log);
      fprintf(stderr, "program link failed:\n%s\n", log);
      exit(1);
   }
   glDeleteShader(vs);
   glDeleteShader(fs);
   return prog;
}

#ifndef EGL_PLATFORM_SURFACELESS_MESA
#define EGL_PLATFORM_SURFACELESS_MESA 0x31DD
#endif

int
main(int argc, char **argv)
{
   struct cm_opts o;
   if (cm_parse_args(&o, argc, argv))
      return 1;
#ifndef HAVE_WAYLAND
   if (o.present) {
      fprintf(stderr, "-p/-F need a wayland-enabled build (Linux guest)\n");
      return 1;
   }
#endif

   int width = CM_WIDTH, height = CM_HEIGHT;
   EGLDisplay dpy = EGL_NO_DISPLAY;
   EGLSurface wsurf = EGL_NO_SURFACE;
#ifdef HAVE_WAYLAND
   struct cm_win *win = NULL;
   struct wl_egl_window *egl_win = NULL;
   if (o.present) {
      win = cm_win_create(CM_WIDTH, CM_HEIGHT, o.fullscreen, "crossmark");
      if (!win)
         return 1;
      width = win->width;
      height = win->height;
      dpy = eglGetPlatformDisplay(EGL_PLATFORM_WAYLAND_KHR, win->dpy, NULL);
   }
#endif
   PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display =
      (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
   if (dpy == EGL_NO_DISPLAY && !o.present && get_platform_display)
      dpy = get_platform_display(EGL_PLATFORM_SURFACELESS_MESA, NULL, NULL);
   if (dpy == EGL_NO_DISPLAY)
      dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
   if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
      fprintf(stderr, "no EGL display (0x%x)\n", eglGetError());
      return 1;
   }
   eglBindAPI(EGL_OPENGL_ES_API);

   const EGLint cfg_attrs[] = { EGL_RENDERABLE_TYPE,
                                EGL_OPENGL_ES3_BIT,
                                EGL_SURFACE_TYPE,
                                o.present ? EGL_WINDOW_BIT : 0,
                                EGL_RED_SIZE,
                                8,
                                EGL_GREEN_SIZE,
                                8,
                                EGL_BLUE_SIZE,
                                8,
                                EGL_NONE };
   EGLConfig cfg;
   EGLint ncfg = 0;
   eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &ncfg);
   if (o.present && !ncfg) {
      fprintf(stderr, "no window-capable EGL config\n");
      return 1;
   }
   const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE };
   EGLContext ctx =
      eglCreateContext(dpy, ncfg ? cfg : EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attrs);
   if (ctx == EGL_NO_CONTEXT) {
      fprintf(stderr, "eglCreateContext failed (0x%x)\n", eglGetError());
      return 1;
   }
#ifdef HAVE_WAYLAND
   if (o.present) {
      egl_win = wl_egl_window_create(win->surface, width, height);
      wsurf = eglCreateWindowSurface(dpy, cfg, (EGLNativeWindowType)egl_win, NULL);
      if (wsurf == EGL_NO_SURFACE) {
         fprintf(stderr, "eglCreateWindowSurface failed (0x%x)\n", eglGetError());
         return 1;
      }
   }
#endif
   if (!eglMakeCurrent(dpy, wsurf, wsurf, ctx)) {
      fprintf(stderr, "eglMakeCurrent failed (0x%x)\n", eglGetError());
      return 1;
   }
   if (o.present)
      eglSwapInterval(dpy, 0); /* uncapped */

   const char *renderer = (const char *)glGetString(GL_RENDERER);
   fprintf(stderr, "device: %s\n", renderer);
   if (o.present)
      fprintf(stderr, "window: %dx%d swap-interval 0\n", width, height);

   /* offscreen target (present mode renders to the window's default FB) */
   GLuint fbo = 0, target = 0;
   if (!o.present) {
      glGenTextures(1, &target);
      glBindTexture(GL_TEXTURE_2D, target);
      glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, CM_WIDTH, CM_HEIGHT);
      glGenFramebuffers(1, &fbo);
      glBindFramebuffer(GL_FRAMEBUFFER, fbo);
      glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                             target, 0);
      if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
         fprintf(stderr, "FBO incomplete\n");
         return 1;
      }
   }
   glViewport(0, 0, width, height);
   glDisable(GL_DITHER); /* determinism for the pixel hash */

   GLuint vao;
   glGenVertexArrays(1, &vao);
   glBindVertexArray(vao);

   /* programs: CM_NVARIANTS flat variants + tex */
   GLuint flat_progs[CM_NVARIANTS], tex_prog;
   GLint flat_rect[CM_NVARIANTS], flat_tint[CM_NVARIANTS], tex_rect, tex_tint;
   for (int v = 0; v < CM_NVARIANTS; v++) {
      char defs[64];
      snprintf(defs, sizeof(defs), "#define VARF %.8f\n",
               (double)(v + 1) / CM_NVARIANTS);
      flat_progs[v] = link_program(defs, flat_frag_body);
      flat_rect[v] = glGetUniformLocation(flat_progs[v], "u_rect");
      flat_tint[v] = glGetUniformLocation(flat_progs[v], "u_tint");
   }
   tex_prog = link_program(NULL, tex_frag_body);
   tex_rect = glGetUniformLocation(tex_prog, "u_rect");
   tex_tint = glGetUniformLocation(tex_prog, "u_tint");

   /* textures: 2 static + 1 streaming, NEAREST/CLAMP for determinism */
   const size_t stream_size = CM_UPLOAD_TEX * CM_UPLOAD_TEX * 4;
   uint8_t *texels = malloc(stream_size);
   GLuint tex[3];
   glGenTextures(3, tex);
   for (int i = 0; i < 3; i++) {
      glBindTexture(GL_TEXTURE_2D, tex[i]);
      glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, CM_UPLOAD_TEX, CM_UPLOAD_TEX);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
      if (i < 2) {
         cm_fill_texture(texels, CM_UPLOAD_TEX, CM_UPLOAD_TEX, i);
         glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, CM_UPLOAD_TEX, CM_UPLOAD_TEX, GL_RGBA,
                         GL_UNSIGNED_BYTE, texels);
      }
   }
   GLCHECK();

   /* streaming PBO */
   GLuint pbo;
   glGenBuffers(1, &pbo);
   uint8_t *stream_src = malloc(stream_size);

   struct cm_times t = { 0 };
   int tex_shape = o.shape == CM_SHAPE_UPLOAD || o.shape == CM_SHAPE_DESKTOP;

   for (int frame = 0; frame < o.nframes + o.warmup; frame++) {
      double t0 = cm_now_ms();

      if (o.shape == CM_SHAPE_UPLOAD) {
         /* stream 1 MiB of texels through the PBO into tex 2 (orphan+upload).
          * CM_GL_NO_PBO=1 uploads from the client pointer instead — a
          * diagnostic split for drivers whose PBO path misbehaves. */
         cm_fill_stream(stream_src, frame);
         int no_pbo = getenv("CM_GL_NO_PBO") != NULL;
         glBindTexture(GL_TEXTURE_2D, tex[2]);
         if (no_pbo) {
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, CM_UPLOAD_TEX, CM_UPLOAD_TEX,
                            GL_RGBA, GL_UNSIGNED_BYTE, stream_src);
         } else {
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
            glBufferData(GL_PIXEL_UNPACK_BUFFER, stream_size, stream_src,
                         GL_STREAM_DRAW);
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, CM_UPLOAD_TEX, CM_UPLOAD_TEX,
                            GL_RGBA, GL_UNSIGNED_BYTE, (const void *)0);
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
         }
      }

      glClearColor(0.05f, 0.05f, 0.1f, 1.0f);
      glClear(GL_COLOR_BUFFER_BIT);

      int bound_tex = -1, bound_variant = -1;
      GLint loc_rect = 0, loc_tint = 0;
      if (tex_shape) {
         glUseProgram(tex_prog);
         loc_rect = tex_rect;
         loc_tint = tex_tint;
      } else if (o.shape == CM_SHAPE_DRAWS) {
         bound_variant = CM_NVARIANTS - 1;
         glUseProgram(flat_progs[bound_variant]);
         loc_rect = flat_rect[bound_variant];
         loc_tint = flat_tint[bound_variant];
      }

      float p[8];
      for (int i = 0; i < o.ndraws; i++) {
         cm_draw_params(o.shape, frame, i, p);
         if (o.shape == CM_SHAPE_STATE) {
            int v = i % CM_NVARIANTS;
            if (v != bound_variant) {
               glUseProgram(flat_progs[v]);
               loc_rect = flat_rect[v];
               loc_tint = flat_tint[v];
               bound_variant = v;
            }
         } else if (tex_shape) {
            int ti = cm_draw_texture(o.shape, i);
            if (ti != bound_tex) {
               glBindTexture(GL_TEXTURE_2D, tex[ti]);
               bound_tex = ti;
            }
         }
         glUniform4fv(loc_rect, 1, p);
         glUniform4fv(loc_tint, 1, p + 4);
         glDrawArrays(GL_TRIANGLES, 0, tex_shape ? 6 : 3);
      }
      double t1 = cm_now_ms();

      glFlush();
      double t2 = cm_now_ms();

      double t3 = t2, t4 = t2;
      if (o.present) {
#ifdef HAVE_WAYLAND
         /* present-paced: no glFinish; eglSwapBuffers is the throttle */
         eglSwapBuffers(dpy, wsurf);
         cm_win_pump(win);
         t4 = cm_now_ms();
#endif
      } else {
         glFinish();
         t3 = cm_now_ms();
         t4 = t3;
      }

      if (frame >= o.warmup) {
         t.draw += t1 - t0;
         t.flush += t2 - t1;
         t.sync += t3 - t2;
         t.present += t4 - t3;
         t.total += t4 - t0;
         t.frames++;
      }
   }
   GLCHECK();

   /* Readback + hash. GL rows come back bottom-up from a bottom-left origin,
    * VK top-down from a top-left origin with +y down — both make row 0 the
    * NDC y=-1 row, so the hashes are directly comparable. Caveat: guest
    * glReadPixels has burned us before (#28 black readback on zink) — an
    * all-zero readback is reported as suspect, not trusted. */
   uint64_t hash = 0;
   if (o.hash) {
      size_t rb_size = CM_WIDTH * CM_HEIGHT * 4;
      uint8_t *rb = malloc(rb_size);
      glReadPixels(0, 0, CM_WIDTH, CM_HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, rb);
      GLCHECK();
      hash = cm_hash_pixels(rb, rb_size);
      int nonzero = 0;
      for (size_t i = 0; i < rb_size && !nonzero; i++)
         nonzero = rb[i] != 0;
      if (!nonzero)
         fprintf(stderr, "WARNING: all-zero readback — glReadPixels may be lying "
                         "(see #28); do not trust this hash\n");
      free(rb);
   }

   cm_report("gl", &o, renderer, &t, hash);
   return 0;
}
