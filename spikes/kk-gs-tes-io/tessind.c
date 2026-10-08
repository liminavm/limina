// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Indirect tessellated draw whose vertex shader fills a private ivec4[N] in a loop (so the
// array spills to KosmicKrisp's scratch) and copies it to an SSBO, counting its invocations.
// KosmicKrisp runs that vertex shader as a compute kernel on an indirect grid.
//   tessind <N> <vertices> [direct]
// reports the vertex-shader invocation count and which vertices' arrays came back wrong.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define F(type, name) static type p##name
F(PFNGLCREATESHADERPROC, CreateShader); F(PFNGLSHADERSOURCEPROC, ShaderSource);
F(PFNGLCOMPILESHADERPROC, CompileShader); F(PFNGLGETSHADERIVPROC, GetShaderiv);
F(PFNGLGETSHADERINFOLOGPROC, GetShaderInfoLog); F(PFNGLCREATEPROGRAMPROC, CreateProgram);
F(PFNGLATTACHSHADERPROC, AttachShader); F(PFNGLLINKPROGRAMPROC, LinkProgram);
F(PFNGLGETPROGRAMIVPROC, GetProgramiv); F(PFNGLGETPROGRAMINFOLOGPROC, GetProgramInfoLog);
F(PFNGLUSEPROGRAMPROC, UseProgram); F(PFNGLGENVERTEXARRAYSPROC, GenVertexArrays);
F(PFNGLBINDVERTEXARRAYPROC, BindVertexArray); F(PFNGLGENBUFFERSPROC, GenBuffers);
F(PFNGLBINDBUFFERPROC, BindBuffer); F(PFNGLBUFFERDATAPROC, BufferData);
F(PFNGLBINDBUFFERBASEPROC, BindBufferBase); F(PFNGLGETBUFFERSUBDATAPROC, GetBufferSubData);
F(PFNGLENABLEVERTEXATTRIBARRAYPROC, EnableVertexAttribArray);
F(PFNGLVERTEXATTRIBPOINTERPROC, VertexAttribPointer); F(PFNGLPATCHPARAMETERIPROC, PatchParameteri);
F(PFNGLDRAWARRAYSPROC, DrawArrays); F(PFNGLDRAWARRAYSINDIRECTPROC, DrawArraysIndirect);
F(PFNGLGENFRAMEBUFFERSPROC, GenFramebuffers); F(PFNGLBINDFRAMEBUFFERPROC, BindFramebuffer);
F(PFNGLGENRENDERBUFFERSPROC, GenRenderbuffers); F(PFNGLBINDRENDERBUFFERPROC, BindRenderbuffer);
F(PFNGLRENDERBUFFERSTORAGEPROC, RenderbufferStorage);
F(PFNGLFRAMEBUFFERRENDERBUFFERPROC, FramebufferRenderbuffer); F(PFNGLVIEWPORTPROC, Viewport);
F(PFNGLMEMORYBARRIERPROC, MemoryBarrier); F(PFNGLFINISHPROC, Finish);
F(PFNGLGETERRORPROC, GetError);

#define LOAD(name)                                                                       \
    do {                                                                                 \
        *(void **)&p##name = (void *)eglGetProcAddress("gl" #name);                      \
        if (!p##name) { fprintf(stderr, "missing gl" #name "\n"); return 3; }            \
    } while (0)

static GLuint shader(GLenum t, const char *src) {
    GLuint s = pCreateShader(t);
    pShaderSource(s, 1, &src, NULL);
    pCompileShader(s);
    GLint ok;
    pGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[4096];
        pGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "shader 0x%x: %s\n", t, log);
        exit(2);
    }
    return s;
}

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 25;
    int verts = argc > 2 ? atoi(argv[2]) : 300; // a multiple of 3, not of 64
    int direct = argc > 3 && !strcmp(argv[3], "direct");
    PFNEGLGETPLATFORMDISPLAYEXTPROC getDpy =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = getDpy(EGL_PLATFORM_SURFACELESS_MESA, (void *)0, NULL);
    EGLint maj, min;
    if (!eglInitialize(dpy, &maj, &min) || !eglBindAPI(EGL_OPENGL_API))
        return 2;
    const EGLint cfga[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                           EGL_NONE};
    EGLConfig cfg;
    EGLint nc;
    const EGLint ca[] = {EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 3,
                         EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                         EGL_NONE};
    if (!eglChooseConfig(dpy, cfga, &cfg, 1, &nc) || !nc)
        return 2;
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ca);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
        return 2;
    LOAD(CreateShader); LOAD(ShaderSource); LOAD(CompileShader); LOAD(GetShaderiv);
    LOAD(GetShaderInfoLog); LOAD(CreateProgram); LOAD(AttachShader); LOAD(LinkProgram);
    LOAD(GetProgramiv); LOAD(GetProgramInfoLog); LOAD(UseProgram); LOAD(GenVertexArrays);
    LOAD(BindVertexArray); LOAD(GenBuffers); LOAD(BindBuffer); LOAD(BufferData);
    LOAD(BindBufferBase); LOAD(GetBufferSubData); LOAD(EnableVertexAttribArray);
    LOAD(VertexAttribPointer); LOAD(PatchParameteri); LOAD(DrawArrays); LOAD(DrawArraysIndirect);
    LOAD(GenFramebuffers); LOAD(BindFramebuffer); LOAD(GenRenderbuffers); LOAD(BindRenderbuffer);
    LOAD(RenderbufferStorage); LOAD(FramebufferRenderbuffer); LOAD(Viewport);
    LOAD(MemoryBarrier); LOAD(Finish); LOAD(GetError);

    char vs[2048];
    snprintf(vs, sizeof vs,
             "#version 430\nin vec2 p;\n"
             "layout(std430, binding = 0) buffer Out { ivec4 o[]; };\n"
             "layout(std430, binding = 1) buffer Calls { uint calls; };\n"
             "void main(){\n ivec4 f[%d];\n"
             " for (int i = 0; i < f.length(); i++)\n"
             "  f[i] = ivec4(i*4, i*4+1, i*4+2, i*4+3) + 1000 * gl_VertexID;\n"
             " for (int i = 0; i < f.length(); i++) o[gl_VertexID * %d + i] = f[i];\n"
             " atomicAdd(calls, 1u);\n gl_Position = vec4(p, 0.0, 1.0);\n}\n",
             n, n);
    const char *tcs = "#version 430\nlayout(vertices = 3) out;\nvoid main(){\n"
                      " gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;\n"
                      " gl_TessLevelOuter = float[4](1.0, 1.0, 1.0, 0.0);\n"
                      " gl_TessLevelInner = float[2](0.0, 0.0);\n}\n";
    const char *tes = "#version 430\nlayout(triangles) in;\nvoid main(){\n"
                      " gl_Position = gl_in[0].gl_Position * gl_TessCoord[0]"
                      " + gl_in[1].gl_Position * gl_TessCoord[1]"
                      " + gl_in[2].gl_Position * gl_TessCoord[2];\n}\n";
    const char *fs = "#version 430\nout vec4 c; void main(){ c = vec4(0, 1, 0, 1); }\n";

    GLuint prog = pCreateProgram();
    pAttachShader(prog, shader(GL_VERTEX_SHADER, vs));
    pAttachShader(prog, shader(GL_TESS_CONTROL_SHADER, tcs));
    pAttachShader(prog, shader(GL_TESS_EVALUATION_SHADER, tes));
    pAttachShader(prog, shader(GL_FRAGMENT_SHADER, fs));
    pLinkProgram(prog);
    GLint linked;
    pGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[4096];
        pGetProgramInfoLog(prog, sizeof log, NULL, log);
        fprintf(stderr, "link: %s\n", log);
        return 2;
    }

    GLuint fbo, rb;
    pGenFramebuffers(1, &fbo);
    pBindFramebuffer(GL_FRAMEBUFFER, fbo);
    pGenRenderbuffers(1, &rb);
    pBindRenderbuffer(GL_RENDERBUFFER, rb);
    pRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 64, 64);
    pFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb);
    pViewport(0, 0, 64, 64);

    float *pos = calloc((size_t)verts * 2, sizeof(float));
    for (int i = 0; i < verts; i++) {
        pos[2 * i] = (i % 3 == 1) ? 1.0f : -1.0f;
        pos[2 * i + 1] = (i % 3 == 2) ? 1.0f : -1.0f;
    }
    GLuint vao, vbo;
    pGenVertexArrays(1, &vao);
    pBindVertexArray(vao);
    pGenBuffers(1, &vbo);
    pBindBuffer(GL_ARRAY_BUFFER, vbo);
    pBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)verts * 2 * sizeof(float), pos, GL_STATIC_DRAW);
    pEnableVertexAttribArray(0);
    pVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);

    size_t bytes = (size_t)verts * n * 16;
    int *init = malloc(bytes);
    for (size_t i = 0; i < bytes / 4; i++)
        init[i] = -7;
    GLuint out, calls;
    pGenBuffers(1, &out);
    pBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    pBufferData(GL_SHADER_STORAGE_BUFFER, (GLsizeiptr)bytes, init, GL_DYNAMIC_READ);
    pBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, out);
    GLuint zero = 0;
    pGenBuffers(1, &calls);
    pBindBuffer(GL_SHADER_STORAGE_BUFFER, calls);
    pBufferData(GL_SHADER_STORAGE_BUFFER, sizeof zero, &zero, GL_DYNAMIC_READ);
    pBindBufferBase(GL_SHADER_STORAGE_BUFFER, 1, calls);

    pUseProgram(prog);
    pPatchParameteri(GL_PATCH_VERTICES, 3);
    if (direct) {
        pDrawArrays(GL_PATCHES, 0, verts);
    } else {
        GLuint cmd[4] = {(GLuint)verts, 1, 0, 0}, ind;
        pGenBuffers(1, &ind);
        pBindBuffer(GL_DRAW_INDIRECT_BUFFER, ind);
        pBufferData(GL_DRAW_INDIRECT_BUFFER, sizeof cmd, cmd, GL_STATIC_DRAW);
        pDrawArraysIndirect(GL_PATCHES, 0);
    }
    pMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT);
    pFinish();

    GLuint ncalls = 0;
    pBindBuffer(GL_SHADER_STORAGE_BUFFER, calls);
    pGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof ncalls, &ncalls);
    int *res = malloc(bytes);
    pBindBuffer(GL_SHADER_STORAGE_BUFFER, out);
    pGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, (GLsizeiptr)bytes, res);

    int bad = 0, shown = 0;
    for (int v = 0; v < verts; v++) {
        int nbad = 0, first = -1;
        for (int i = 0; i < n; i++)
            for (int c = 0; c < 4; c++)
                if (res[(v * n + i) * 4 + c] != i * 4 + c + 1000 * v) {
                    if (first < 0)
                        first = i;
                    nbad++;
                }
        if (nbad) {
            bad++;
            if (shown++ < 6)
                printf("  vertex %d: %d wrong components, first wrong element %d (got .x=%d)\n",
                       v, nbad, first, res[(v * n + first) * 4]);
        }
    }
    printf("N=%d vertices=%d %s: vertex shader ran %u times, %d vertices wrong: %s (gl error 0x%x)\n",
           n, verts, direct ? "direct" : "indirect", ncalls, bad,
           (bad || ncalls != (GLuint)verts) ? "WRONG" : "ok", pGetError());
    return 0;
}
