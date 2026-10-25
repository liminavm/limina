/* Clear an integer render target, then load a float one in the same slot.
 *
 * zink keeps each attachment slot's last clear value and hands it to the next pass even when
 * that pass loads instead of clearing. Vulkan ignores it then, but KosmicKrisp converts it by
 * the new attachment's format, so all-ones integer bits become a NaN float clear color. Run it
 * in the guest; with a host that rejects non-finite clear colors the context is lost (the
 * worker log names `context lost: BadValue ... (op 0x203)`) and the readback fails.
 *
 *   gcc -O2 -o stale-clear stale-clear.c -lEGL -lGLESv2
 *   ./stale-clear
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl32.h>
#include <stdio.h>

int
main(void)
{
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

   GLuint rb[2], fb;
   glGenRenderbuffers(2, rb);
   glBindRenderbuffer(GL_RENDERBUFFER, rb[0]);
   glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA32UI, 4, 4);
   glBindRenderbuffer(GL_RENDERBUFFER, rb[1]);
   glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 4, 4);
   glGenFramebuffers(1, &fb);
   glBindFramebuffer(GL_FRAMEBUFFER, fb);
   glViewport(0, 0, 4, 4);

   /* The integer clear leaves all-ones bits as the slot's clear value. */
   glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[0]);
   const GLuint ones[4] = {~0u, ~0u, ~0u, ~0u};
   glClearBufferuiv(GL_COLOR, 0, ones);
   glFinish();

   /* A float target in the same slot, drawn into with a load: a scissored clear is a draw. */
   glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb[1]);
   glEnable(GL_SCISSOR_TEST);
   glScissor(0, 0, 1, 1);
   glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
   glClear(GL_COLOR_BUFFER_BIT);
   glFinish();

   unsigned char px[4] = {0};
   glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
   GLenum err = glGetError();
   printf("pixel %u,%u,%u,%u (%s), gl error 0x%x\n", px[0], px[1], px[2], px[3],
          px[0] == 0 && px[1] == 0 && px[2] == 255 ? "ok" : "WRONG", err);
   return 0;
}
