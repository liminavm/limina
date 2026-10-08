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
 * Run:   MESA_LOADER_DRIVER_OVERRIDE=zink ./zink-msrtt-recursion [color|zs]
 *        on a Vulkan driver without VK_EXT_multisampled_render_to_single_sampled
 *        (e.g. anv, KosmicKrisp, or venus on either).
 *
 * color (default): the clear is on the colour attachment. Pixels: the
 *   cleared quadrant red, the drawn quadrant green, an untouched quadrant
 *   keeps pass 1's blue.
 * zs: the clear is on a depth attachment, which zink shadows separately (the
 *   depth/stencil leg of zink_render_attachment_shadow). Pass 1 writes depth
 *   0.5 everywhere; pass 3 leaves a scissored depth clear to 1.0 pending on
 *   the lower-left quadrant and draws green at depth 0.75 with GL_LESS.
 *   Pixels: green where the clear landed, pass 1's blue wherever the
 *   replicated depth 0.5 survived.
 *
 * Unfixed: "Caught recursion" from u_blitter, then SIGSEGV (stack overflow).
 * Fixed:   prints the pixels and "ok", exit 0.
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

static GLuint texture(GLenum format)
{
   GLuint t;
   glGenTextures(1, &t);
   glBindTexture(GL_TEXTURE_2D, t);
   glTexStorage2D(GL_TEXTURE_2D, 1, format, W, H);
   return t;
}

static void complete(void)
{
   if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
      die("fbo incomplete");
}

int main(int argc, char **argv)
{
   const char *mode = argc > 1 ? argv[1] : "color";
   const int zs = !strcmp(mode, "zs");
   if (!zs && strcmp(mode, "color")) die("mode: color|zs");

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
      "uniform float z;\n"
      "void main() {\n"
      "  vec2 p = vec2(gl_VertexID & 1, gl_VertexID >> 1) * 4.0 - 1.0;\n"
      "  gl_Position = vec4(p, z, 1.0);\n"
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
   GLint z = glGetUniformLocation(prog, "z");

   GLuint fbo[2];
   glGenFramebuffers(2, fbo);
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[0]);
   fb_tex_ms(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture(GL_RGBA8), 0, 4);
   if (zs)
      fb_tex_ms(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D,
                texture(GL_DEPTH_COMPONENT24), 0, 4);
   complete();
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[1]);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                          texture(GL_RGBA8), 0);
   complete();
   glViewport(0, 0, W, H);

   /* 1. give the textures contents through the MSRTT attachments: colour blue,
    * and in zs mode depth 0.5 (z = 0) everywhere */
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[0]);
   if (zs) {
      glEnable(GL_DEPTH_TEST);
      glDepthFunc(GL_LESS);
      glClearDepthf(1.0f);
      glClear(GL_DEPTH_BUFFER_BIT);
   }
   glUniform1f(z, 0.0f);
   glUniform4f(color, 0, 0, 1, 1);
   glDrawArrays(GL_TRIANGLES, 0, 3);

   /* 2. bind something else: the transients are no longer valid */
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[1]);
   glDisable(GL_DEPTH_TEST);
   glClearColor(0, 0, 0, 1);
   glClear(GL_COLOR_BUFFER_BIT);
   glDrawArrays(GL_TRIANGLES, 0, 3);

   /* 3. back to MSRTT, partial clear, draw */
   glBindFramebuffer(GL_FRAMEBUFFER, fbo[0]);
   glEnable(GL_SCISSOR_TEST);
   glScissor(0, 0, W / 2, H / 2);
   if (zs) {
      glEnable(GL_DEPTH_TEST);
      glClearDepthf(1.0f);
      glClear(GL_DEPTH_BUFFER_BIT);
      glDisable(GL_SCISSOR_TEST);
      glUniform1f(z, 0.5f); /* depth 0.75: passes only where the clear landed */
   } else {
      glClearColor(1, 0, 0, 1);
      glClear(GL_COLOR_BUFFER_BIT);
      glScissor(W / 2, H / 2, W / 2, H / 2);
   }
   glUniform4f(color, 0, 1, 0, 1);
   glDrawArrays(GL_TRIANGLES, 0, 3);
   glDisable(GL_SCISSOR_TEST);
   glDisable(GL_DEPTH_TEST);

   static const int at[3][2] = { { 4, 4 }, { W - 4, H - 4 }, { W - 4, 4 } };
   static const GLubyte want_color[3][3] = { { 255, 0, 0 }, { 0, 255, 0 }, { 0, 0, 255 } };
   static const GLubyte want_zs[3][3] = { { 0, 255, 0 }, { 0, 0, 255 }, { 0, 0, 255 } };
   const GLubyte (*want)[3] = zs ? want_zs : want_color;
   int ok = 1;
   for (int i = 0; i < 3; i++) {
      GLubyte px[4];
      glReadPixels(at[i][0], at[i][1], 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
      printf("pixel %d: %u %u %u %u\n", i, px[0], px[1], px[2], px[3]);
      ok &= px[0] == want[i][0] && px[1] == want[i][1] && px[2] == want[i][2];
   }
   printf("%s\n", ok ? "ok" : "WRONG PIXELS");

   eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
   eglDestroyContext(dpy, ctx);
   eglTerminate(dpy);
   return ok ? 0 : 1;
}
