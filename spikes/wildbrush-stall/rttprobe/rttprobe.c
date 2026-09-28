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
 *
 * `rttprobe depth` is the other split wildbrush pays for: depth/stencil going in and out of use
 * inside one pass. One draw writes depth, then thousands of draws with the depth test off follow --
 * enough to cross threaded_context's batch boundaries, each of which starts a new render-pass info
 * that may see no depth use -- with a depth-tested draw every DEPTH_EVERY. Those must still see the
 * first draw's depth: the left half's draw lies behind it and must fail, the right half's in front
 * and must pass. So the left half must stay red and the right half turn yellow.
 */
#include <string.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
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
program_vs(const char *vs, const char *fs)
{
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
program(const char *fs)
{
   static const char *vs = "#version 300 es\n"
                           "void main() {\n"
                           "  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n"
                           "  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);\n"
                           "}\n";
   return program_vs(vs, fs);
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

/* CPU seconds of the whole process, zink's and KosmicKrisp's threads included: with the wall time,
 * it says whether a slower arm spends more CPU or waits more. */
static double
cpu(void)
{
   struct rusage ru;
   getrusage(RUSAGE_SELF, &ru);
   return ru.ru_utime.tv_sec + ru.ru_utime.tv_usec / 1e6 + ru.ru_stime.tv_sec +
          ru.ru_stime.tv_usec / 1e6;
}

static double
now(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec + ts.tv_nsec / 1e9;
}

#define DEPTH_DRAWS 4000
#define DEPTH_EVERY 400

/* A rectangle [x0,x1] x [-1,1] at depth z (NDC), in one color. */
static void
rect(GLuint prog, float x0, float x1, float z, const float *color)
{
   glUniform3f(glGetUniformLocation(prog, "r"), x0, x1, z);
   glUniform4fv(glGetUniformLocation(prog, "c"), 1, color);
   glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
}

static int
depth_phase(void)
{
   static const float red[4] = {1, 0, 0, 1}, green[4] = {0, 1, 0, 1}, blue[4] = {0, 0, 1, 1},
                      yellow[4] = {1, 1, 0, 1};
   GLuint prog = program_vs("#version 300 es\n"
                            "uniform vec3 r;\n"
                            "void main() {\n"
                            "  float x = (gl_VertexID & 1) == 0 ? r.x : r.y;\n"
                            "  float y = (gl_VertexID & 2) == 0 ? -1.0 : 1.0;\n"
                            "  gl_Position = vec4(x, y, r.z, 1.0);\n"
                            "}\n",
                            "#version 300 es\n"
                            "precision highp float;\n"
                            "uniform vec4 c;\n"
                            "out vec4 color;\n"
                            "void main() { color = c; }\n");
   GLuint color = texture(), depth;
   glGenRenderbuffers(1, &depth);
   glBindRenderbuffer(GL_RENDERBUFFER, depth);
   glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8, SIZE, SIZE);
   GLuint fbo;
   glGenFramebuffers(1, &fbo);
   glBindFramebuffer(GL_FRAMEBUFFER, fbo);
   glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, color, 0);
   glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, depth);
   GLuint vao;
   glGenVertexArrays(1, &vao);
   glBindVertexArray(vao);
   glViewport(0, 0, SIZE, SIZE);
   glUseProgram(prog);

   /* RTTPROBE_DEPTH_EVERY overrides how often a depth-tested draw comes, to see what the cost of
    * an arm scales with. */
   const char *every_env = getenv("RTTPROBE_DEPTH_EVERY");
   int depth_every = every_env ? atoi(every_env) : DEPTH_EVERY;
   const char *draws_env = getenv("RTTPROBE_DEPTH_DRAWS");
   int depth_draws = draws_env ? atoi(draws_env) : DEPTH_DRAWS;
   int rounds = 0, bad = 0;
   double start = now();
   static unsigned char px[SIZE * SIZE * 4];
   while (now() - start < RUN_SECONDS) {
      glClearColor(0, 0, 0, 1);
      glClearDepthf(1.0f);
      glDepthMask(GL_TRUE); /* a clear honours the depth mask the last round left off */
      glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
      glEnable(GL_DEPTH_TEST);
      glDepthFunc(GL_LESS);
      rect(prog, -1, 1, 0.0f, red);
      glDepthMask(GL_FALSE);
      for (int i = 1; i <= depth_draws; i++) {
         if (i % depth_every) {
            /* Depth off: pixel (0,0) only. */
            glDisable(GL_DEPTH_TEST);
            glEnable(GL_SCISSOR_TEST);
            glScissor(0, 0, 1, 1);
            rect(prog, -1, 1, 0.9f, green);
            glDisable(GL_SCISSOR_TEST);
         } else {
            glEnable(GL_DEPTH_TEST);
            rect(prog, -1, 0, 0.5f, blue);    /* behind the red: must fail */
            rect(prog, 0, 1, -0.5f, yellow);  /* in front of it: must pass */
         }
      }
      glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, px);
      int wrong = 0, first = -1;
      for (int i = 1; i < SIZE * SIZE; i++) {
         const unsigned char *p = &px[i * 4];
         bool left = (i % SIZE) < SIZE / 2;
         bool ok = left ? (p[0] == 255 && p[1] == 0 && p[2] == 0)
                        : (p[0] == 255 && p[1] == 255 && p[2] == 0);
         if (!ok && first < 0)
            first = i;
         wrong += !ok;
      }
      if (wrong) {
         if (!bad)
            fprintf(stderr, "round %d: %d pixels wrong, e.g. pixel %d = %u,%u,%u\n", rounds, wrong,
                    first, px[first * 4], px[first * 4 + 1], px[first * 4 + 2]);
         bad++;
      }
      rounds++;
   }
   printf("depth: %d rounds of %d draws, %d with wrong pixels: %s (%.1f s CPU in %.1f s)\n", rounds,
          depth_draws, bad, bad ? "FAIL" : "PASS", cpu(), now() - start);
   return bad ? 1 : 0;
}

int
main(int argc, char **argv)
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
   if (argc > 1 && !strcmp(argv[1], "depth"))
      return depth_phase();

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
   printf("%d rounds of %d ping-pong steps, %d with wrong pixels: %s (%.1f s CPU in %.1f s)\n",
          rounds, STEPS, bad, bad ? "FAIL" : "PASS", cpu(), now() - start);
   return bad ? 1 : 0;
}
