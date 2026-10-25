/* Bind one long-lived texture buffer at a new range every frame.
 *
 * WebRender does this, and zink caches a VkBufferView per (format, offset,
 * size) on the buffer for as long as the buffer lives, so each new range
 * leaves one more texel-buffer texture on the GPU host. Run it in the guest
 * and read the host's count for its context while it sleeps at the end:
 *
 *   gcc -O2 -o tbo-churn tbo-churn.c -lEGL -lGLESv2
 *   ./tbo-churn [frames] [linger-seconds]
 *   limina debug <vm> status        # on the host, during the linger
 *
 * It prints the frames drawn and whether the last one sampled what it wrote, then deletes the
 * buffer: with zink's cache bounded, that teardown is what crashes a driver that walks the
 * cache's evicted entries as views (`./tbo-churn 100 0` aborts or segfaults).
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl32.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define BUF_BYTES (4 << 20)

static GLuint
shader(GLenum type, const char *src)
{
   GLuint s = glCreateShader(type);
   glShaderSource(s, 1, &src, NULL);
   glCompileShader(s);
   GLint ok;
   glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
   if (!ok) {
      char log[1024];
      glGetShaderInfoLog(s, sizeof(log), NULL, log);
      fprintf(stderr, "shader: %s\n", log);
      exit(1);
   }
   return s;
}

int
main(int argc, char **argv)
{
   int frames = argc > 1 ? atoi(argv[1]) : 1000;
   int linger = argc > 2 ? atoi(argv[2]) : 30;

   PFNEGLGETPLATFORMDISPLAYEXTPROC get_display =
      (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
   EGLDisplay dpy = get_display(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
   if (!eglInitialize(dpy, NULL, NULL)) {
      fprintf(stderr, "eglInitialize failed\n");
      return 1;
   }
   eglBindAPI(EGL_OPENGL_ES_API);
   const EGLint cattr[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 2, EGL_NONE};
   EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
   if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
      fprintf(stderr, "no GLES 3.2 context\n");
      return 1;
   }
   printf("renderer: %s\n", glGetString(GL_RENDERER));

   GLuint rb, fb;
   glGenRenderbuffers(1, &rb);
   glBindRenderbuffer(GL_RENDERBUFFER, rb);
   glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 1, 1);
   glGenFramebuffers(1, &fb);
   glBindFramebuffer(GL_FRAMEBUFFER, fb);
   glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb);
   glViewport(0, 0, 1, 1);

   GLuint prog = glCreateProgram();
   glAttachShader(prog, shader(GL_VERTEX_SHADER,
      "#version 320 es\n"
      "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); gl_PointSize = 1.0; }\n"));
   glAttachShader(prog, shader(GL_FRAGMENT_SHADER,
      "#version 320 es\n"
      "precision highp float;\n"
      "uniform highp samplerBuffer tbo;\n"
      "out vec4 color;\n"
      "void main() { color = texelFetch(tbo, 0); }\n"));
   glLinkProgram(prog);
   glUseProgram(prog);
   glUniform1i(glGetUniformLocation(prog, "tbo"), 0);

   GLint align;
   glGetIntegerv(GL_TEXTURE_BUFFER_OFFSET_ALIGNMENT, &align);

   /* Every texel is (1, 0, 1, 1), so any range samples magenta. */
   float *texels = malloc(BUF_BYTES);
   for (size_t i = 0; i < BUF_BYTES / sizeof(float); i++)
      texels[i] = (i % 4 == 1) ? 0.0f : 1.0f;
   GLuint buf, tex;
   glGenBuffers(1, &buf);
   glBindBuffer(GL_TEXTURE_BUFFER, buf);
   glBufferData(GL_TEXTURE_BUFFER, BUF_BYTES, texels, GL_STATIC_DRAW);
   unsigned char px[4] = {0};
   glActiveTexture(GL_TEXTURE0);

   /* Hold many views at once, one per texture object, so the buffer's view cache grows big
    * before any can be evicted; then let them go. Firefox's caches look like this. */
   enum { HELD = 200 };
   GLuint held[HELD];
   glGenTextures(HELD, held);
   for (int i = 0; i < HELD; i++) {
      glBindTexture(GL_TEXTURE_BUFFER, held[i]);
      glTexBufferRange(GL_TEXTURE_BUFFER, GL_RGBA32F, buf, (GLintptr)(BUF_BYTES / 2) + i * align, 4096);
      glDrawArrays(GL_POINTS, 0, 1);
   }
   glFinish();
   glDeleteTextures(HELD, held);

   glGenTextures(1, &tex);
   glBindTexture(GL_TEXTURE_BUFFER, tex);
   for (int f = 0; f < frames; f++) {
      GLintptr offset = (GLintptr)f * align % (BUF_BYTES / 2);
      glTexBufferRange(GL_TEXTURE_BUFFER, GL_RGBA32F, buf, offset, 4096);
      glClear(GL_COLOR_BUFFER_BIT);
      glDrawArrays(GL_POINTS, 0, 1);
      glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
   }
   GLenum err = glGetError();
   printf("frames %d, last pixel %u,%u,%u,%u (%s), gl error 0x%x\n", frames,
          px[0], px[1], px[2], px[3],
          px[0] == 255 && px[1] == 0 && px[2] == 255 ? "ok" : "WRONG", err);
   fflush(stdout);
   sleep(linger);

   /* The buffer dies with the views its cache has left: a teardown that walks evicted entries
    * as views crashes here. */
   glDeleteTextures(1, &tex);
   glDeleteBuffers(1, &buf);
   /* a draw unbinds its sampler view; the batches that used it release it as they recycle */
   for (int f = 0; f < 8; f++) {
      glClear(GL_COLOR_BUFFER_BIT);
      glDrawArrays(GL_POINTS, 0, 1);
      glFinish();
   }
   printf("buffer deleted\n");
   return 0;
}
