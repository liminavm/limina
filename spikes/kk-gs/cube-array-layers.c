// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Renders a colour into each of the 18 layer-faces of a cube-map array through an FBO layer, then
// samples each back through a samplerCubeArray with the layer in gl_TexCoord[0].w, the way piglit's
// arb_texture_cube_map_array-fbo-cubemap-array does (compatibility profile, glBegin). Runs on the
// host GL stack vrend uses. Exit 0 = every layer reads its own colour.
//   ./cube-array-layers [quads|tris]     FIXEDFN=1 renders the layers with glColor + GL_QUADS
//                                        instead of glClear
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#define F(t, n) static t n;
static void *gpa(const char *n) { char b[128]; strcpy(b, n); size_t l = strlen(b); if (b[l-1] == '_') b[l-1] = 0; void *f = (void *)eglGetProcAddress(b); if (!f) fprintf(stderr, "missing %s\n", b); return f; }
#define L(t, n) n = (t)gpa(#n);
#define FUNCS(X) X(PFNGLCREATESHADERPROC, glCreateShader_) X(PFNGLSHADERSOURCEPROC, glShaderSource_) X(PFNGLCOMPILESHADERPROC, glCompileShader_) \
 X(PFNGLCREATEPROGRAMPROC, glCreateProgram_) X(PFNGLATTACHSHADERPROC, glAttachShader_) X(PFNGLLINKPROGRAMPROC, glLinkProgram_) X(PFNGLUSEPROGRAMPROC, glUseProgram_) \
 X(PFNGLGETUNIFORMLOCATIONPROC, glGetUniformLocation_) X(PFNGLUNIFORM1IPROC, glUniform1i_) X(PFNGLGENFRAMEBUFFERSPROC, glGenFramebuffers_) X(PFNGLBINDFRAMEBUFFERPROC, glBindFramebuffer_) \
 X(PFNGLFRAMEBUFFERTEXTURELAYERPROC, glFramebufferTextureLayer_) X(PFNGLGENRENDERBUFFERSPROC, glGenRenderbuffers_) X(PFNGLBINDRENDERBUFFERPROC, glBindRenderbuffer_) \
 X(PFNGLRENDERBUFFERSTORAGEPROC, glRenderbufferStorage_) X(PFNGLFRAMEBUFFERRENDERBUFFERPROC, glFramebufferRenderbuffer_) X(PFNGLTEXIMAGE3DPROC, glTexImage3D_)
FUNCS(F)
typedef void (*V)(void); typedef void (*E1)(GLenum); typedef void (*F4)(const GLfloat *); typedef void (*F2)(GLfloat, GLfloat);
int main(int argc, char **argv) {
  int tris = argc > 1 && !strcmp(argv[1], "tris");
  EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY); eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
  const EGLint ca[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE}; EGLContext c = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ca); if (!c || !eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, c)) return 2;
  FUNCS(L)
  void (*Begin)(GLenum) = (void*)eglGetProcAddress("glBegin"); V End = (V)eglGetProcAddress("glEnd");
  F4 TexCoord4fv = (F4)eglGetProcAddress("glTexCoord4fv"); F2 Vertex2f = (F2)eglGetProcAddress("glVertex2f");
  void (*GenTextures)(GLsizei, GLuint*) = (void*)eglGetProcAddress("glGenTextures"); void (*BindTexture)(GLenum, GLuint) = (void*)eglGetProcAddress("glBindTexture");
  void (*TexParameteri)(GLenum, GLenum, GLint) = (void*)eglGetProcAddress("glTexParameteri"); void (*Viewport)(GLint,GLint,GLsizei,GLsizei) = (void*)eglGetProcAddress("glViewport");
  void (*ClearColor)(float,float,float,float) = (void*)eglGetProcAddress("glClearColor"); E1 Clear = (E1)eglGetProcAddress("glClear");
  void (*ReadPixels)(GLint,GLint,GLsizei,GLsizei,GLenum,GLenum,void*) = (void*)eglGetProcAddress("glReadPixels");
  printf("%s\n", ((const char *(*)(GLenum))eglGetProcAddress("glGetString"))(GL_VERSION));
  GLuint tex; GenTextures(1, &tex); BindTexture(GL_TEXTURE_CUBE_MAP_ARRAY, tex);
  glTexImage3D_(GL_TEXTURE_CUBE_MAP_ARRAY, 0, GL_RGBA8, 8, 8, 18, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
  TexParameteri(GL_TEXTURE_CUBE_MAP_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST); TexParameteri(GL_TEXTURE_CUBE_MAP_ARRAY, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
  GLuint fb; glGenFramebuffers_(1, &fb); glBindFramebuffer_(GL_FRAMEBUFFER, fb); Viewport(0, 0, 8, 8);
  void (*Color4f)(float,float,float,float) = (void*)eglGetProcAddress("glColor4f");
  int fixedfn = getenv("FIXEDFN") != NULL;
  for (int i = 0; i < 18; i++) { glFramebufferTextureLayer_(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, tex, 0, i);
    if (fixedfn) { Color4f((i&1)?1:0, (i&2)?1:0, (i&4)?1:0, (i+1)/32.f); Begin(GL_QUADS); Vertex2f(-1,-1); Vertex2f(1,-1); Vertex2f(1,1); Vertex2f(-1,1); End(); }
    else { ClearColor((i&1)?1:0, (i&2)?1:0, (i&4)?1:0, (i+1)/32.f); Clear(GL_COLOR_BUFFER_BIT); } }
  const char *fs = "#version 130\n#extension GL_ARB_texture_cube_map_array : enable\nuniform samplerCubeArray tex;\nvoid main(){ gl_FragColor = texture(tex, gl_TexCoord[0]); }\n";
  GLuint s = glCreateShader_(GL_FRAGMENT_SHADER); glShaderSource_(s, 1, &fs, NULL); glCompileShader_(s); GLuint p = glCreateProgram_(); glAttachShader_(p, s); glLinkProgram_(p);
  GLuint rb; glGenRenderbuffers_(1, &rb); glBindRenderbuffer_(GL_RENDERBUFFER, rb); glRenderbufferStorage_(GL_RENDERBUFFER, GL_RGBA8, 4, 4);
  GLuint fb2; glGenFramebuffers_(1, &fb2); glBindFramebuffer_(GL_FRAMEBUFFER, fb2); glFramebufferRenderbuffer_(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb); Viewport(0, 0, 4, 4);
  glUseProgram_(p); glUniform1i_(glGetUniformLocation_(p, "tex"), 0);
  static const float dir[6][3] = {{1,0,0},{-1,0,0},{0,1,0},{0,-1,0},{0,0,1},{0,0,-1}};
  int bad = 0;
  for (int i = 0; i < 18; i++) {
    float tc[4] = {dir[i%6][0], dir[i%6][1], dir[i%6][2], (float)(i/6)};
    float vx[4][2] = {{-1,-1},{1,-1},{1,1},{-1,1}};
    if (tris) { int o[6] = {0,1,2,0,2,3}; Begin(GL_TRIANGLES); for (int k = 0; k < 6; k++) { TexCoord4fv(tc); Vertex2f(vx[o[k]][0], vx[o[k]][1]); } End(); }
    else { Begin(GL_QUADS); for (int k = 0; k < 4; k++) { TexCoord4fv(tc); Vertex2f(vx[k][0], vx[k][1]); } End(); }
    unsigned char px[4]; ReadPixels(2, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    int e0 = (i&1)?255:0, e1 = (i&2)?255:0, e2 = (i&4)?255:0, e3 = (int)((i+1)/32.f*255+0.5f);
    int ok = abs(px[0]-e0)<3 && abs(px[1]-e1)<3 && abs(px[2]-e2)<3 && abs(px[3]-e3)<3; bad += !ok;
    printf("%2d: %3d %3d %3d %3d%s (exp %3d %3d %3d %3d)\n", i, px[0], px[1], px[2], px[3], ok ? "" : " *", e0, e1, e2, e3);
  }
  printf("%s %s: bad %d\n", tris ? "triangles" : "quads", bad ? "FAIL" : "PASS", bad);
  return bad != 0;
}
