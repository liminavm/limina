/*
 * zink: unbounded recursion populating an EXT_multisampled_render_to_texture
 * transient when a scissored clear is pending on that attachment.
 *
 * Without VK_EXT_multisampled_render_to_single_sampled, zink emulates MSRTT
 * with a transient MSAA image that it fills by a replicate blit from the
 * single-sampled texture before the first renderpass. If a partial clear is
 * pending on that same attachment, the blit's framebuffer change flushes the
 * clear, which begins a renderpass, which starts the replicate blit again.
 *
 * Sequence: render to the texture through an MSRTT FBO, bind another FBO
 * (unbinding invalidates the transient), bind the MSRTT FBO again, issue a
 * scissored clear, draw.
 *
 * Build: cc -o zink-msrtt-recursion zink-msrtt-recursion.c -lEGL -lGLESv2
 * Run:   MESA_LOADER_DRIVER_OVERRIDE=zink ./zink-msrtt-recursion
 *        on a Vulkan driver without VK_EXT_multisampled_render_to_single_sampled
 *        (e.g. anv on Ice Lake).
 * Unfixed: "Caught recursion" from u_blitter, then SIGSEGV (stack overflow).
 * Fixed:   prints the centre/corner pixels and "ok", exit 0.
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define W 64
#define H 64

static void die(const char *m) { fprintf(stderr, "%s\n", m); exit(2); }

static GLuint shader(GLenum type, const char *src)
{
   GLuint s = glCreateShader(type);
   glShaderSource(s, 1, &src, NULL);
   glCompileShader(s);
   GLint ok;
   glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
   if (!ok) die("shader compile failed");
   return s;
}

int main(void)
{
   PFNEGLGETPLATFORMDISPLAYEXTPROC get_display =
      (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
   EGLDisplay dpy = get_display(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
   if (!eglInitialize(dpy, NULL, NULL)) die("eglInitialize failed");
   eglBindAPI(EGL_OPENGL_ES_API);
   static const EGLint ctx_attr[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
   EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attr);
   if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
      die("context failed");
   printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));
   if (!strstr((const char *)glGetString(GL_EXTENSIONS), "GL_EXT_multisampled_render_to_texture"))
      die("no GL_EXT_multisampled_render_to_texture");

   PFNGLFRAMEBUFFERTEXTURE2DMULTISAMPLEEXTPROC fb_tex_ms =
      (void *)eglGetProcAddress("glFramebufferTexture2DMultisampleEXT");

   GLuint prog = glCreateProgram();
   glAttachShader(prog, shader(GL_VERTEX_SHADER,
      "#version 300 es\n"
      "void main() {\n"
      "  vec2 p = vec2(gl_VertexID & 1, gl_VertexID >> 1) * 4.0 - 1.0;\n"
      "  gl_Position = vec4(p, 0.0, 1.0);\n"
      "}\n"));
   glAttachShader(prog, shader(GL_FRAGMENT_SHADER,
      "#version 300 es\n"
      "precision mediump float;\n"
      "uniform vec4 color;\n"
      "out vec4 o;\n"
      "void main() { o = color; }\n"));
   glLinkProgram(prog);
   glUseProgram(prog);
   GLint color = glGetUniformLocation(prog, "color");

   GLuint tex[2], fbo[2];
   glGenTextures(2, tex);
   glGenFramebuffers(2, fbo);
   for (int i = 0; i < 2; i++) {
      glBindTexture(GL_TEXTURE_2D, tex[i]);
      glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, W, H);
      glBindFramebuffer(GL_FRAMEBUFFER, fbo[i]);
      if (i == 0)
         fb_tex_ms(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex[i], 0, 4);
      else
         glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex[i], 0);
      if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
         die("fbo incomplete");
   }
   glViewport(0, 0, W, H);

   /* 1. give the texture contents through the MSRTT attachment */
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[0]);
   glUniform4f(color, 0, 0, 1, 1);
   glDrawArrays(GL_TRIANGLES, 0, 3);

   /* 2. bind something else: the transient is no longer valid */
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[1]);
   glClearColor(0, 0, 0, 1);
   glClear(GL_COLOR_BUFFER_BIT);
   glDrawArrays(GL_TRIANGLES, 0, 3);

   /* 3. back to MSRTT, partial clear, draw over part of it */
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[0]);
   glEnable(GL_SCISSOR_TEST);
   glScissor(0, 0, W / 2, H / 2);
   glClearColor(1, 0, 0, 1);
   glClear(GL_COLOR_BUFFER_BIT);
   glScissor(W / 2, H / 2, W / 2, H / 2);
   glUniform4f(color, 0, 1, 0, 1);
   glDrawArrays(GL_TRIANGLES, 0, 3);
   glDisable(GL_SCISSOR_TEST);

   GLubyte px[3][4];
   glReadPixels(4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px[0]);         /* cleared red */
   glReadPixels(W - 4, H - 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px[1]); /* drawn green */
   glReadPixels(W - 4, 4, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px[2]);     /* kept blue */
   for (int i = 0; i < 3; i++)
      printf("pixel %d: %u %u %u %u\n", i, px[i][0], px[i][1], px[i][2], px[i][3]);
   int ok = px[0][0] == 255 && px[0][1] == 0 && px[0][2] == 0 &&
            px[1][0] == 0 && px[1][1] == 255 && px[1][2] == 0 &&
            px[2][0] == 0 && px[2][1] == 0 && px[2][2] == 255;
   printf("%s\n", ok ? "ok" : "WRONG PIXELS");

   eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
   eglDestroyContext(dpy, ctx);
   eglTerminate(dpy);
   return ok ? 0 : 1;
}
