// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Does a geometry shader's flat output come from the provoking vertex of each strip primitive
// it emits, under both conventions? Metal provokes from the first vertex only, so the last-vertex
// convention (GL's default) is emulated in poly's rasterization program. Runs on the GL stack vrend
// uses (EGL surfaceless -> Mesa st -> zink -> KosmicKrisp) and reads the pixels back.
// Exit 0 = both conventions right, 1 = wrong, 2 = setup failure.
//
// One point goes through a GS that emits a 4-vertex triangle strip covering the viewport, each
// vertex with its own flat colour: v0 red, v1 green, v2 blue, v3 white. Triangle 0 (v0 v1 v2)
// covers the bottom-left half, triangle 1 (v1 v2 v3) the top-right half. Last-vertex: blue then
// white. First-vertex: red then green.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/glcorearb.h>
#include <stdio.h>

// Every GL entry point through eglGetProcAddress: Mesa's libEGL exports no GL symbols.
#define GL_FUNCS(X)                                                                                \
  X(GETSTRING, GetString) X(CREATESHADER, CreateShader) X(SHADERSOURCE, ShaderSource)              \
  X(COMPILESHADER, CompileShader) X(GETSHADERIV, GetShaderiv) X(GETPROGRAMIV, GetProgramiv)        \
  X(GENRENDERBUFFERS, GenRenderbuffers) X(BINDRENDERBUFFER, BindRenderbuffer)                      \
  X(RENDERBUFFERSTORAGE, RenderbufferStorage) X(GENFRAMEBUFFERS, GenFramebuffers)                  \
  X(BINDFRAMEBUFFER, BindFramebuffer) X(FRAMEBUFFERRENDERBUFFER, FramebufferRenderbuffer)          \
  X(CHECKFRAMEBUFFERSTATUS, CheckFramebufferStatus) X(VIEWPORT, Viewport)                          \
  X(CREATEPROGRAM, CreateProgram) X(ATTACHSHADER, AttachShader) X(LINKPROGRAM, LinkProgram)       \
  X(USEPROGRAM, UseProgram) X(GENVERTEXARRAYS, GenVertexArrays)                                    \
  X(BINDVERTEXARRAY, BindVertexArray) X(CLEARCOLOR, ClearColor) X(CLEAR, Clear)                    \
  X(DRAWARRAYS, DrawArrays) X(FINISH, Finish) X(READPIXELS, ReadPixels) X(GETERROR, GetError)      \
  X(PROVOKINGVERTEX, ProvokingVertex)
#define DECL(T, n) static PFNGL##T##PROC gl##n;
GL_FUNCS(DECL)
#define LOAD(T, n)                                                                                 \
  if (!(gl##n = (PFNGL##T##PROC)eglGetProcAddress("gl" #n))) {                                     \
    fprintf(stderr, "no gl" #n "\n");                                                              \
    return 2;                                                                                      \
  }

#define W 64
#define H 64

static const char *vs = "#version 400 core\nvoid main() { gl_Position = vec4(0.0); }\n";
static const char *gs =
    "#version 400 core\nlayout(points) in;\nlayout(triangle_strip, max_vertices = 4) out;\n"
    "flat out vec4 col;\n"
    "void main() {\n"
    "  gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); col = vec4(1, 0, 0, 1); EmitVertex();\n"
    "  gl_Position = vec4(-1.0,  1.0, 0.0, 1.0); col = vec4(0, 1, 0, 1); EmitVertex();\n"
    "  gl_Position = vec4( 1.0, -1.0, 0.0, 1.0); col = vec4(0, 0, 1, 1); EmitVertex();\n"
    "  gl_Position = vec4( 1.0,  1.0, 0.0, 1.0); col = vec4(1, 1, 1, 1); EmitVertex();\n"
    "  EndPrimitive();\n"
    "}\n";
static const char *fs = "#version 400 core\nflat in vec4 col;\nout vec4 c;\n"
                        "void main() { c = col; }\n";

static GLuint shader(GLenum type, const char *src) {
  GLuint s = glCreateShader(type);
  glShaderSource(s, 1, &src, NULL);
  glCompileShader(s);
  GLint ok;
  glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
  if (!ok) { fprintf(stderr, "shader compile failed (type 0x%x)\n", type); return 0; }
  return s;
}

static unsigned pack(const unsigned char *p) { return p[0] << 16 | p[1] << 8 | p[2]; }

// Draws with the given convention; returns 1 if the two triangles read want0 and want1.
static int run(GLenum convention, const char *name, unsigned want0, unsigned want1) {
  glProvokingVertex(convention);
  glClearColor(0, 0, 0, 1);
  glClear(GL_COLOR_BUFFER_BIT);
  glDrawArrays(GL_POINTS, 0, 1);
  glFinish();

  unsigned char px[W * H * 4];
  glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
  // GL's origin is the bottom-left: (8, 8) is in triangle 0, (55, 55) in triangle 1.
  unsigned got0 = pack(&px[(8 * W + 8) * 4]), got1 = pack(&px[(55 * W + 55) * 4]);
  int ok = got0 == want0 && got1 == want1;
  printf("%s: triangle 0 %06x (want %06x), triangle 1 %06x (want %06x) %s (GL error 0x%x)\n",
         name, got0, want0, got1, want1, ok ? "ok" : "WRONG", glGetError());
  return ok;
}

int main(void) {
  EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);  // EGL_PLATFORM=surfaceless, as vrend runs
  if (!eglInitialize(dpy, NULL, NULL)) { fprintf(stderr, "eglInitialize failed\n"); return 2; }
  eglBindAPI(EGL_OPENGL_API);
  const EGLint ctx_attr[] = {EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 0,
                             EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                             EGL_NONE};
  EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attr);
  if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
    fprintf(stderr, "context failed\n");
    return 2;
  }
  GL_FUNCS(LOAD)
  printf("GL_RENDERER: %s\nGL_VERSION: %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION));

  GLuint fbo, rb;
  glGenRenderbuffers(1, &rb);
  glBindRenderbuffer(GL_RENDERBUFFER, rb);
  glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, W, H);
  glGenFramebuffers(1, &fbo);
  glBindFramebuffer(GL_FRAMEBUFFER, fbo);
  glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb);
  if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) return 2;
  glViewport(0, 0, W, H);

  GLuint prog = glCreateProgram();
  GLuint v = shader(GL_VERTEX_SHADER, vs), g = shader(GL_GEOMETRY_SHADER, gs),
         f = shader(GL_FRAGMENT_SHADER, fs);
  if (!v || !g || !f) return 2;
  glAttachShader(prog, v);
  glAttachShader(prog, g);
  glAttachShader(prog, f);
  glLinkProgram(prog);
  GLint linked;
  glGetProgramiv(prog, GL_LINK_STATUS, &linked);
  if (!linked) { fprintf(stderr, "link failed\n"); return 2; }
  glUseProgram(prog);

  GLuint vao;  // core profile draws need one bound, even with no attributes
  glGenVertexArrays(1, &vao);
  glBindVertexArray(vao);

  int ok = run(GL_LAST_VERTEX_CONVENTION, "last-vertex", 0x0000ff, 0xffffff);
  ok &= run(GL_FIRST_VERTEX_CONVENTION, "first-vertex", 0xff0000, 0x00ff00);
  printf("%s\n", ok ? "PASS" : "FAIL");
  return ok ? 0 : 1;
}
