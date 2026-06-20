// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Desktop-GL (core profile) textured-triangle probe for zink-on-KosmicKrisp via surfaceless EGL.
// Goes beyond eglprobe.c's glClear: confirms (1) a real DESKTOP GL context (not GLES) comes up on
// zink→KK→Metal and reports its max version, and (2) the full pipeline works — VAO/VBO, a compiled
// vertex+fragment shader program, TEXTURE sampling, and rasterization — by drawing a centered
// triangle that samples a known-color texture, then reading pixels INSIDE (expect texture color)
// and OUTSIDE (expect clear color). That inside≠outside + correct-color check is what vrend needs.
//
// No libGL to link (glx/glvnd disabled): bind EGL_OPENGL_API, create the context, and load every GL
// entry point via eglGetProcAddress. Build/run via run-probe.sh glprobe.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#define GL_GLEXT_PROTOTYPES 0
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define EGLCHECK(cond, msg)                                                                \
    do { if (!(cond)) { fprintf(stderr, "FAIL: %s (egl 0x%x)\n", msg, eglGetError()); return 2; } } while (0)

// --- minimal GL loader via eglGetProcAddress ---
static PFNGLGETSTRINGPROC pglGetString;
static PFNGLCLEARCOLORPROC pglClearColor;
static PFNGLCLEARPROC pglClear;
static PFNGLVIEWPORTPROC pglViewport;
static PFNGLGENVERTEXARRAYSPROC pglGenVertexArrays;
static PFNGLBINDVERTEXARRAYPROC pglBindVertexArray;
static PFNGLGENBUFFERSPROC pglGenBuffers;
static PFNGLBINDBUFFERPROC pglBindBuffer;
static PFNGLBUFFERDATAPROC pglBufferData;
static PFNGLCREATESHADERPROC pglCreateShader;
static PFNGLSHADERSOURCEPROC pglShaderSource;
static PFNGLCOMPILESHADERPROC pglCompileShader;
static PFNGLGETSHADERIVPROC pglGetShaderiv;
static PFNGLGETSHADERINFOLOGPROC pglGetShaderInfoLog;
static PFNGLCREATEPROGRAMPROC pglCreateProgram;
static PFNGLATTACHSHADERPROC pglAttachShader;
static PFNGLLINKPROGRAMPROC pglLinkProgram;
static PFNGLGETPROGRAMIVPROC pglGetProgramiv;
static PFNGLUSEPROGRAMPROC pglUseProgram;
static PFNGLGETATTRIBLOCATIONPROC pglGetAttribLocation;
static PFNGLBINDATTRIBLOCATIONPROC pglBindAttribLocation;
static PFNGLENABLEVERTEXATTRIBARRAYPROC pglEnableVertexAttribArray;
static PFNGLVERTEXATTRIBPOINTERPROC pglVertexAttribPointer;
static PFNGLGENTEXTURESPROC pglGenTextures;
static PFNGLBINDTEXTUREPROC pglBindTexture;
static PFNGLTEXIMAGE2DPROC pglTexImage2D;
static PFNGLTEXPARAMETERIPROC pglTexParameteri;
static PFNGLGETUNIFORMLOCATIONPROC pglGetUniformLocation;
static PFNGLUNIFORM1IPROC pglUniform1i;
static PFNGLDRAWARRAYSPROC pglDrawArrays;
static PFNGLREADPIXELSPROC pglReadPixels;
static PFNGLFINISHPROC pglFinish;

#define LOAD(p, name)                                                                      \
    do { p = (void *)eglGetProcAddress(name); if (!p) { fprintf(stderr, "missing GL func: %s\n", name); return 3; } } while (0)

static int load_gl(void) {
    LOAD(pglGetString, "glGetString");
    LOAD(pglClearColor, "glClearColor"); LOAD(pglClear, "glClear"); LOAD(pglViewport, "glViewport");
    LOAD(pglGenVertexArrays, "glGenVertexArrays"); LOAD(pglBindVertexArray, "glBindVertexArray");
    LOAD(pglGenBuffers, "glGenBuffers"); LOAD(pglBindBuffer, "glBindBuffer"); LOAD(pglBufferData, "glBufferData");
    LOAD(pglCreateShader, "glCreateShader"); LOAD(pglShaderSource, "glShaderSource");
    LOAD(pglCompileShader, "glCompileShader"); LOAD(pglGetShaderiv, "glGetShaderiv");
    LOAD(pglGetShaderInfoLog, "glGetShaderInfoLog"); LOAD(pglCreateProgram, "glCreateProgram");
    LOAD(pglAttachShader, "glAttachShader"); LOAD(pglLinkProgram, "glLinkProgram");
    LOAD(pglGetProgramiv, "glGetProgramiv"); LOAD(pglUseProgram, "glUseProgram");
    LOAD(pglGetAttribLocation, "glGetAttribLocation");
    LOAD(pglBindAttribLocation, "glBindAttribLocation");
    LOAD(pglEnableVertexAttribArray, "glEnableVertexAttribArray");
    LOAD(pglVertexAttribPointer, "glVertexAttribPointer");
    LOAD(pglGenTextures, "glGenTextures"); LOAD(pglBindTexture, "glBindTexture");
    LOAD(pglTexImage2D, "glTexImage2D"); LOAD(pglTexParameteri, "glTexParameteri");
    LOAD(pglGetUniformLocation, "glGetUniformLocation"); LOAD(pglUniform1i, "glUniform1i");
    LOAD(pglDrawArrays, "glDrawArrays"); LOAD(pglReadPixels, "glReadPixels"); LOAD(pglFinish, "glFinish");
    return 0;
}

static GLuint compile(GLenum type, const char *src) {
    GLuint s = pglCreateShader(type);
    pglShaderSource(s, 1, &src, NULL);
    pglCompileShader(s);
    GLint ok = 0; pglGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024]; pglGetShaderInfoLog(s, sizeof log, NULL, log); fprintf(stderr, "shader: %s\n", log); }
    return s;
}

// GLSL 1.40 (GL 3.1) — the ceiling zink-on-KK currently grants (no core profile, no 3.3+).
// No layout(location=) (that's 3.3+); attrib locations bound explicitly before link.
static const char *VS =
    "#version 140\n"
    "in vec2 pos;\n"
    "in vec2 uv;\n"
    "out vec2 vuv;\n"
    "void main(){ vuv=uv; gl_Position=vec4(pos,0.0,1.0); }\n";
static const char *FS =
    "#version 140\n"
    "in vec2 vuv; out vec4 frag; uniform sampler2D tex;\n"
    "void main(){ frag = texture(tex, vuv); }\n";

int main(void) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC getDpy =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = getDpy ? getDpy(EGL_PLATFORM_SURFACELESS_MESA, (void *)0, NULL)
                            : eglGetDisplay(EGL_DEFAULT_DISPLAY);
    EGLCHECK(dpy != EGL_NO_DISPLAY, "get display");
    EGLint maj, min; EGLCHECK(eglInitialize(dpy, &maj, &min), "eglInitialize");
    EGLCHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI(GL)"); // desktop GL, NOT ES

    const EGLint cfga[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                           EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE};
    EGLConfig cfg; EGLint n = 0;
    EGLCHECK(eglChooseConfig(dpy, cfga, &cfg, 1, &n) && n > 0, "eglChooseConfig(GL)");

    // Try a ladder of desktop-GL context requests; report which one zink-on-KK actually grants.
    struct { int maj, min, profile; const char *desc; } ladder[] = {
        {4, 1, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, "4.1 core"},
        {3, 3, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, "3.3 core"},
        {3, 2, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, "3.2 core"},
        {4, 1, EGL_CONTEXT_OPENGL_COMPATIBILITY_PROFILE_BIT, "4.1 compat"},
        {3, 0, 0, "3.0"}, {2, 1, 0, "2.1"}, {0, 0, 0, "default(bare)"},
    };
    EGLContext ctx = EGL_NO_CONTEXT; const char *got = "none";
    for (size_t i = 0; i < sizeof ladder / sizeof ladder[0] && ctx == EGL_NO_CONTEXT; i++) {
        EGLint ca[7]; int k = 0;
        if (ladder[i].maj) { ca[k++]=EGL_CONTEXT_MAJOR_VERSION; ca[k++]=ladder[i].maj;
                             ca[k++]=EGL_CONTEXT_MINOR_VERSION; ca[k++]=ladder[i].min; }
        if (ladder[i].profile) { ca[k++]=EGL_CONTEXT_OPENGL_PROFILE_MASK; ca[k++]=ladder[i].profile; }
        ca[k] = EGL_NONE;
        ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ca);
        if (ctx != EGL_NO_CONTEXT) got = ladder[i].desc;
        else fprintf(stderr, "  ctx %-12s -> egl 0x%x\n", ladder[i].desc, eglGetError());
    }
    printf("ctx granted : %s\n", got);
    EGLCHECK(ctx != EGL_NO_CONTEXT, "eglCreateContext(GL, any)");
    const EGLint pba[] = {EGL_WIDTH, 64, EGL_HEIGHT, 64, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, pba);
    EGLCHECK(surf != EGL_NO_SURFACE, "pbuffer");
    EGLCHECK(eglMakeCurrent(dpy, surf, surf, ctx), "makeCurrent");

    if (load_gl()) return 3;
    const char *ver = (const char *)pglGetString(GL_VERSION);
    printf("GL_RENDERER : %s\n", (const char *)pglGetString(GL_RENDERER));
    printf("GL_VERSION  : %s\n", ver);
    int is_es = ver && strstr(ver, "ES") != NULL;
    printf("desktop GL  : %s\n", is_es ? "NO (got ES)" : "YES");

    pglViewport(0, 0, 64, 64);

    // Known-color texture (magenta), 2x2.
    unsigned char tex[2 * 2 * 4];
    for (int i = 0; i < 4; i++) { tex[i*4+0]=255; tex[i*4+1]=0; tex[i*4+2]=255; tex[i*4+3]=255; }
    GLuint t; pglGenTextures(1, &t); pglBindTexture(GL_TEXTURE_2D, t);
    pglTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    pglTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    pglTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 2, 2, 0, GL_RGBA, GL_UNSIGNED_BYTE, tex);

    GLuint prog = pglCreateProgram();
    pglAttachShader(prog, compile(GL_VERTEX_SHADER, VS));
    pglAttachShader(prog, compile(GL_FRAGMENT_SHADER, FS));
    pglBindAttribLocation(prog, 0, "pos"); // GLSL 1.40 has no layout(location=)
    pglBindAttribLocation(prog, 1, "uv");
    pglLinkProgram(prog);
    GLint linked = 0; pglGetProgramiv(prog, GL_LINK_STATUS, &linked);
    EGLCHECK(linked, "link program");
    pglUseProgram(prog);
    pglUniform1i(pglGetUniformLocation(prog, "tex"), 0);

    // Centered triangle (covers the middle, leaves corners as clear color).
    const float verts[] = { /* pos */ -0.5f,-0.5f, /* uv */ 0,0,
                                        0.5f,-0.5f,            1,0,
                                        0.0f, 0.5f,            0.5f,1 };
    GLuint vao, vbo; pglGenVertexArrays(1, &vao); pglBindVertexArray(vao);
    pglGenBuffers(1, &vbo); pglBindBuffer(GL_ARRAY_BUFFER, vbo);
    pglBufferData(GL_ARRAY_BUFFER, sizeof verts, verts, GL_STATIC_DRAW);
    pglEnableVertexAttribArray(0); pglVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 4*sizeof(float), (void*)0);
    pglEnableVertexAttribArray(1); pglVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 4*sizeof(float), (void*)(2*sizeof(float)));

    pglClearColor(0.0f, 0.0f, 0.0f, 1.0f); // black background
    pglClear(GL_COLOR_BUFFER_BIT);
    pglDrawArrays(GL_TRIANGLES, 0, 3);
    pglFinish();

    unsigned char in[4]={0}, out[4]={0};
    pglReadPixels(28, 22, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, in);   // inside the triangle
    pglReadPixels(1, 62, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, out);   // top-left corner (outside)
    printf("inside  px  : (%u,%u,%u,%u)  expect ~magenta (255,0,255)\n", in[0],in[1],in[2],in[3]);
    printf("outside px  : (%u,%u,%u,%u)  expect black (0,0,0)\n", out[0],out[1],out[2],out[3]);

    int tex_ok  = in[0]>200 && in[1]<40 && in[2]>200;
    int rast_ok = (out[0]+out[1]+out[2]) < 40;       // corner stayed background
    int ok = !is_es && tex_ok && rast_ok;
    printf("RESULT      : %s\n", ok ? "PASS — desktop GL textured-triangle render on zink-on-KK"
            : (is_es ? "got GLES, not desktop GL" : (tex_ok ? "rasterization wrong" : "texture sample wrong")));

    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglTerminate(dpy);
    return ok ? 0 : 1;
}
