// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Compute-only companion to tesgs.c: each invocation fills a private ivec4[N] in a loop (a
// dynamic index, so the array lives in KosmicKrisp's scratch), then copies it to an SSBO.
// cs <N> <local_size_x> <invocations> reports which invocations and elements came back wrong.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

#define F(type, name) static type p##name
F(PFNGLCREATESHADERPROC, CreateShader); F(PFNGLSHADERSOURCEPROC, ShaderSource);
F(PFNGLCOMPILESHADERPROC, CompileShader); F(PFNGLGETSHADERIVPROC, GetShaderiv);
F(PFNGLGETSHADERINFOLOGPROC, GetShaderInfoLog); F(PFNGLCREATEPROGRAMPROC, CreateProgram);
F(PFNGLATTACHSHADERPROC, AttachShader); F(PFNGLLINKPROGRAMPROC, LinkProgram);
F(PFNGLGETPROGRAMIVPROC, GetProgramiv); F(PFNGLUSEPROGRAMPROC, UseProgram);
F(PFNGLGENBUFFERSPROC, GenBuffers); F(PFNGLBINDBUFFERPROC, BindBuffer);
F(PFNGLBUFFERDATAPROC, BufferData); F(PFNGLBINDBUFFERBASEPROC, BindBufferBase);
F(PFNGLDISPATCHCOMPUTEPROC, DispatchCompute); F(PFNGLMEMORYBARRIERPROC, MemoryBarrier);
F(PFNGLGETBUFFERSUBDATAPROC, GetBufferSubData); F(PFNGLGETERRORPROC, GetError);
F(PFNGLDISPATCHCOMPUTEINDIRECTPROC, DispatchComputeIndirect);

#define LOAD(name)                                                                       \
    do {                                                                                 \
        *(void **)&p##name = (void *)eglGetProcAddress("gl" #name);                      \
        if (!p##name) { fprintf(stderr, "missing gl" #name "\n"); return 3; }            \
    } while (0)

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 25;
    int local = argc > 2 ? atoi(argv[2]) : 64;
    int count = argc > 3 ? atoi(argv[3]) : 6;
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
    LOAD(GetProgramiv); LOAD(UseProgram); LOAD(GenBuffers); LOAD(BindBuffer); LOAD(BufferData);
    LOAD(BindBufferBase); LOAD(DispatchCompute); LOAD(MemoryBarrier); LOAD(GetBufferSubData);
    LOAD(GetError); LOAD(DispatchComputeIndirect);

    char src[2048];
    snprintf(src, sizeof src,
             "#version 430\nlayout(local_size_x = %d) in;\n"
             "layout(std430, binding = 0) buffer Out { ivec4 o[]; };\n"
             "uniform int count;\n"
             "void main(){\n uint id = gl_GlobalInvocationID.x;\n if (id >= %du) return;\n"
             " ivec4 f[%d];\n"
             " for (int i = 0; i < f.length(); i++)\n"
             "  f[i] = ivec4(i*4, i*4+1, i*4+2, i*4+3) + 1000 * int(id);\n"
             " for (int i = 0; i < f.length(); i++) o[id * %du + uint(i)] = f[i];\n}\n",
             local, count, n, n);
    GLuint s = pCreateShader(GL_COMPUTE_SHADER);
    const char *p = src;
    pShaderSource(s, 1, &p, NULL);
    pCompileShader(s);
    GLint ok;
    pGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[4096];
        pGetShaderInfoLog(s, sizeof log, NULL, log);
        fprintf(stderr, "cs: %s\n", log);
        return 2;
    }
    GLuint prog = pCreateProgram();
    pAttachShader(prog, s);
    pLinkProgram(prog);
    pUseProgram(prog);

    size_t bytes = (size_t)count * n * 16;
    int *init = malloc(bytes);
    for (size_t i = 0; i < bytes / 4; i++)
        init[i] = -7;
    GLuint buf;
    pGenBuffers(1, &buf);
    pBindBuffer(GL_SHADER_STORAGE_BUFFER, buf);
    pBufferData(GL_SHADER_STORAGE_BUFFER, bytes, init, GL_DYNAMIC_READ);
    pBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buf);
    if (getenv("INDIRECT")) {
        // INDIRECT=1 dispatches the same workgroups through glDispatchComputeIndirect.
        GLuint groups[3] = {(GLuint)((count + local - 1) / local), 1, 1}, ind;
        pGenBuffers(1, &ind);
        pBindBuffer(GL_DISPATCH_INDIRECT_BUFFER, ind);
        pBufferData(GL_DISPATCH_INDIRECT_BUFFER, sizeof groups, groups, GL_STATIC_DRAW);
        pDispatchComputeIndirect(0);
        pBindBuffer(GL_SHADER_STORAGE_BUFFER, buf);
    } else {
        pDispatchCompute((count + local - 1) / local, 1, 1);
    }
    pMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT);
    int *out = malloc(bytes);
    pGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, bytes, out);

    int bad = 0;
    for (int id = 0; id < count; id++) {
        int first = -1, nbad = 0;
        for (int i = 0; i < n; i++)
            for (int c = 0; c < 4; c++)
                if (out[(id * n + i) * 4 + c] != i * 4 + c + 1000 * id) {
                    if (first < 0)
                        first = i;
                    nbad++;
                }
        if (nbad) {
            bad++;
            printf("  invocation %d: %d wrong components, first wrong element %d (got .x=%d)\n",
                   id, nbad, first, out[(id * n + first) * 4]);
        }
    }
    printf("N=%d local=%d invocations=%d: %s (gl error 0x%x)\n", n, local, count,
           bad ? "WRONG" : "ok", pGetError());
    return 0;
}
