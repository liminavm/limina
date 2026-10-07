// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Does host GL honour primitive restart inside a triangle LIST? Draws on the GL stack vrend uses
// (EGL surfaceless -> Mesa st -> zink -> KosmicKrisp), from a bound element buffer, the way vrend
// draws, and reads the pixels back. Exit 0 = conformant, 1 = wrong pixels, 2 = setup failure.
//
// Index list {0, R, 1, 2, 3} with R the restart index. Restart discards the pending 0 and starts
// over, so the draw is triangle 1-2-3 (right half). A driver that ignores restart in lists sees
// triangles (0, R, 1) and an incomplete (2, 3): the right half stays clear. Vertex 0 lies on the
// LEFT, so a misread that pulls it in shows there too.
//
// Usage: probe [fixed|index]
//   fixed  GL_PRIMITIVE_RESTART_FIXED_INDEX (0xFFFF for shorts), what WebGL 2 uses
//   index  GL_PRIMITIVE_RESTART + glPrimitiveRestartIndex(0xFFFF)  (default)
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

// Every GL entry point through eglGetProcAddress: Mesa's libEGL exports no GL symbols.
#define GL_FUNCS(X)                                                                                \
  X(GETSTRING, GetString) X(CREATESHADER, CreateShader) X(SHADERSOURCE, ShaderSource)              \
  X(COMPILESHADER, CompileShader) X(GETSHADERIV, GetShaderiv)                                      \
  X(GENRENDERBUFFERS, GenRenderbuffers) X(BINDRENDERBUFFER, BindRenderbuffer)                      \
  X(RENDERBUFFERSTORAGE, RenderbufferStorage) X(GENFRAMEBUFFERS, GenFramebuffers)                  \
  X(BINDFRAMEBUFFER, BindFramebuffer) X(FRAMEBUFFERRENDERBUFFER, FramebufferRenderbuffer)          \
  X(CHECKFRAMEBUFFERSTATUS, CheckFramebufferStatus) X(VIEWPORT, Viewport)                          \
  X(CREATEPROGRAM, CreateProgram) X(ATTACHSHADER, AttachShader) X(LINKPROGRAM, LinkProgram)       \
  X(USEPROGRAM, UseProgram) X(GENVERTEXARRAYS, GenVertexArrays)                                    \
  X(BINDVERTEXARRAY, BindVertexArray) X(GENBUFFERS, GenBuffers) X(BINDBUFFER, BindBuffer)          \
  X(BUFFERDATA, BufferData) X(VERTEXATTRIBPOINTER, VertexAttribPointer)                            \
  X(ENABLEVERTEXATTRIBARRAY, EnableVertexAttribArray) X(ENABLE, Enable)                            \
  X(PRIMITIVERESTARTINDEX, PrimitiveRestartIndex) X(CLEARCOLOR, ClearColor) X(CLEAR, Clear)        \
  X(DRAWELEMENTS, DrawElements) X(FINISH, Finish) X(READPIXELS, ReadPixels)                        \
  X(GETERROR, GetError)
#define DECL(T, n) static PFNGL##T##PROC gl##n;
GL_FUNCS(DECL)
#define LOAD(T, n)                                                                                 \
  if (!(gl##n = (PFNGL##T##PROC)eglGetProcAddress("gl" #n))) {                                     \
    fprintf(stderr, "no gl" #n "\n");                                                              \
    return 2;                                                                                      \
  }

#define W 64
#define H 64

static const char *vs = "#version 330 core\nlayout(location=0) in vec2 p;\n"
                        "void main() { gl_Position = vec4(p, 0.0, 1.0); }\n";
static const char *fs = "#version 330 core\nout vec4 c;\n"
                        "void main() { c = vec4(0.0, 1.0, 0.0, 1.0); }\n";

static GLuint shader(GLenum type, const char *src) {
  GLuint s = glCreateShader(type);
  glShaderSource(s, 1, &src, NULL);
  glCompileShader(s);
  GLint ok;
  glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
  if (!ok) { fprintf(stderr, "shader compile failed\n"); return 0; }
  return s;
}

int main(int argc, char **argv) {
  int fixed = argc > 1 && !strcmp(argv[1], "fixed");
  EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);  // EGL_PLATFORM=surfaceless, as vrend runs
  if (!eglInitialize(dpy, NULL, NULL)) { fprintf(stderr, "eglInitialize failed\n"); return 2; }
  eglBindAPI(EGL_OPENGL_API);
  const EGLint ctx_attr[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3,
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
  GLuint v = shader(GL_VERTEX_SHADER, vs), f = shader(GL_FRAGMENT_SHADER, fs);
  if (!v || !f) return 2;
  glAttachShader(prog, v);
  glAttachShader(prog, f);
  glLinkProgram(prog);
  glUseProgram(prog);

  // 0 on the left; 1-2-3 cover most of the right half.
  static const float verts[] = {-0.9f, 0.0f, 0.1f, -0.9f, 0.9f, -0.9f, 0.5f, 0.9f};
  static const GLushort idx[] = {0, 0xFFFF, 1, 2, 3};
  GLuint vao, vbo, ebo;
  glGenVertexArrays(1, &vao);
  glBindVertexArray(vao);
  glGenBuffers(1, &vbo);
  glBindBuffer(GL_ARRAY_BUFFER, vbo);
  glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_STATIC_DRAW);
  glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
  glEnableVertexAttribArray(0);
  glGenBuffers(1, &ebo);
  glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ebo);
  glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof(idx), idx, GL_STATIC_DRAW);

  if (fixed) {
    glEnable(GL_PRIMITIVE_RESTART_FIXED_INDEX);
  } else {
    glEnable(GL_PRIMITIVE_RESTART);
    glPrimitiveRestartIndex(0xFFFF);
  }
  glClearColor(0, 0, 0, 1);
  glClear(GL_COLOR_BUFFER_BIT);
  // Twice: the second draw takes the index-scan cache's hit path.
  glDrawElements(GL_TRIANGLES, 5, GL_UNSIGNED_SHORT, 0);
  glDrawElements(GL_TRIANGLES, 5, GL_UNSIGNED_SHORT, 0);
  glFinish();

  unsigned char px[W * H * 4];
  glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
  int left = 0, right = 0;
  for (int y = 0; y < H; y++)
    for (int x = 0; x < W; x++)
      if (px[(y * W + x) * 4 + 1] > 128) *(x < W / 2 ? &left : &right) += 1;
  printf("mode=%s green pixels: left=%d right=%d (GL error 0x%x)\n", fixed ? "fixed" : "index",
         left, right, glGetError());
  int ok = left == 0 && right > W * H / 8;
  printf("%s\n", ok ? "PASS: restart honoured in a triangle list" : "FAIL");
  return ok ? 0 : 1;
}
