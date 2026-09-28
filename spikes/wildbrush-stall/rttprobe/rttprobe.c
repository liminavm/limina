/* The wildbrush render-pass split, on host zink-on-KosmicKrisp, with a pixel oracle.
 *
 * Each step renders into one of two textures while sampling the other (ping-pong), adding 1/255
 * to red. Before the sampling draw, a decoy draw opens the render pass on the destination with a
 * texture nothing ever writes. Binding the source -- written by the previous step's pass -- then
 * needs a read-after-write barrier while that pass is open, which is the split wildbrush pays for
 * on every such bind (spikes/wildbrush-stall/RESULTS.md, "What ends a render pass").
 *
 * After STEPS steps every pixel but the decoy's corner must read exactly STEPS. With
 * LIMINA_ZINK_RP_STATS=1 the [LIMINA-ZINK-RP] lines show how many passes resumed on the same
 * attachments. Runs rounds for RUN_SECONDS so a 5 s stats report lands inside the run.
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#define SIZE 64
#define STEPS 200
#define RUN_SECONDS 11.0

static GLuint
shader(GLenum type, const char *src)
{
   GLuint s = glCreateShader(type);
   glShaderSource(s, 1, &src, NULL);
   glCompileShader(s);
   GLint ok = 0;
   glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
   if (!ok) {
      char log[1024];
      glGetShaderInfoLog(s, sizeof(log), NULL, log);
      fprintf(stderr, "shader: %s\n", log);
      exit(2);
   }
   return s;
}

static GLuint
program(const char *fs)
{
   static const char *vs = "#version 300 es\n"
                           "void main() {\n"
                           "  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n"
                           "  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n"
                           "}\n";
   GLuint p = glCreateProgram();
   glAttachShader(p, shader(GL_VERTEX_SHADER, vs));
   glAttachShader(p, shader(GL_FRAGMENT_SHADER, fs));
   glLinkProgram(p);
   GLint ok = 0;
   glGetProgramiv(p, GL_LINK_STATUS, &ok);
   if (!ok) {
      fprintf(stderr, "link failed\n");
      exit(2);
   }
   return p;
}

static GLuint
texture(void)
{
   GLuint t;
   glGenTextures(1, &t);
   glBindTexture(GL_TEXTURE_2D, t);
   glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, SIZE, SIZE);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
   return t;
}

static double
now(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec + ts.tv_nsec / 1e9;
}

int
main(void)
{
   PFNEGLGETPLATFORMDISPLAYEXTPROC get_display =
      (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
   EGLDisplay dpy = get_display(EGL_PLATFORM_SURFACELESS_MESA, NULL, NULL);
   if (!eglInitialize(dpy, NULL, NULL)) {
      fprintf(stderr, "eglInitialize failed\n");
      return 2;
   }
   eglBindAPI(EGL_OPENGL_ES_API);
   static const EGLint cfg_attr[] = {EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE};
   EGLConfig cfg;
   EGLint n = 0;
   eglChooseConfig(dpy, cfg_attr, &cfg, 1, &n);
   static const EGLint ctx_attr[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE};
   EGLContext ctx = eglCreateContext(dpy, n ? cfg : EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attr);
   if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
      fprintf(stderr, "no GLES3 context\n");
      return 2;
   }
   printf("renderer: %s\n", glGetString(GL_RENDERER));

   GLuint accumulate = program("#version 300 es\n"
                               "precision highp float;\n"
                               "uniform highp sampler2D src;\n"
                               "out vec4 color;\n"
                               "void main() {\n"
                               "  vec4 s = texelFetch(src, ivec2(gl_FragCoord.xy), 0);\n"
                               "  color = vec4(s.r + 1.0 / 255.0, 0.0, 0.0, 1.0);\n"
                               "}\n");
   GLuint decoy = program("#version 300 es\n"
                          "precision highp float;\n"
                          "uniform highp sampler2D k;\n"
                          "out vec4 color;\n"
                          "void main() { color = texelFetch(k, ivec2(0), 0); }\n");

   GLuint tex[2] = {texture(), texture()};
   GLuint constant = texture();
   GLuint fbo[2];
   glGenFramebuffers(2, fbo);
   for (int i = 0; i < 2; i++) {
      glBindFramebuffer(GL_FRAMEBUFFER, fbo[i]);
      glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex[i], 0);
   }
   /* The decoy's texture is filled once, by upload, and never rendered to. */
   static unsigned char blue[SIZE * SIZE * 4];
   for (int i = 0; i < SIZE * SIZE; i++)
      blue[i * 4 + 2] = blue[i * 4 + 3] = 255;
   glBindTexture(GL_TEXTURE_2D, constant);
   glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, blue);

   GLuint vao;
   glGenVertexArrays(1, &vao);
   glBindVertexArray(vao);
   glViewport(0, 0, SIZE, SIZE);

   int rounds = 0, bad = 0;
   double start = now();
   static unsigned char px[SIZE * SIZE * 4];
   while (now() - start < RUN_SECONDS) {
      glBindFramebuffer(GL_FRAMEBUFFER, fbo[0]);
      glClearColor(0, 0, 0, 1);
      glClear(GL_COLOR_BUFFER_BIT);
      for (int step = 0; step < STEPS; step++) {
         int src = step & 1, dst = src ^ 1;
         glBindFramebuffer(GL_FRAMEBUFFER, fbo[dst]);
         /* Open the pass on dst with a texture that needs no barrier: pixel (0,0) only. */
         glEnable(GL_SCISSOR_TEST);
         glScissor(0, 0, 1, 1);
         glUseProgram(decoy);
         glActiveTexture(GL_TEXTURE0);
         glBindTexture(GL_TEXTURE_2D, constant);
         glDrawArrays(GL_TRIANGLES, 0, 3);
         glDisable(GL_SCISSOR_TEST);
         /* Now sample what the previous step's pass wrote, with that pass ended and this one open. */
         glUseProgram(accumulate);
         glBindTexture(GL_TEXTURE_2D, tex[src]);
         glDrawArrays(GL_TRIANGLES, 0, 3);
      }
      glBindFramebuffer(GL_FRAMEBUFFER, fbo[STEPS & 1]);
      glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, px);
      int wrong = 0;
      for (int i = 1; i < SIZE * SIZE; i++)
         wrong += px[i * 4] != STEPS;
      if (wrong) {
         if (!bad)
            fprintf(stderr, "round %d: %d pixels wrong, e.g. (1,0) red=%u (want %d)\n", rounds,
                    wrong, px[4], STEPS);
         bad++;
      }
      rounds++;
   }
   printf("%d rounds of %d ping-pong steps, %d with wrong pixels: %s\n", rounds, STEPS, bad,
          bad ? "FAIL" : "PASS");
   return bad ? 1 : 0;
}
