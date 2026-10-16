// From issue #15 (liminavm/limina), verbatim.
// Measures vertex throughput of the GLES driver: draws many tiny triangles into an offscreen framebuffer and reports
// triangles per second, for plain indexed meshes and instanced draws. Tiny triangles keep fragment cost negligible.
//
//   cc -O2 -o vtxbench vtxbench.c -lEGL -lGLESv2
//   ./vtxbench [frames]

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static const char* kVS =
    "#version 300 es\n"
    "layout(location = 0) in vec3 aPos;\n"
    "layout(location = 1) in vec3 aNormal;\n"
    "layout(location = 2) in vec3 aOffset;\n"
    "uniform mat4 uMVP;\n"
    "uniform mat3 uNormalMatrix;\n"
    "out vec3 vNormal;\n"
    "void main() {\n"
    "  vNormal = uNormalMatrix * aNormal;\n"
    "  gl_Position = uMVP * vec4(aPos + aOffset, 1.0);\n"
    "}\n";

static const char* kFS =
    "#version 300 es\n"
    "precision mediump float;\n"
    "in vec3 vNormal;\n"
    "out vec4 oColor;\n"
    "void main() { oColor = vec4(normalize(vNormal) * 0.5 + 0.5, 1.0); }\n";

static double now_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static GLuint compile(GLenum type, const char* src) {
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

// A grid of `tris` tiny triangles spread over clip space, as positions, normals and 32- or 16-bit indices.
static void make_mesh(int tris, int index16, GLuint* vbo, GLuint* nbo, GLuint* ibo) {
  int verts = tris * 3;
  float* pos = malloc(sizeof(float) * 3 * verts);
  float* nrm = malloc(sizeof(float) * 3 * verts);
  int side = 1;
  while (side * side < tris) side++;
  for (int t = 0; t < tris; t++) {
    float x = -0.95f + 1.9f * (t % side) / side, y = -0.95f + 1.9f * (t / side) / side;
    float d = 0.0005f;
    float v[9] = {x, y, 0, x + d, y, 0, x, y + d, 0};
    memcpy(pos + t * 9, v, sizeof(v));
    for (int k = 0; k < 3; k++) {
      nrm[t * 9 + k * 3] = 0;
      nrm[t * 9 + k * 3 + 1] = 0;
      nrm[t * 9 + k * 3 + 2] = 1;
    }
  }
  glGenBuffers(1, vbo);
  glBindBuffer(GL_ARRAY_BUFFER, *vbo);
  glBufferData(GL_ARRAY_BUFFER, sizeof(float) * 3 * verts, pos, GL_STATIC_DRAW);
  glGenBuffers(1, nbo);
  glBindBuffer(GL_ARRAY_BUFFER, *nbo);
  glBufferData(GL_ARRAY_BUFFER, sizeof(float) * 3 * verts, nrm, GL_STATIC_DRAW);
  glGenBuffers(1, ibo);
  glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, *ibo);
  if (index16) {
    uint16_t* idx = malloc(sizeof(uint16_t) * verts);
    for (int i = 0; i < verts; i++) idx[i] = (uint16_t)i;
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(uint16_t) * verts, idx, GL_STATIC_DRAW);
    free(idx);
  } else {
    uint32_t* idx = malloc(sizeof(uint32_t) * verts);
    for (int i = 0; i < verts; i++) idx[i] = (uint32_t)i;
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(uint32_t) * verts, idx, GL_STATIC_DRAW);
    free(idx);
  }
  free(pos);
  free(nrm);
}

// Draws `draws` draw calls of `tris` triangles each (times `instances` when instanced) per frame and returns
// the median milliseconds per frame, measured with glFinish.
static double run(int frames, int tris, int draws, int instances, int index16) {
  GLuint vbo, nbo, ibo, obo, vao;
  glGenVertexArrays(1, &vao);
  glBindVertexArray(vao);
  make_mesh(tris, index16, &vbo, &nbo, &ibo);
  glBindBuffer(GL_ARRAY_BUFFER, vbo);
  glEnableVertexAttribArray(0);
  glVertexAttribPointer(0, 3, GL_FLOAT, GL_FALSE, 0, 0);
  glBindBuffer(GL_ARRAY_BUFFER, nbo);
  glEnableVertexAttribArray(1);
  glVertexAttribPointer(1, 3, GL_FLOAT, GL_FALSE, 0, 0);
  float* offs = calloc((size_t)instances * 3, sizeof(float));
  for (int i = 0; i < instances; i++) offs[i * 3] = 0.00001f * i;
  glGenBuffers(1, &obo);
  glBindBuffer(GL_ARRAY_BUFFER, obo);
  glBufferData(GL_ARRAY_BUFFER, sizeof(float) * 3 * instances, offs, GL_STATIC_DRAW);
  free(offs);
  glEnableVertexAttribArray(2);
  glVertexAttribPointer(2, 3, GL_FLOAT, GL_FALSE, 0, 0);
  glVertexAttribDivisor(2, 1);

  GLenum type = index16 ? GL_UNSIGNED_SHORT : GL_UNSIGNED_INT;
  double* t = malloc(sizeof(double) * frames);
  for (int f = -2; f < frames; f++) {
    double t0 = now_ms();
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    for (int d = 0; d < draws; d++) {
      if (instances > 1)
        glDrawElementsInstanced(GL_TRIANGLES, tris * 3, type, 0, instances);
      else
        glDrawElements(GL_TRIANGLES, tris * 3, type, 0);
    }
    glFinish();
    if (f >= 0) t[f] = now_ms() - t0;
  }
  for (int i = 0; i < frames; i++)
    for (int j = i + 1; j < frames; j++)
      if (t[j] < t[i]) {
        double x = t[i];
        t[i] = t[j];
        t[j] = x;
      }
  double med = t[frames / 2];
  free(t);
  glDeleteBuffers(1, &vbo);
  glDeleteBuffers(1, &nbo);
  glDeleteBuffers(1, &ibo);
  glDeleteBuffers(1, &obo);
  glDeleteVertexArrays(1, &vao);
  return med;
}

int main(int argc, char** argv) {
  int frames = argc > 1 ? atoi(argv[1]) : 15;
  EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
  if (!eglInitialize(dpy, NULL, NULL)) {
    fprintf(stderr, "eglInitialize failed\n");
    return 1;
  }
  eglBindAPI(EGL_OPENGL_ES_API);
  EGLint cfg_attrs[] = {EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE};
  EGLConfig cfg;
  EGLint n;
  eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
  EGLint ctx_attrs[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE};
  EGLContext ctx = eglCreateContext(dpy, n ? cfg : EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attrs);
  if (!ctx || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
    fprintf(stderr, "context creation failed\n");
    return 1;
  }
  printf("renderer: %s\nversion: %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION));

  GLuint fbo, color, depth;
  glGenFramebuffers(1, &fbo);
  glBindFramebuffer(GL_FRAMEBUFFER, fbo);
  glGenRenderbuffers(1, &color);
  glBindRenderbuffer(GL_RENDERBUFFER, color);
  glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 1920, 1080);
  glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, color);
  glGenRenderbuffers(1, &depth);
  glBindRenderbuffer(GL_RENDERBUFFER, depth);
  glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH_COMPONENT24, 1920, 1080);
  glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, depth);
  glViewport(0, 0, 1920, 1080);
  glEnable(GL_DEPTH_TEST);

  GLuint prog = glCreateProgram();
  glAttachShader(prog, compile(GL_VERTEX_SHADER, kVS));
  glAttachShader(prog, compile(GL_FRAGMENT_SHADER, kFS));
  glLinkProgram(prog);
  glUseProgram(prog);
  float mvp[16] = {1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1};
  float nm[9] = {1, 0, 0, 0, 1, 0, 0, 0, 1};
  glUniformMatrix4fv(glGetUniformLocation(prog, "uMVP"), 1, GL_FALSE, mvp);
  glUniformMatrix3fv(glGetUniformLocation(prog, "uNormalMatrix"), 1, GL_FALSE, nm);

  struct {
    const char* name;
    int tris, draws, instances, index16;
  } cases[] = {
      {"empty frame (clear + finish)", 1, 0, 1, 0},
      {"1 draw x 100k tris, u32", 100000, 1, 1, 0},
      {"1 draw x 1M tris, u32", 1000000, 1, 1, 0},
      {"1 draw x 1M tris, u16 (multi-use)", 20000, 50, 1, 1},
      {"100 draws x 10k tris, u32", 10000, 100, 1, 0},
      {"1000 draws x 1k tris, u32", 1000, 1000, 1, 0},
      {"instanced 1k tris x 1000, u32", 1000, 1, 1000, 0},
      {"instanced 100 tris x 10000, u32", 100, 1, 10000, 0},
  };
  for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
    double ms = run(frames, cases[i].tris, cases[i].draws, cases[i].instances, cases[i].index16);
    double total = (double)cases[i].tris * cases[i].draws * cases[i].instances;
    printf("%-36s %9.2f ms/frame  %8.1f M tris/s\n", cases[i].name, ms,
           cases[i].draws ? total / ms / 1e3 : 0.0);
    fflush(stdout);
  }
  return 0;
}
