// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Does glDrawTransformFeedback draw what a GEOMETRY SHADER captured? zink turns it into
// vkCmdDrawIndirectByteCountEXT, and with a GS only the GPU knows the byte count. Runs on the GL
// stack vrend uses (EGL surfaceless -> Mesa st -> zink -> KosmicKrisp) and reads the pixels back.
// Exit 0 = drawn as captured, 1 = wrong, 2 = setup failure.
//
// Pass 1 captures, with rasterization discarded: four points go through a GS that emits a
// triangle around a point only when gl_PrimitiveIDIn < 2, so it captures 2 triangles (6
// vertices), from the two TOP points. Pass 2 draws the capture with glDrawTransformFeedback.
// Expected: green in the top-left and top-right quadrants, nothing in the bottom half. A dropped
// draw leaves everything clear; a count taken from the input (4 points) instead of the GS output
// draws a wrong shape or garbage.
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
  X(BINDVERTEXARRAY, BindVertexArray) X(GENBUFFERS, GenBuffers) X(BINDBUFFER, BindBuffer)          \
  X(BUFFERDATA, BufferData) X(VERTEXATTRIBPOINTER, VertexAttribPointer)                            \
  X(ENABLEVERTEXATTRIBARRAY, EnableVertexAttribArray) X(ENABLE, Enable) X(DISABLE, Disable)        \
  X(CLEARCOLOR, ClearColor) X(CLEAR, Clear) X(DRAWARRAYS, DrawArrays) X(FINISH, Finish)            \
  X(READPIXELS, ReadPixels) X(GETERROR, GetError)                                                  \
  X(TRANSFORMFEEDBACKVARYINGS, TransformFeedbackVaryings)                                          \
  X(GENTRANSFORMFEEDBACKS, GenTransformFeedbacks) X(BINDTRANSFORMFEEDBACK, BindTransformFeedback)  \
  X(BINDBUFFERBASE, BindBufferBase) X(BEGINTRANSFORMFEEDBACK, BeginTransformFeedback)              \
  X(ENDTRANSFORMFEEDBACK, EndTransformFeedback) X(DRAWTRANSFORMFEEDBACK, DrawTransformFeedback)    \
  X(GENQUERIES, GenQueries) X(BEGINQUERY, BeginQuery) X(ENDQUERY, EndQuery)                        \
  X(GETQUERYOBJECTUIV, GetQueryObjectuiv)
#define DECL(T, n) static PFNGL##T##PROC gl##n;
GL_FUNCS(DECL)
#define LOAD(T, n)                                                                                 \
  if (!(gl##n = (PFNGL##T##PROC)eglGetProcAddress("gl" #n))) {                                     \
    fprintf(stderr, "no gl" #n "\n");                                                              \
    return 2;                                                                                      \
  }

#define W 64
#define H 64

static const char *cap_vs = "#version 400 core\nlayout(location=0) in vec2 p;\n"
                            "void main() { gl_Position = vec4(p, 0.0, 1.0); }\n";
static const char *cap_gs =
    "#version 400 core\nlayout(points) in;\nlayout(triangle_strip, max_vertices = 3) out;\n"
    "void main() {\n"
    "  if (gl_PrimitiveIDIn >= 2) return;\n"
    "  vec4 c = gl_in[0].gl_Position;\n"
    "  gl_Position = c + vec4(-0.3, -0.3, 0.0, 0.0); EmitVertex();\n"
    "  gl_Position = c + vec4( 0.3, -0.3, 0.0, 0.0); EmitVertex();\n"
    "  gl_Position = c + vec4( 0.0,  0.3, 0.0, 0.0); EmitVertex();\n"
    "  EndPrimitive();\n"
    "}\n";
static const char *draw_vs = "#version 400 core\nlayout(location=0) in vec4 p;\n"
                             "void main() { gl_Position = p; }\n";
static const char *draw_fs = "#version 400 core\nout vec4 c;\n"
                             "void main() { c = vec4(0.0, 1.0, 0.0, 1.0); }\n";

static GLuint shader(GLenum type, const char *src) {
  GLuint s = glCreateShader(type);
  glShaderSource(s, 1, &src, NULL);
  glCompileShader(s);
  GLint ok;
  glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
  if (!ok) { fprintf(stderr, "shader compile failed (type 0x%x)\n", type); return 0; }
  return s;
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

  // Capture program: VS + GS, gl_Position recorded.
  GLuint cap = glCreateProgram();
  GLuint cv = shader(GL_VERTEX_SHADER, cap_vs), cg = shader(GL_GEOMETRY_SHADER, cap_gs);
  if (!cv || !cg) return 2;
  glAttachShader(cap, cv);
  glAttachShader(cap, cg);
  const char *varyings[] = {"gl_Position"};
  glTransformFeedbackVaryings(cap, 1, varyings, GL_INTERLEAVED_ATTRIBS);
  glLinkProgram(cap);
  GLint linked;
  glGetProgramiv(cap, GL_LINK_STATUS, &linked);
  if (!linked) { fprintf(stderr, "capture link failed\n"); return 2; }

  GLuint draw = glCreateProgram();
  GLuint dv = shader(GL_VERTEX_SHADER, draw_vs), df = shader(GL_FRAGMENT_SHADER, draw_fs);
  if (!dv || !df) return 2;
  glAttachShader(draw, dv);
  glAttachShader(draw, df);
  glLinkProgram(draw);
  glGetProgramiv(draw, GL_LINK_STATUS, &linked);
  if (!linked) { fprintf(stderr, "draw link failed\n"); return 2; }

  // Points: 0 and 1 on top (captured), 2 and 3 at the bottom (dropped by the GS).
  static const float pts[] = {-0.5f, 0.5f, 0.5f, 0.5f, -0.5f, -0.5f, 0.5f, -0.5f};
  GLuint vao_in, vbo_in;
  glGenVertexArrays(1, &vao_in);
  glBindVertexArray(vao_in);
  glGenBuffers(1, &vbo_in);
  glBindBuffer(GL_ARRAY_BUFFER, vbo_in);
  glBufferData(GL_ARRAY_BUFFER, sizeof(pts), pts, GL_STATIC_DRAW);
  glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
  glEnableVertexAttribArray(0);

  // Room for every point's triangle, so a wrong count reads defined data.
  GLuint xfb_buf, tfo, q;
  glGenBuffers(1, &xfb_buf);
  glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, xfb_buf);
  glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 4 * 3 * 16, NULL, GL_STATIC_DRAW);
  glGenTransformFeedbacks(1, &tfo);
  glBindTransformFeedback(GL_TRANSFORM_FEEDBACK, tfo);
  glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, xfb_buf);
  glGenQueries(1, &q);

  glUseProgram(cap);
  glEnable(GL_RASTERIZER_DISCARD);
  glBeginQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, q);
  glBeginTransformFeedback(GL_TRIANGLES);
  glDrawArrays(GL_POINTS, 0, 4);
  glEndTransformFeedback();
  glEndQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN);
  glDisable(GL_RASTERIZER_DISCARD);
  glBindTransformFeedback(GL_TRANSFORM_FEEDBACK, 0);

  // Draw the capture.
  GLuint vao_out;
  glGenVertexArrays(1, &vao_out);
  glBindVertexArray(vao_out);
  glBindBuffer(GL_ARRAY_BUFFER, xfb_buf);
  glVertexAttribPointer(0, 4, GL_FLOAT, GL_FALSE, 16, 0);
  glEnableVertexAttribArray(0);
  glUseProgram(draw);
  glClearColor(0, 0, 0, 1);
  glClear(GL_COLOR_BUFFER_BIT);
  glDrawTransformFeedback(GL_TRIANGLES, tfo);
  glFinish();

  GLuint prims = 0;
  glGetQueryObjectuiv(q, GL_QUERY_RESULT, &prims);
  unsigned char px[W * H * 4];
  glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
  int tl = 0, tr = 0, bottom = 0;
  for (int y = 0; y < H; y++)
    for (int x = 0; x < W; x++)
      if (px[(y * W + x) * 4 + 1] > 128) {
        if (y < H / 2) bottom++;  // GL's origin is the bottom-left
        else if (x < W / 2) tl++;
        else tr++;
      }
  printf("primitives written %u; green pixels: top-left=%d top-right=%d bottom=%d (GL error 0x%x)\n",
         prims, tl, tr, bottom, glGetError());
  int ok = prims == 2 && tl > 0 && tr > 0 && bottom == 0;
  printf("%s\n", ok ? "PASS: drew what the geometry shader captured" : "FAIL");
  return ok ? 0 : 1;
}
