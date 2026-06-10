// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * xfb-test: GLES3 transform feedback oracle for the venus/KK XFB
 * implementation (VK_EXT_transform_feedback lowered to VS global stores).
 *
 * Requests an ES 3.0 context (fails = the ES3 gate is still closed), captures
 * gl_Position-derived varyings from a 6-vertex (2-triangle) draw into a TF
 * buffer, and verifies:
 *   1. the captured values match what the VS computed,
 *   2. GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN reports 2,
 *   3. a second Begin/End round APPENDS (pause/resume counter path).
 *
 * PASS prints "XFB WORKS"; any mismatch prints details and exits 2.
 *
 * Build (guest): gcc -o xfb-test xfb-test.c -lEGL -lGLESv2
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

static const char *vs_src =
    "#version 300 es\n"
    "in vec2 p;\n"
    "out vec4 cap;\n"
    "void main() {\n"
    "  gl_Position = vec4(p, 0.0, 1.0);\n"
    "  cap = vec4(p * 2.0, float(gl_VertexID), 7.0);\n"
    "}\n";
static const char *fs_src =
    "#version 300 es\n"
    "precision mediump float;\n"
    "out vec4 c;\n"
    "void main(){ c = vec4(1.0); }\n";

int main(void) {
    setbuf(stdout, NULL);
    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA,
                                           EGL_DEFAULT_DISPLAY, NULL);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        fprintf(stderr, "no surfaceless EGL display\n");
        return 1;
    }
    static const EGLint cfg_attrs[] = { EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT,
                                        EGL_SURFACE_TYPE, 0, EGL_NONE };
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_ES_API);
    static const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, n ? cfg : NULL, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT) {
        printf("NO ES3 CONTEXT (gate closed)\n");
        return 1;
    }
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
    printf("version: %s / %s\n", glGetString(GL_VERSION), glGetString(GL_RENDERER));

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs); glAttachShader(prog, fs);
    glBindAttribLocation(prog, 0, "p");
    const char *varyings[] = { "cap" };
    glTransformFeedbackVaryings(prog, 1, varyings, GL_INTERLEAVED_ATTRIBS);
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) { printf("LINK FAILED\n"); return 2; }
    glUseProgram(prog);

    /* An FBO so the draw has a render target (surfaceless). */
    GLuint fbo, crb;
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glGenRenderbuffers(1, &crb);
    glBindRenderbuffer(GL_RENDERBUFFER, crb);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, 64, 64);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, crb);
    glViewport(0, 0, 64, 64);

    /* TF buffer with room for 12 vec4 (two rounds of 6). */
    GLuint tfb;
    glGenBuffers(1, &tfb);
    glBindBuffer(GL_TRANSFORM_FEEDBACK_BUFFER, tfb);
    glBufferData(GL_TRANSFORM_FEEDBACK_BUFFER, 12 * 4 * sizeof(float), NULL,
                 GL_DYNAMIC_READ);
    glBindBufferBase(GL_TRANSFORM_FEEDBACK_BUFFER, 0, tfb);

    static const float verts[12] = { -1,-1, 1,-1, 1,1,  -1,-1, 1,1, -1,1 };
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, verts);
    glEnableVertexAttribArray(0);

    GLuint q;
    glGenQueries(1, &q);

    /* One transform feedback operation with a Pause/Resume in the middle:
     * GL appends across Resume (zink implements it as End/Begin with counter
     * buffers = the resume path under test). Round 2 uses shifted coords so
     * an overwrite-at-0 is distinguishable from a proper append. */
    static const float verts2[12] = { -.5f,-.5f, .5f,-.5f, .5f,.5f,
                                      -.5f,-.5f, .5f,.5f, -.5f,.5f };
    glBeginQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN, q);
    glBeginTransformFeedback(GL_TRIANGLES);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glPauseTransformFeedback();
    glResumeTransformFeedback();
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, verts2);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glEndTransformFeedback();
    glEndQuery(GL_TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN);
    glFinish();

    GLuint written = 0;
    glGetQueryObjectuiv(q, GL_QUERY_RESULT, &written);

    const float *data = glMapBufferRange(GL_TRANSFORM_FEEDBACK_BUFFER, 0,
                                         12 * 4 * sizeof(float),
                                         GL_MAP_READ_BIT);
    if (!data) { printf("MAP FAILED err=0x%x\n", glGetError()); return 2; }

    int bad = -1;
    for (int round = 0; round < 2 && bad < 0; round++) {
        const float *src = round ? verts2 : verts;
        for (int v = 0; v < 6 && bad < 0; v++) {
            const float *got = data + (round * 6 + v) * 4;
            float ex[4] = { src[v*2] * 2.0f, src[v*2+1] * 2.0f, (float)v, 7.0f };
            for (int c = 0; c < 4; c++)
                if (fabsf(got[c] - ex[c]) > 0.001f) bad = round * 6 + v;
        }
    }

    printf("primitives_written=%u (expect 4: %s)\n", written,
           written == 4 ? "OK" : "WRONG");
    if (bad >= 0) {
        const float *g = data + bad * 4;
        printf("capture MISMATCH at vertex %d: got (%.2f %.2f %.2f %.2f)\n",
               bad, g[0], g[1], g[2], g[3]);
        printf("XFB BROKEN\n");
        return 2;
    }
    if (written != 4) { printf("XFB BROKEN (query)\n"); return 2; }
    printf("XFB WORKS\n");
    return 0;
}
