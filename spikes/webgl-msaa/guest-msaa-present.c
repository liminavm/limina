// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* The presenting half of the WebGL/MSAA reproducer.
 *
 * host-msaa-loop.c (this directory) renders multisampled at full size, with
 * reallocation churn, and does NOT reproduce the device loss: 30,712 frames on
 * the host, 226,909 inside the guest over virgl, against roughly 4,000 that kill
 * the VM when Firefox does it. What separates them is measured, not guessed:
 *
 *   Firefox window   drawing buffer                  result
 *   kiosk            follows the window (large)      DIES in 45-55 s
 *   kiosk            forced 800x600                  survives 210 s
 *   windowed         follows the window              survives 210 s
 *   windowed         forced 2560x1440, scaled down   survives 210 s
 *
 * A large buffer survives if it is scaled on the way out; a small one survives in
 * a full-size surface. Only a large buffer PRESENTED AT ITS OWN SIZE dies. Every
 * offscreen probe misses it because presenting is the one step none of them takes.
 *
 * So this probe presents. It keeps the two sizes independent, because that
 * separation is what made the browser arms readable at all and a probe that ties
 * its buffer to its window can only reproduce the confound:
 *
 *   --fullscreen / --windowed WxH   the SURFACE size
 *   --buffer WxH                    the DRAWING BUFFER size (default: surface)
 *
 * and two ways for the buffer to be multisampled, because it is not yet known
 * which one Firefox's canvas ends up as:
 *
 *   --mode surface   the window surface itself is multisampled (EGL/SDL
 *                    MULTISAMPLESAMPLES), the driver resolves at swap. This is
 *                    what a GL app asking for an antialiased default framebuffer
 *                    gets.
 *   --mode msrtt     a single-sample window, plus an offscreen colour texture with
 *                    an implicit multisample attachment via
 *                    EXT_multisampled_render_to_texture -- zink's shadow-attachment
 *                    path -- drawn 1:1 into the window each frame. This is what a
 *                    canvas composited by the browser looks like.
 *
 * SDL2 rather than raw Wayland deliberately: xdg-shell boilerplate would be three
 * times this file and none of it is the experiment.
 *
 * Exit status: 0 = survived, 1 = a GL error or context reset was seen, 77 = cannot
 * test here. A VM that dies takes the probe with it, so the real verdict is whether
 * the VM is still alive; the probe's own output is for the frame count and for
 * proving multisampling was actually granted.
 *
 * Build in the guest:  gcc -O2 -g -o guest-msaa-present guest-msaa-present.c \
 *                          $(pkg-config --cflags --libs sdl2) -lGLESv2
 */
#include <SDL2/SDL.h>
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

static const char *vs_src =
    "attribute vec2 pos;\n"
    "attribute vec2 uv;\n"
    "uniform vec2 off;\n"
    "uniform vec2 scale;\n"
    "varying vec2 v;\n"
    "void main() { v = uv; gl_Position = vec4(pos * scale + off, 0.0, 1.0); }\n";

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

static GLuint checker_texture(void) {
   enum { N = 256 };
   static unsigned char px[N * N * 4];
   for (int y = 0; y < N; y++)
      for (int x = 0; x < N; x++) {
         bool on = ((x >> 4) ^ (y >> 4)) & 1;
         unsigned char *p = px + 4 * (y * N + x);
         p[0] = on ? 255 : 40;
         p[1] = on ? 110 : 60;
         p[2] = on ? 20 : 220;
         p[3] = 255;
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

int main(int argc, char **argv) {
   int win_w = 0, win_h = 0; /* 0,0 = fullscreen desktop */
   int buf_w = 0, buf_h = 0; /* 0,0 = same as the drawable */
   int samples = 4, seconds = 180;
   bool msrtt = false;

   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "--seconds") && i + 1 < argc)
         seconds = atoi(argv[++i]);
      else if (!strcmp(argv[i], "--samples") && i + 1 < argc)
         samples = atoi(argv[++i]);
      else if (!strcmp(argv[i], "--windowed") && i + 1 < argc)
         sscanf(argv[++i], "%dx%d", &win_w, &win_h);
      else if (!strcmp(argv[i], "--fullscreen"))
         win_w = win_h = 0;
      else if (!strcmp(argv[i], "--buffer") && i + 1 < argc)
         sscanf(argv[++i], "%dx%d", &buf_w, &buf_h);
      else if (!strcmp(argv[i], "--mode") && i + 1 < argc)
         msrtt = !strcmp(argv[++i], "msrtt");
      else {
         fprintf(stderr,
                 "usage: %s [--fullscreen | --windowed WxH] [--buffer WxH]\n"
                 "          [--mode surface|msrtt] [--samples N] [--seconds N]\n",
                 argv[0]);
         return 2;
      }
   }

   if (SDL_Init(SDL_INIT_VIDEO) != 0) {
      fprintf(stderr, "SKIP: SDL_Init: %s\n", SDL_GetError());
      return SKIP;
   }
   SDL_GL_SetAttribute(SDL_GL_CONTEXT_PROFILE_MASK, SDL_GL_CONTEXT_PROFILE_ES);
   SDL_GL_SetAttribute(SDL_GL_CONTEXT_MAJOR_VERSION, 2);
   SDL_GL_SetAttribute(SDL_GL_RED_SIZE, 8);
   SDL_GL_SetAttribute(SDL_GL_GREEN_SIZE, 8);
   SDL_GL_SetAttribute(SDL_GL_BLUE_SIZE, 8);
   SDL_GL_SetAttribute(SDL_GL_ALPHA_SIZE, 8);
   SDL_GL_SetAttribute(SDL_GL_DEPTH_SIZE, 16);
   if (!msrtt) {
      SDL_GL_SetAttribute(SDL_GL_MULTISAMPLEBUFFERS, 1);
      SDL_GL_SetAttribute(SDL_GL_MULTISAMPLESAMPLES, samples);
   }

   Uint32 flags = SDL_WINDOW_OPENGL | SDL_WINDOW_SHOWN;
   if (win_w == 0)
      flags |= SDL_WINDOW_FULLSCREEN_DESKTOP;
   SDL_Window *win = SDL_CreateWindow(
       "msaa-present", SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED,
       win_w ? win_w : 1280, win_h ? win_h : 720, flags);
   if (!win) {
      fprintf(stderr, "SKIP: SDL_CreateWindow: %s\n", SDL_GetError());
      return SKIP;
   }
   SDL_GLContext ctx = SDL_GL_CreateContext(win);
   if (!ctx) {
      fprintf(stderr, "SKIP: SDL_GL_CreateContext: %s\n", SDL_GetError());
      return SKIP;
   }
   SDL_GL_SetSwapInterval(0); /* never pace on the compositor: the bug is a rate */

   int draw_w = 0, draw_h = 0;
   SDL_GL_GetDrawableSize(win, &draw_w, &draw_h);
   if (buf_w == 0) {
      buf_w = draw_w;
      buf_h = draw_h;
   }

   printf("GL_RENDERER: %s\n", (const char *)glGetString(GL_RENDERER));
   printf("surface: %dx%d (%s)   buffer: %dx%d   mode: %s   %d s\n", draw_w,
          draw_h, win_w ? "windowed" : "fullscreen", buf_w, buf_h,
          msrtt ? "msrtt" : "surface", seconds);

   /* Ask what was GRANTED. A stack that quietly declines multisampling renders at
    * full speed and reports a complete framebuffer, and every "survived" from such
    * a run is a statement about a workload that never took the path under test. */
   GLint got_samples = 0, got_sample_buffers = 0;

   GLuint msrtt_tex = 0, msrtt_depth = 0, msrtt_fb = 0;
   if (msrtt) {
      const char *exts = (const char *)glGetString(GL_EXTENSIONS);
      if (!exts || !strstr(exts, "GL_EXT_multisampled_render_to_texture")) {
         fprintf(stderr, "SKIP: no GL_EXT_multisampled_render_to_texture\n");
         return SKIP;
      }
      FBTEX2DMS fbtex2dms =
          (FBTEX2DMS)SDL_GL_GetProcAddress("glFramebufferTexture2DMultisampleEXT");
      RBSTORAGEMS rbstorage_ms =
          (RBSTORAGEMS)SDL_GL_GetProcAddress("glRenderbufferStorageMultisampleEXT");
      if (!fbtex2dms || !rbstorage_ms) {
         fprintf(stderr, "SKIP: MSRTT entry points missing\n");
         return SKIP;
      }
      glGenTextures(1, &msrtt_tex);
      glBindTexture(GL_TEXTURE_2D, msrtt_tex);
      glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, buf_w, buf_h, 0, GL_RGBA,
                   GL_UNSIGNED_BYTE, NULL);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
      glGenRenderbuffers(1, &msrtt_depth);
      glBindRenderbuffer(GL_RENDERBUFFER, msrtt_depth);
      rbstorage_ms(GL_RENDERBUFFER, samples, GL_DEPTH_COMPONENT16, buf_w, buf_h);
      glGenFramebuffers(1, &msrtt_fb);
      glBindFramebuffer(GL_FRAMEBUFFER, msrtt_fb);
      fbtex2dms(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, msrtt_tex, 0,
                samples);
      glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT,
                                GL_RENDERBUFFER, msrtt_depth);
      GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
      if (st != GL_FRAMEBUFFER_COMPLETE) {
         fprintf(stderr, "SKIP: MSRTT framebuffer incomplete: 0x%x\n", st);
         return SKIP;
      }
   }
   glGetIntegerv(GL_SAMPLES, &got_samples);
   glGetIntegerv(GL_SAMPLE_BUFFERS, &got_sample_buffers);
   printf("granted: SAMPLES=%d SAMPLE_BUFFERS=%d\n", got_samples,
          got_sample_buffers);
   if (got_samples < 2 || got_sample_buffers < 1) {
      fprintf(stderr,
              "SKIP: multisampling not granted (SAMPLES=%d SAMPLE_BUFFERS=%d)\n",
              got_samples, got_sample_buffers);
      return SKIP;
   }
   fflush(stdout);

   GLuint prog = glCreateProgram();
   glAttachShader(prog, compile(GL_VERTEX_SHADER, vs_src));
   glAttachShader(prog, compile(GL_FRAGMENT_SHADER, fs_src));
   glBindAttribLocation(prog, 0, "pos");
   glBindAttribLocation(prog, 1, "uv");
   glLinkProgram(prog);
   glUseProgram(prog);
   glUniform1i(glGetUniformLocation(prog, "t"), 0);
   GLint u_off = glGetUniformLocation(prog, "off");
   GLint u_scale = glGetUniformLocation(prog, "scale");

   GLuint tex = checker_texture();

   static const GLfloat quad[] = {
       -1.0f, -1.0f, 0.0f, 0.0f, 1.0f, -1.0f, 1.0f, 0.0f,
       -1.0f, 1.0f,  0.0f, 1.0f, 1.0f, 1.0f,  1.0f, 1.0f,
   };
   GLuint vbo;
   glGenBuffers(1, &vbo);
   glBindBuffer(GL_ARRAY_BUFFER, vbo);
   glBufferData(GL_ARRAY_BUFFER, sizeof(quad), quad, GL_STATIC_DRAW);
   glEnableVertexAttribArray(0);
   glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 16, (void *)0);
   glEnableVertexAttribArray(1);
   glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 16, (void *)8);

   const double t0 = now_s();
   double next_report = t0 + 5.0;
   unsigned long frames = 0;
   int rc = 0;

   while (now_s() - t0 < seconds) {
      SDL_Event ev;
      while (SDL_PollEvent(&ev))
         if (ev.type == SDL_QUIT)
            goto done;

      /* Render the antialiased content, into the multisampled window surface or
       * into the multisampled-render-to-texture canvas. */
      glBindFramebuffer(GL_FRAMEBUFFER, msrtt ? msrtt_fb : 0);
      glViewport(0, 0, msrtt ? buf_w : draw_w, msrtt ? buf_h : draw_h);
      glEnable(GL_DEPTH_TEST);
      glDisable(GL_SCISSOR_TEST);
      glClearColor(0.05f, 0.05f, 0.09f, 1.0f);
      glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
      /* A scissored clear keeps zink off its "skip the replicate blit because the
       * image will be fully cleared" branch, which is how the shadow path is
       * entered at all. */
      glEnable(GL_SCISSOR_TEST);
      glScissor(0, 0, (msrtt ? buf_w : draw_w) / 3, (msrtt ? buf_h : draw_h) / 3);
      glClearColor(0.0f, 0.2f, 0.0f, 1.0f);
      glClear(GL_COLOR_BUFFER_BIT);
      glDisable(GL_SCISSOR_TEST);

      glActiveTexture(GL_TEXTURE0);
      glBindTexture(GL_TEXTURE_2D, tex);
      glUniform2f(u_scale, 0.35f, 0.35f);
      for (int i = 0; i < 3; i++) {
         float a = (float)((frames + i * 40) % 240) / 240.0f;
         glUniform2f(u_off, -0.5f + i * 0.5f, -0.3f + 0.6f * a);
         glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
      }

      if (msrtt) {
         /* Draw the canvas into the window 1:1 — the browser's composite step,
          * and the step whose absence made every offscreen probe survive. */
         glBindFramebuffer(GL_FRAMEBUFFER, 0);
         glViewport(0, 0, draw_w, draw_h);
         glDisable(GL_DEPTH_TEST);
         glBindTexture(GL_TEXTURE_2D, msrtt_tex);
         glUniform2f(u_scale, 1.0f, 1.0f);
         glUniform2f(u_off, 0.0f, 0.0f);
         glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
      }

      SDL_GL_SwapWindow(win);
      frames++;

      GLenum err = glGetError();
      if (err != GL_NO_ERROR) {
         fprintf(stderr, "REPRO: GL error 0x%x after %lu frames, %.1f s\n", err,
                 frames, now_s() - t0);
         rc = 1;
         break;
      }

      if (now_s() >= next_report) {
         double el = now_s() - t0;
         printf("  %6.1f s  %8lu frames  %.1f fps\n", el, frames, frames / el);
         fflush(stdout);
         next_report += 5.0;
      }
   }
done:;
   double el = now_s() - t0;
   printf("%s after %lu frames in %.1f s (%.1f fps)\n",
          rc ? "REPRODUCED" : "survived", frames, el, frames / el);
   fflush(stdout);
   return rc;
}
