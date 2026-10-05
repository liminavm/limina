/* virgl: glTexSubImage2D from a bound pixel-unpack buffer waits on the host.
 *
 * Each iteration draws with an atlas texture, writes a tile into a PBO with
 * glBufferSubData, uploads the tile from the PBO into the atlas with
 * glTexSubImage2D, and draws again -- the pattern of a canvas renderer that
 * streams glyph/path tiles into a shared atlas. The same loop runs with a
 * client-memory pointer for comparison.
 *
 * DRM_IOCTL_VIRTGPU_WAIT calls are counted and timed by interposing ioctl(),
 * so the program needs nothing beyond EGL/GLES.
 *
 * Build: cc -O2 -o virgl-pbo-upload-wait virgl-pbo-upload-wait.c \
 *            -lEGL -lGLESv2 -ldl
 * Run:   ./virgl-pbo-upload-wait pbo
 *        ./virgl-pbo-upload-wait cpu
 *
 * Every uploaded tile is read back at the end, so a faster run that uploads
 * the wrong data fails.
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <dlfcn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define DRM_IOCTL_VIRTGPU_WAIT 0xc0086448

static unsigned long n_wait;
static double t_wait, max_wait;

static double
now(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec + ts.tv_nsec / 1e9;
}

int
ioctl(int fd, unsigned long req, ...)
{
   static int (*real_ioctl)(int, unsigned long, ...);
   va_list ap;
   va_start(ap, req);
   void *arg = va_arg(ap, void *);
   va_end(ap);
   if (!real_ioctl)
      real_ioctl = dlsym(RTLD_NEXT, "ioctl");
   if (req != DRM_IOCTL_VIRTGPU_WAIT)
      return real_ioctl(fd, req, arg);
   const double t0 = now();
   const int r = real_ioctl(fd, req, arg);
   const double dt = now() - t0;
   n_wait++;
   t_wait += dt;
   if (dt > max_wait)
      max_wait = dt;
   return r;
}

#define CHECK(x)                                                               \
   do {                                                                        \
      if (!(x)) {                                                              \
         fprintf(stderr, "FAILED: %s\n", #x);                                  \
         exit(2);                                                              \
      }                                                                        \
   } while (0)

#define ATLAS 1024
#define TILE 64
#define PBO_SIZE (1 << 20)
#define ITERATIONS 300

static const char *vs_src =
   "#version 300 es\n"
   "const vec2 p[4] = vec2[](vec2(-1,-1), vec2(1,-1), vec2(-1,1), vec2(1,1));\n"
   "out vec2 uv;\n"
   "void main() { uv = p[gl_VertexID] * 0.5 + 0.5;"
   " gl_Position = vec4(p[gl_VertexID], 0, 1); }\n";
static const char *fs_src =
   "#version 300 es\n"
   "precision mediump float;\n"
   "uniform sampler2D t;\n"
   "in vec2 uv;\n"
   "out vec4 c;\n"
   "void main() { c = texture(t, uv); }\n";

static GLuint
shader(GLenum type, const char *src)
{
   GLuint s = glCreateShader(type);
   glShaderSource(s, 1, &src, NULL);
   glCompileShader(s);
   GLint ok;
   glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
   CHECK(ok);
   return s;
}

int
main(int argc, char **argv)
{
   const int use_pbo = argc > 1 && !strcmp(argv[1], "pbo");

   PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display =
      (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress(
         "eglGetPlatformDisplayEXT");
   EGLDisplay dpy =
      get_platform_display(EGL_PLATFORM_SURFACELESS_MESA, NULL, NULL);
   CHECK(dpy != EGL_NO_DISPLAY && eglInitialize(dpy, NULL, NULL));
   CHECK(eglBindAPI(EGL_OPENGL_ES_API));
   const EGLint ctx_attribs[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
   EGLContext ctx =
      eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attribs);
   CHECK(ctx != EGL_NO_CONTEXT);
   CHECK(eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx));
   printf("GL_RENDERER: %s\nmode: %s\n", glGetString(GL_RENDERER),
          use_pbo ? "PBO" : "client memory");

   GLuint prog = glCreateProgram();
   glAttachShader(prog, shader(GL_VERTEX_SHADER, vs_src));
   glAttachShader(prog, shader(GL_FRAGMENT_SHADER, fs_src));
   glLinkProgram(prog);
   glUseProgram(prog);
   GLuint vao;
   glGenVertexArrays(1, &vao);
   glBindVertexArray(vao);

   GLuint atlas;
   glGenTextures(1, &atlas);
   glBindTexture(GL_TEXTURE_2D, atlas);
   glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, ATLAS, ATLAS);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);

   GLuint rt, fb;
   glGenTextures(1, &rt);
   glBindTexture(GL_TEXTURE_2D, rt);
   glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 512, 512);
   glGenFramebuffers(1, &fb);
   glBindFramebuffer(GL_FRAMEBUFFER, fb);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                          rt, 0);
   CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
   glViewport(0, 0, 512, 512);
   glBindTexture(GL_TEXTURE_2D, atlas);

   GLuint pbo;
   glGenBuffers(1, &pbo);
   glBindBuffer(GL_PIXEL_UNPACK_BUFFER, pbo);
   glBufferData(GL_PIXEL_UNPACK_BUFFER, PBO_SIZE, NULL, GL_STREAM_DRAW);
   if (!use_pbo)
      glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);

   static uint8_t tile[TILE * TILE * 4];
   const size_t tile_bytes = sizeof(tile);
   size_t offset = 0;

   glFinish();
   n_wait = 0;
   t_wait = max_wait = 0;
   const double t0 = now();
   for (int i = 0; i < ITERATIONS; i++) {
      memset(tile, i, tile_bytes);
      const int tx = (i % (ATLAS / TILE)) * TILE;
      const int ty = (i / (ATLAS / TILE) % (ATLAS / TILE)) * TILE;

      glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

      if (use_pbo) {
         if (offset + tile_bytes > PBO_SIZE) {
            glBufferData(GL_PIXEL_UNPACK_BUFFER, PBO_SIZE, NULL,
                         GL_STREAM_DRAW);
            offset = 0;
         }
         glBufferSubData(GL_PIXEL_UNPACK_BUFFER, offset, tile_bytes, tile);
         glTexSubImage2D(GL_TEXTURE_2D, 0, tx, ty, TILE, TILE, GL_RGBA,
                         GL_UNSIGNED_BYTE, (const void *)(uintptr_t)offset);
         offset += tile_bytes;
      } else {
         glTexSubImage2D(GL_TEXTURE_2D, 0, tx, ty, TILE, TILE, GL_RGBA,
                         GL_UNSIGNED_BYTE, tile);
      }

      glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
      if (i % 10 == 9)
         glFlush();
   }
   glFinish();
   const double total = now() - t0;
   const unsigned long loop_waits = n_wait;
   const double loop_wait_time = t_wait, loop_max_wait = max_wait;

   /* Check the uploads landed: read every tile of the atlas back. */
   GLuint check;
   glGenFramebuffers(1, &check);
   glBindFramebuffer(GL_FRAMEBUFFER, check);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                          atlas, 0);
   int bad = 0;
   for (int i = 0; i < ITERATIONS && i < (ATLAS / TILE) * (ATLAS / TILE); i++) {
      const int tx = (i % (ATLAS / TILE)) * TILE;
      const int ty = (i / (ATLAS / TILE) % (ATLAS / TILE)) * TILE;
      uint8_t px[4];
      glReadPixels(tx + TILE / 2, ty + TILE / 2, 1, 1, GL_RGBA,
                   GL_UNSIGNED_BYTE, px);
      bad += px[0] != (uint8_t)i;
   }

   printf("%d iterations: %.1f ms total, %.3f ms/iteration\n", ITERATIONS,
          total * 1e3, total * 1e3 / ITERATIONS);
   printf("VIRTGPU_WAIT in the loop: %lu calls, %.1f ms total, max %.2f ms\n",
          loop_waits, loop_wait_time * 1e3, loop_max_wait * 1e3);
   printf("%d tiles with the wrong contents\n", bad);
   return bad ? 1 : 0;
}
