// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Host repro of piglit's tes-gs-max-in-out-components on zink-on-KK, with the array length as
// a parameter: tesgs <N> passes N flat ivec4 per vertex (plus gl_Position) from the evaluation
// shader to a geometry shader, over two triangle patches, and reports what each half of the
// window shows: green (data intact), red (data wrong) or the clear colour (nothing drawn).
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
F(PFNGLENABLEVERTEXATTRIBARRAYPROC, EnableVertexAttribArray);
F(PFNGLVERTEXATTRIBPOINTERPROC, VertexAttribPointer); F(PFNGLPATCHPARAMETERIPROC, PatchParameteri);
F(PFNGLDRAWARRAYSPROC, DrawArrays); F(PFNGLCLEARCOLORPROC, ClearColor); F(PFNGLCLEARPROC, Clear);
F(PFNGLREADPIXELSPROC, ReadPixels); F(PFNGLGENFRAMEBUFFERSPROC, GenFramebuffers);
F(PFNGLBINDFRAMEBUFFERPROC, BindFramebuffer); F(PFNGLGENRENDERBUFFERSPROC, GenRenderbuffers);
F(PFNGLBINDRENDERBUFFERPROC, BindRenderbuffer); F(PFNGLRENDERBUFFERSTORAGEPROC, RenderbufferStorage);
F(PFNGLFRAMEBUFFERRENDERBUFFERPROC, FramebufferRenderbuffer); F(PFNGLVIEWPORTPROC, Viewport);
F(PFNGLGETERRORPROC, GetError); F(PFNGLGETINTEGERVPROC, GetIntegerv);
F(PFNGLGETSTRINGPROC, GetString);

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

static const char *what(const unsigned char *px) {
    if (px[0] < 30 && px[1] > 220 && px[2] < 30) return "green";
    if (px[0] > 220 && px[1] < 30 && px[2] < 30) return "RED";
    if (px[0] < 40 && px[1] < 40 && px[2] < 40) return "EMPTY";
    return "other";
}

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 24;
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
    const EGLint ca[] = {EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 0,
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
    LOAD(EnableVertexAttribArray); LOAD(VertexAttribPointer); LOAD(PatchParameteri);
    LOAD(DrawArrays); LOAD(ClearColor); LOAD(Clear); LOAD(ReadPixels); LOAD(GenFramebuffers);
    LOAD(BindFramebuffer); LOAD(GenRenderbuffers); LOAD(BindRenderbuffer);
    LOAD(RenderbufferStorage); LOAD(FramebufferRenderbuffer); LOAD(Viewport); LOAD(GetError);
    LOAD(GetIntegerv); LOAD(GetString);

    GLint tes_out = 0, gs_in = 0;
    pGetIntegerv(GL_MAX_TESS_EVALUATION_OUTPUT_COMPONENTS, &tes_out);
    pGetIntegerv(GL_MAX_GEOMETRY_INPUT_COMPONENTS, &gs_in);
    printf("%s | TES out %d, GS in %d components; N = %d ivec4 (+ position = %d vec4)\n",
           (const char *)pGetString(GL_RENDERER), tes_out, gs_in, n, n + 1);

    char vs[] = "#version 400\nin vec2 p; void main(){ gl_Position = vec4(p, 0.0, 1.0); }\n";
    char tcs[] = "#version 400\nlayout(vertices = 3) out;\nvoid main(){\n"
                 " gl_out[gl_InvocationID].gl_Position = gl_in[gl_InvocationID].gl_Position;\n"
                 " gl_TessLevelOuter = float[4](1.0, 1.0, 1.0, 0.0);\n"
                 " gl_TessLevelInner = float[2](0.0, 0.0);\n}\n";
    char tes[4096], gs[1536];
    // UNROLL=1 writes f[] as straight-line literal stores instead of a loop, so the evaluation
    // shader keeps no private array.
    char tes_body[2560] = "";
    if (getenv("UNROLL")) {
        for (int i = 0; i < n; i++) {
            size_t used = strlen(tes_body);
            snprintf(tes_body + used, sizeof tes_body - used,
                     " f[%d] = ivec4(%d, %d, %d, %d) + %s;\n", i, i * 4, i * 4 + 1, i * 4 + 2,
                     i * 4 + 3, getenv("GOT") ? "100 * gl_PrimitiveID" : "0");
        }
    } else {
        snprintf(tes_body, sizeof tes_body,
                 " for (int i = 0; i < f.length(); i++) f[i] = ivec4(i*4, i*4+1, i*4+2, i*4+3) + %s;\n",
                 getenv("GOT") ? "100 * gl_PrimitiveID" : "0");
    }
    snprintf(tes, sizeof tes,
             "#version 400\nlayout(triangles) in;\nflat out ivec4 f[%d];\nvoid main(){\n"
             " gl_Position = gl_in[0].gl_Position * gl_TessCoord[0]"
             " + gl_in[1].gl_Position * gl_TessCoord[1] + gl_in[2].gl_Position * gl_TessCoord[2];\n"
             "%s}\n",
             n, tes_body);
    // ONLY=<k> checks just element k, at a constant index, on every vertex: a sweep over k maps
    // which elements arrive wrong without the data-dependent loop of the DIAG variant (which
    // hung the GPU once -- avoid it).
    // LOOP=v fixes the vertex and loops over the elements; LOOP=i loops over the vertices of
    // one constant element (ONLY=<k>, default 0).
    const char *only = getenv("ONLY"), *loop = getenv("LOOP");
    if (loop)
        snprintf(gs, sizeof gs,
                 "#version 400\n#extension GL_ARB_arrays_of_arrays: require\n"
                 "layout(triangles) in;\nlayout(triangle_strip, max_vertices = 3) out;\n"
                 "flat in ivec4 f[3][%d];\nout vec4 color;\n#define LIM %s\nvoid main(){\n"
                 " const int k = %d;\n bool ok = true;\n%s"
                 " for (int i = 0; i < 3; i++) { gl_Position = gl_in[i].gl_Position;\n"
                 "  color = ok ? vec4(0, 1, 0, 1) : vec4(1, 0, 0, 1); EmitVertex(); }\n"
                 " EndPrimitive();\n}\n",
                 // LIM=<m> bounds the LOOP=v walk at m elements instead of the array length.
                 n, getenv("LIM") ? getenv("LIM") : "f[k].length()", only ? atoi(only) : 0,
                 loop[0] == 'v'
                     ? (getenv("NOSC") ? " for (int i = 0; i < LIM; i++)\n"
                                         "  ok = (f[k][i] == ivec4(i*4, i*4+1, i*4+2, i*4+3)) && ok;\n"
                                       : " for (int i = 0; i < LIM; i++)\n"
                                         "  ok = ok && f[k][i] == ivec4(i*4, i*4+1, i*4+2, i*4+3);\n")
                     : " for (int v = 0; v < 3; v++)\n"
                       "  ok = ok && f[v][k] == ivec4(k*4, k*4+1, k*4+2, k*4+3);\n");
    else if (only && getenv("GOT"))
        // GOT=1 with ONLY=<k>: the evaluation shader adds 100 * its primitive to every value,
        // and the GS reports f[0][k] -- picked out of a dynamic walk, with no data-dependent
        // branch -- as the colour, low byte of x/y/z.
        snprintf(gs, sizeof gs,
                 "#version 400\n#extension GL_ARB_arrays_of_arrays: require\n"
                 "layout(triangles) in;\nlayout(triangle_strip, max_vertices = 3) out;\n"
                 "flat in ivec4 f[3][%d];\nout vec4 color;\nvoid main(){\n"
                 // base is zero at run time but unknown to the compiler, so the read stays a
                 // dynamic index (a select over a constant-bound loop unrolls to literal reads).
                 " int base = f[0][0].x - 100 * gl_PrimitiveIDIn;\n"
                 " ivec4 got = f[0][%d + base];\n"
                 " for (int i = 0; i < 3; i++) { gl_Position = gl_in[i].gl_Position;\n"
                 "  color = vec4(float(got.%c & 255) / 255.0, float(got.y & 255) / 255.0,"
                 " float(got.z & 255) / 255.0, 1.0); EmitVertex(); }\n"
                 " EndPrimitive();\n}\n",
                 n, atoi(only), getenv("GOTW") ? 'w' : 'x');
    else if (only && getenv("KEEP"))
        // KEEP=1 with ONLY=<k>: check f[0][k] at a literal index while a dynamic walk keeps
        // every element live, so the evaluation shader and the buffer layout stay those of the
        // full test (a lone literal read lets the linker trim the other outputs away).
        snprintf(gs, sizeof gs,
                 "#version 400\n#extension GL_ARB_arrays_of_arrays: require\n"
                 "layout(triangles) in;\nlayout(triangle_strip, max_vertices = 3) out;\n"
                 "flat in ivec4 f[3][%d];\nout vec4 color;\nvoid main(){\n"
                 " bool ok = f[0][%d] == ivec4(%d, %d, %d, %d);\n"
                 " int s = 0;\n for (int i = 0; i < f[0].length(); i++) s += f[0][i].x;\n"
                 " if (s == 123456789) ok = false;\n"
                 " for (int i = 0; i < 3; i++) { gl_Position = gl_in[i].gl_Position;\n"
                 "  color = ok ? vec4(0, 1, 0, 1) : vec4(1, 0, 0, 1); EmitVertex(); }\n"
                 " EndPrimitive();\n}\n",
                 n, atoi(only), atoi(only) * 4, atoi(only) * 4 + 1, atoi(only) * 4 + 2,
                 atoi(only) * 4 + 3);
    else if (only && getenv("VERT"))
        // VERT=<v> with ONLY=<k>: check the single element f[v][k], both indices literal.
        snprintf(gs, sizeof gs,
                 "#version 400\n#extension GL_ARB_arrays_of_arrays: require\n"
                 "layout(triangles) in;\nlayout(triangle_strip, max_vertices = 3) out;\n"
                 "flat in ivec4 f[3][%d];\nout vec4 color;\nvoid main(){\n"
                 " bool ok = f[%d][%d] == ivec4(%d, %d, %d, %d);\n"
                 " for (int i = 0; i < 3; i++) { gl_Position = gl_in[i].gl_Position;\n"
                 "  color = ok ? vec4(0, 1, 0, 1) : vec4(1, 0, 0, 1); EmitVertex(); }\n"
                 " EndPrimitive();\n}\n",
                 n, atoi(getenv("VERT")), atoi(only), atoi(only) * 4, atoi(only) * 4 + 1,
                 atoi(only) * 4 + 2, atoi(only) * 4 + 3);
    else if (only)
        snprintf(gs, sizeof gs,
                 "#version 400\n#extension GL_ARB_arrays_of_arrays: require\n"
                 "layout(triangles) in;\nlayout(triangle_strip, max_vertices = 3) out;\n"
                 "flat in ivec4 f[3][%d];\nout vec4 color;\nvoid main(){\n"
                 " const int k = %d;\n bool ok = true;\n"
                 " for (int v = 0; v < 3; v++)\n"
                 "  ok = ok && f[v][k] == ivec4(k*4, k*4+1, k*4+2, k*4+3);\n"
                 " for (int i = 0; i < 3; i++) { gl_Position = gl_in[i].gl_Position;\n"
                 "  color = ok ? vec4(0, 1, 0, 1) : vec4(1, 0, 0, 1); EmitVertex(); }\n"
                 " EndPrimitive();\n}\n",
                 n, atoi(only));
    else
    snprintf(gs, sizeof gs,
             "#version 400\n#extension GL_ARB_arrays_of_arrays: require\n"
             "layout(triangles) in;\nlayout(triangle_strip, max_vertices = 3) out;\n"
             "flat in ivec4 f[3][%d];\nout vec4 color;\nvoid main(){\n bool ok = true;\n"
             " ivec3 bad = ivec3(0);\n"
             " for (int v = 0; v < 3; v++) for (int i = 0; i < f[v].length(); i++)\n"
             "  if (ok && f[v][i] != ivec4(i*4, i*4+1, i*4+2, i*4+3)) {\n"
             "   ok = false; bad = ivec3(i, v, f[v][i].x); }\n"
             " for (int i = 0; i < 3; i++) { gl_Position = gl_in[i].gl_Position;\n"
             "  color = ok ? vec4(0, 1, 0, 1) : %s; EmitVertex(); }\n"
             " EndPrimitive();\n}\n",
             n, getenv("DIAG") ? "vec4(float(bad.x) / 255.0, float(bad.y) / 255.0,"
                                 " float(bad.z & 255) / 255.0, 1.0)"
                               : "vec4(1, 0, 0, 1)");
    char fs[] = "#version 400\nin vec4 color; out vec4 o; void main(){ o = color; }\n";

    GLuint prog = pCreateProgram();
    pAttachShader(prog, shader(GL_VERTEX_SHADER, vs));
    pAttachShader(prog, shader(GL_TESS_CONTROL_SHADER, tcs));
    pAttachShader(prog, shader(GL_TESS_EVALUATION_SHADER, tes));
    pAttachShader(prog, shader(GL_GEOMETRY_SHADER, gs));
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

    const float verts[] = {-1, -1, 1, -1, -1, 1, -1, 1, 1, -1, 1, 1};
    GLuint vao, vbo;
    pGenVertexArrays(1, &vao);
    pBindVertexArray(vao);
    pGenBuffers(1, &vbo);
    pBindBuffer(GL_ARRAY_BUFFER, vbo);
    pBufferData(GL_ARRAY_BUFFER, sizeof verts, verts, GL_STATIC_DRAW);
    pEnableVertexAttribArray(0);
    pVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);

    pUseProgram(prog);
    pClearColor(0.1f, 0.1f, 0.1f, 0.1f);
    pClear(GL_COLOR_BUFFER_BIT);
    pPatchParameteri(GL_PATCH_VERTICES, 3);
    pDrawArrays(GL_PATCHES, 0, 6);

    unsigned char a[4], b[4];
    pReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, a);   // lower-left: patch 1
    pReadPixels(56, 56, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, b); // upper-right: patch 2
    if (getenv("DRAW2")) {
        // A second draw after the readback, so a driver-side heap dump taken when the next
        // command buffer starts using the heap sees the first draw's finished contents.
        pDrawArrays(GL_PATCHES, 0, 6);
        unsigned char c[4];
        pReadPixels(8, 8, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, c);
    }
    if (getenv("DIAG"))
        printf("first bad: patch 1 element %d vertex %d got .x=%d | patch 2 element %d vertex %d got .x=%d\n",
               a[0], a[1], a[2], b[0], b[1], b[2]);
    printf("patch 1: %s (%d %d %d)  patch 2: %s (%d %d %d)  gl error 0x%x\n", what(a), a[0], a[1],
           a[2], what(b), b[0], b[1], b[2], pGetError());
    return 0;
}
