// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Pixel check of KHR_blend_equation_advanced on host zink-on-KK.
//
// For each mode: clear an RGBA8 FBO to an opaque destination colour, draw an opaque source quad
// with that blend equation, and compare the read-back pixel with the spec's formula (with both
// alphas 1 the premultiplied terms reduce to f(Cs, Cd)). Exit 0 only if every mode matches
// within 2/255; exit 77 if the context does not offer the extension.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <GLES2/gl2ext.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

#define F(type, name) static type p##name
F(PFNGLGETSTRINGIPROC, GetStringi); F(PFNGLGETINTEGERVPROC, GetIntegerv);
F(PFNGLGENFRAMEBUFFERSPROC, GenFramebuffers); F(PFNGLBINDFRAMEBUFFERPROC, BindFramebuffer);
F(PFNGLGENTEXTURESPROC, GenTextures); F(PFNGLBINDTEXTUREPROC, BindTexture);
F(PFNGLTEXSTORAGE2DPROC, TexStorage2D); F(PFNGLFRAMEBUFFERTEXTURE2DPROC, FramebufferTexture2D);
F(PFNGLCHECKFRAMEBUFFERSTATUSPROC, CheckFramebufferStatus); F(PFNGLVIEWPORTPROC, Viewport);
F(PFNGLCLEARCOLORPROC, ClearColor); F(PFNGLCLEARPROC, Clear); F(PFNGLENABLEPROC, Enable);
F(PFNGLDISABLEPROC, Disable); F(PFNGLBLENDEQUATIONPROC, BlendEquation);
F(PFNGLCREATESHADERPROC, CreateShader); F(PFNGLSHADERSOURCEPROC, ShaderSource);
F(PFNGLCOMPILESHADERPROC, CompileShader); F(PFNGLGETSHADERIVPROC, GetShaderiv);
F(PFNGLGETSHADERINFOLOGPROC, GetShaderInfoLog); F(PFNGLCREATEPROGRAMPROC, CreateProgram);
F(PFNGLATTACHSHADERPROC, AttachShader); F(PFNGLLINKPROGRAMPROC, LinkProgram);
F(PFNGLGETPROGRAMIVPROC, GetProgramiv); F(PFNGLUSEPROGRAMPROC, UseProgram);
F(PFNGLGETUNIFORMLOCATIONPROC, GetUniformLocation); F(PFNGLUNIFORM4FPROC, Uniform4f);
F(PFNGLDRAWARRAYSPROC, DrawArrays); F(PFNGLREADPIXELSPROC, ReadPixels);
F(PFNGLGETERRORPROC, GetError); F(PFNGLGENVERTEXARRAYSPROC, GenVertexArrays);
F(PFNGLBINDVERTEXARRAYPROC, BindVertexArray);
static void (*pBlendBarrier)(void);

#define LOAD(name)                                                                       \
    do {                                                                                 \
        *(void **)&p##name = (void *)eglGetProcAddress("gl" #name);                      \
        if (!p##name) { fprintf(stderr, "missing gl" #name "\n"); return 3; }            \
    } while (0)

static float sat(float x) { return x < 0 ? 0 : x > 1 ? 1 : x; }
static float multiply(float s, float d) { return s * d; }
static float screen(float s, float d) { return s + d - s * d; }
static float overlay(float s, float d) { return d <= 0.5f ? 2 * s * d : 1 - 2 * (1 - s) * (1 - d); }
static float darken(float s, float d) { return fminf(s, d); }
static float lighten(float s, float d) { return fmaxf(s, d); }
static float hardlight(float s, float d) { return s <= 0.5f ? 2 * s * d : 1 - 2 * (1 - s) * (1 - d); }
static float difference(float s, float d) { return fabsf(s - d); }
static float exclusion(float s, float d) { return s + d - 2 * s * d; }

static const struct {
    const char *name;
    GLenum eq;
    float (*f)(float, float);
} modes[] = {
    {"MULTIPLY", GL_MULTIPLY_KHR, multiply},   {"SCREEN", GL_SCREEN_KHR, screen},
    {"OVERLAY", GL_OVERLAY_KHR, overlay},      {"DARKEN", GL_DARKEN_KHR, darken},
    {"LIGHTEN", GL_LIGHTEN_KHR, lighten},      {"HARDLIGHT", GL_HARDLIGHT_KHR, hardlight},
    {"DIFFERENCE", GL_DIFFERENCE_KHR, difference}, {"EXCLUSION", GL_EXCLUSION_KHR, exclusion},
};

static const char *VS = "#version 310 es\n"
                        "void main(){ vec2 p = vec2(gl_VertexID & 1, gl_VertexID >> 1) * 4.0 - 1.0;"
                        " gl_Position = vec4(p, 0.0, 1.0); }\n";
static const char *FS = "#version 310 es\n"
                        "#extension GL_KHR_blend_equation_advanced : require\n"
                        "precision mediump float;\n"
                        "layout(blend_support_all_equations) out;\n"
                        "uniform vec4 src; layout(location = 0) out vec4 o;\n"
                        "void main(){ o = src; }\n";

static GLuint shader(GLenum t, const char *src) {
    GLuint s = pCreateShader(t);
    pShaderSource(s, 1, &src, NULL);
    pCompileShader(s);
    GLint ok;
    pGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[2048];
        pGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "shader: %s\n", log);
    }
    return s;
}

int main(void) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC getDpy =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = getDpy(EGL_PLATFORM_SURFACELESS_MESA, (void *)0, NULL);
    EGLint maj, min;
    if (!eglInitialize(dpy, &maj, &min) || !eglBindAPI(EGL_OPENGL_ES_API))
        return 2;
    const EGLint cfga[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE,
                           EGL_OPENGL_ES3_BIT, EGL_NONE};
    EGLConfig cfg;
    EGLint n;
    const EGLint ca[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE};
    if (!eglChooseConfig(dpy, cfga, &cfg, 1, &n) || !n)
        return 2;
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ca);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
        return 2;

    LOAD(GetStringi); LOAD(GetIntegerv); LOAD(GenFramebuffers); LOAD(BindFramebuffer);
    LOAD(GenTextures); LOAD(BindTexture); LOAD(TexStorage2D); LOAD(FramebufferTexture2D);
    LOAD(CheckFramebufferStatus); LOAD(Viewport); LOAD(ClearColor); LOAD(Clear); LOAD(Enable);
    LOAD(Disable); LOAD(BlendEquation); LOAD(CreateShader); LOAD(ShaderSource);
    LOAD(CompileShader); LOAD(GetShaderiv); LOAD(GetShaderInfoLog); LOAD(CreateProgram);
    LOAD(AttachShader); LOAD(LinkProgram); LOAD(GetProgramiv); LOAD(UseProgram);
    LOAD(GetUniformLocation); LOAD(Uniform4f); LOAD(DrawArrays); LOAD(ReadPixels);
    LOAD(GetError); LOAD(GenVertexArrays); LOAD(BindVertexArray);

    GLint next = 0, has = 0;
    pGetIntegerv(GL_NUM_EXTENSIONS, &next);
    for (GLint i = 0; i < next; i++)
        has |= !strcmp((const char *)pGetStringi(GL_EXTENSIONS, i), "GL_KHR_blend_equation_advanced");
    if (!has) {
        printf("SKIP: GL_KHR_blend_equation_advanced not offered\n");
        return 77;
    }
    *(void **)&pBlendBarrier = (void *)eglGetProcAddress("glBlendBarrierKHR");

    GLuint tex, fbo, vao;
    pGenTextures(1, &tex);
    pBindTexture(GL_TEXTURE_2D, tex);
    pTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 16, 16);
    pGenFramebuffers(1, &fbo);
    pBindFramebuffer(GL_FRAMEBUFFER, fbo);
    pFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    if (pCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    pViewport(0, 0, 16, 16);
    pGenVertexArrays(1, &vao);
    pBindVertexArray(vao);

    GLuint prog = pCreateProgram();
    pAttachShader(prog, shader(GL_VERTEX_SHADER, VS));
    pAttachShader(prog, shader(GL_FRAGMENT_SHADER, FS));
    pLinkProgram(prog);
    GLint linked;
    pGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked)
        return 2;
    pUseProgram(prog);
    GLint usrc = pGetUniformLocation(prog, "src");

    const float s[3] = {0.25f, 0.75f, 0.60f}, d[3] = {0.80f, 0.30f, 0.50f};
    int bad = 0;
    for (size_t m = 0; m < sizeof modes / sizeof modes[0]; m++) {
        pDisable(GL_BLEND);
        pClearColor(d[0], d[1], d[2], 1);
        pClear(GL_COLOR_BUFFER_BIT);
        pEnable(GL_BLEND);
        pBlendEquation(modes[m].eq);
        if (pBlendBarrier)
            pBlendBarrier();
        pUniform4f(usrc, s[0], s[1], s[2], 1);
        pDrawArrays(GL_TRIANGLES, 0, 3);
        unsigned char px[4];
        pReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        GLenum err = pGetError();
        int ok = err == GL_NO_ERROR;
        float want[3];
        for (int c = 0; c < 3; c++) {
            // Each operand passes through an 8-bit framebuffer or uniform first.
            float dq = roundf(d[c] * 255) / 255;
            want[c] = sat(modes[m].f(s[c], dq));
            ok &= fabsf(px[c] / 255.0f - want[c]) <= 2.0f / 255;
        }
        bad += !ok;
        printf("%-4s %-10s got %3d %3d %3d  want %3.0f %3.0f %3.0f%s\n", ok ? "ok" : "FAIL",
               modes[m].name, px[0], px[1], px[2], want[0] * 255, want[1] * 255, want[2] * 255,
               err ? "  (GL error)" : "");
    }
    printf("%d of %zu modes wrong\n", bad, sizeof modes / sizeof modes[0]);
    return bad ? 1 : 0;
}
