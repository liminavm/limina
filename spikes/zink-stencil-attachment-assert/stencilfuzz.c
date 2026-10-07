/* Random GL op sequences over framebuffers carrying depth/stencil, on zink, to hit
 *   assert(!pStencilAttachment || stencilAttachmentFormat)   (zink_context.c, begin_rendering)
 * Needs a zink built with asserts. On SIGABRT the last ops are printed.
 *
 *   stencilfuzz [ops] [seed]
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "CHECK failed: %s (line %d)\n", #x, __LINE__); exit(1); } } while (0)
#define W 256
#define H 128
#define RING 48

static const char *ring[RING];
static unsigned ring_pos;
static void note(const char *op) { ring[ring_pos++ % RING] = op; }
static void on_abort(int sig) {
   (void)sig;
   static const char hdr[] = "stencilfuzz: SIGABRT; last ops (oldest first):\n";
   write(2, hdr, sizeof hdr - 1);
   unsigned start = ring_pos > RING ? ring_pos - RING : 0;
   for (unsigned i = start; i < ring_pos; i++) {
      const char *s = ring[i % RING];
      write(2, "  ", 2); write(2, s, strlen(s)); write(2, "\n", 1);
   }
   signal(SIGABRT, SIG_DFL);
   abort();
}

static GLuint prog(void) {
   const char *vs = "#version 300 es\nin vec2 p; void main(){ gl_Position = vec4(p,0.5,1.0); }";
   const char *fs = "#version 300 es\nprecision mediump float; out vec4 c; uniform vec4 col; void main(){ c = col; }";
   GLuint v = glCreateShader(GL_VERTEX_SHADER), f = glCreateShader(GL_FRAGMENT_SHADER);
   glShaderSource(v, 1, &vs, NULL); glCompileShader(v);
   glShaderSource(f, 1, &fs, NULL); glCompileShader(f);
   GLuint p = glCreateProgram(); glAttachShader(p, v); glAttachShader(p, f);
   glBindAttribLocation(p, 0, "p"); glLinkProgram(p);
   GLint ok; glGetProgramiv(p, GL_LINK_STATUS, &ok); CHECK(ok);
   return p;
}

enum { FB_PLAIN, FB_MSAA, FB_MSRTT, FB_RESOLVE, FB_DEPTHONLY, NFB };

int main(int argc, char **argv) {
   long ops = argc > 1 ? atol(argv[1]) : 20000;
   unsigned seed = argc > 2 ? (unsigned)atoi(argv[2]) : 1;
   srand(seed);
   signal(SIGABRT, on_abort);

   EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
   CHECK(dpy != EGL_NO_DISPLAY && eglInitialize(dpy, NULL, NULL) && eglBindAPI(EGL_OPENGL_ES_API));
   EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE };
   EGLConfig cfg; EGLint n = 0;
   CHECK(eglChooseConfig(dpy, ca, &cfg, 1, &n) && n > 0);
   EGLint xa[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
   EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, xa);
   CHECK(ctx != EGL_NO_CONTEXT && eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx));
   const char *ext = (const char *)glGetString(GL_EXTENSIONS);
   int has_msrtt = ext && strstr(ext, "GL_EXT_multisampled_render_to_texture") != NULL;
   PFNGLFRAMEBUFFERTEXTURE2DMULTISAMPLEEXTPROC fbtex_ms =
      (PFNGLFRAMEBUFFERTEXTURE2DMULTISAMPLEEXTPROC)eglGetProcAddress("glFramebufferTexture2DMultisampleEXT");
   PFNGLRENDERBUFFERSTORAGEMULTISAMPLEEXTPROC rbs_ms =
      (PFNGLRENDERBUFFERSTORAGEMULTISAMPLEEXTPROC)eglGetProcAddress("glRenderbufferStorageMultisampleEXT");
   printf("stencilfuzz: %s | msrtt=%d | ops=%ld seed=%u\n", (const char *)glGetString(GL_RENDERER), has_msrtt, ops, seed);

   GLuint fbo[NFB], tex[NFB], zs[NFB];
   glGenFramebuffers(NFB, fbo); glGenTextures(NFB, tex); glGenRenderbuffers(NFB, zs);
   for (int i = 0; i < NFB; i++) {
      glBindTexture(GL_TEXTURE_2D, tex[i]);
      glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, W, H);
      glBindFramebuffer(GL_FRAMEBUFFER, fbo[i]);
      glBindRenderbuffer(GL_RENDERBUFFER, zs[i]);
      if (i == FB_MSAA) {
         GLuint crb; glGenRenderbuffers(1, &crb); glBindRenderbuffer(GL_RENDERBUFFER, crb);
         glRenderbufferStorageMultisample(GL_RENDERBUFFER, 4, GL_RGBA8, W, H);
         glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, crb);
         glBindRenderbuffer(GL_RENDERBUFFER, zs[i]);
         glRenderbufferStorageMultisample(GL_RENDERBUFFER, 4, GL_DEPTH24_STENCIL8, W, H);
      } else if (i == FB_MSRTT && has_msrtt) {
         fbtex_ms(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex[i], 0, 4);
         rbs_ms(GL_RENDERBUFFER, 4, GL_DEPTH24_STENCIL8, W, H);
      } else if (i == FB_DEPTHONLY) {
         glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex[i], 0);
         glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH_COMPONENT24, W, H);
      } else {
         glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex[i], 0);
         glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, W, H);
      }
      glFramebufferRenderbuffer(GL_FRAMEBUFFER, i == FB_DEPTHONLY ? GL_DEPTH_ATTACHMENT : GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, zs[i]);
      CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
   }

   GLuint p = prog(); glUseProgram(p);
   GLint col = glGetUniformLocation(p, "col");
   float tri[] = { -1, -1, 3, -1, -1, 3 };
   GLuint vbo; glGenBuffers(1, &vbo); glBindBuffer(GL_ARRAY_BUFFER, vbo);
   glBufferData(GL_ARRAY_BUFFER, sizeof tri, tri, GL_STATIC_DRAW);
   glEnableVertexAttribArray(0); glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
   unsigned char px[16];
   int cur = FB_PLAIN;
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[cur]);

   for (long i = 0; i < ops; i++) {
      int op = getenv("SF_DIRECTED") ? 16 : rand() % 17;
      glUniform4f(col, (rand() % 256) / 255.f, 0.5f, 0.2f, 1.f);
      switch (op) {
      case 0: note("bind fbo"); cur = rand() % NFB; if (cur == FB_MSRTT && !has_msrtt) cur = FB_PLAIN;
              glBindFramebuffer(GL_FRAMEBUFFER, fbo[cur]); break;
      case 1: note("clear all"); glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT); break;
      case 2: note("clear stencil"); glClearStencil(rand() & 255); glClear(GL_STENCIL_BUFFER_BIT); break;
      case 3: note("scissored clear zs"); glEnable(GL_SCISSOR_TEST); glScissor(rand() % W, rand() % H, 17, 9);
              glClear(rand() & 1 ? GL_STENCIL_BUFFER_BIT : GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
              glDisable(GL_SCISSOR_TEST); break;
      case 4: case 5: note("draw no-zs"); glDisable(GL_DEPTH_TEST); glDisable(GL_STENCIL_TEST); glDrawArrays(GL_TRIANGLES, 0, 3); break;
      case 6: note("draw stencil"); glEnable(GL_STENCIL_TEST); glStencilFunc(GL_EQUAL, rand() & 1, 0xff);
              glStencilOp(GL_KEEP, GL_INCR, GL_INCR); glDrawArrays(GL_TRIANGLES, 0, 3); glDisable(GL_STENCIL_TEST); break;
      case 7: note("draw depth"); glEnable(GL_DEPTH_TEST); glDrawArrays(GL_TRIANGLES, 0, 3); glDisable(GL_DEPTH_TEST); break;
      case 8: note("flush"); glFlush(); break;
      case 9: note("readpixels"); if (cur != FB_MSAA) glReadPixels(rand() % W, rand() % H, 2, 2, GL_RGBA, GL_UNSIGNED_BYTE, px); break;
      case 10: { note("blit msaa->resolve");
              glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo[FB_MSAA]); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fbo[FB_RESOLVE]);
              glBlitFramebuffer(0, 0, W, H, 0, 0, W, H, GL_COLOR_BUFFER_BIT, GL_NEAREST);
              glBindFramebuffer(GL_FRAMEBUFFER, fbo[cur]); break; }
      case 11: { note("blit zs plain->resolve");
              glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo[FB_PLAIN]); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fbo[FB_RESOLVE]);
              glBlitFramebuffer(0, 0, W, H, 0, 0, W, H, GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT, GL_NEAREST);
              glBindFramebuffer(GL_FRAMEBUFFER, fbo[cur]); break; }
      case 12: { note("invalidate zs"); GLenum a[] = { GL_DEPTH_ATTACHMENT, GL_STENCIL_ATTACHMENT };
              glInvalidateFramebuffer(GL_FRAMEBUFFER, 2, a); break; }
      case 13: note("copytexsubimage"); if (cur != FB_MSAA) { glBindTexture(GL_TEXTURE_2D, tex[FB_RESOLVE]);
              if (cur != FB_RESOLVE) glCopyTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 0, 0, 16, 16); } break;
      case 14: note("depth/stencil mask off draw"); glEnable(GL_DEPTH_TEST); glDepthMask(GL_FALSE); glEnable(GL_STENCIL_TEST);
              glStencilMask(0); glDrawArrays(GL_TRIANGLES, 0, 3); glStencilMask(0xff); glDepthMask(GL_TRUE);
              glDisable(GL_DEPTH_TEST); glDisable(GL_STENCIL_TEST); break;
      case 16: { note("directed: depth-only draw, flush, scaled DS blit, depth-only draw");
              glBindFramebuffer(GL_FRAMEBUFFER, fbo[FB_DEPTHONLY]); cur = FB_DEPTHONLY;
              glEnable(GL_DEPTH_TEST); glDrawArrays(GL_TRIANGLES, 0, 3); glDisable(GL_DEPTH_TEST);
              glFlush();
              glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo[FB_PLAIN]); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fbo[FB_RESOLVE]);
              GLbitfield m = (rand() & 1) ? GL_DEPTH_BUFFER_BIT | GL_STENCIL_BUFFER_BIT : GL_STENCIL_BUFFER_BIT;
              glBlitFramebuffer(0, 0, W, H, 0, 0, W / 2, H / 2, m, GL_NEAREST);
              glBindFramebuffer(GL_FRAMEBUFFER, fbo[FB_DEPTHONLY]);
              glEnable(GL_DEPTH_TEST); glDrawArrays(GL_TRIANGLES, 0, 3); glDisable(GL_DEPTH_TEST);
              glDrawArrays(GL_TRIANGLES, 0, 3);
              break; }
      case 15: note("finish"); if (rand() % 8 == 0) glFinish(); break;
      }
   }
   glFinish();
   printf("stencilfuzz: completed %ld ops, GL error 0x%x\n", ops, glGetError());
   return 0;
}
