// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Minimal vehicle for the small-texture content corruption seen in gnome-shell on the virgl path.
//
// Every structural explanation has been eliminated by experiment (offscreen FBOs, cogl's texture
// atlas), leaving "the CONTENT of small textures arrives wrong". This reproduces that claim with no
// compositor, no toolkit and no window: upload a known pattern into a small single-channel texture
// the way a glyph cache does — many small glTexSubImage2D rects, textures created and destroyed
// under churn — sample it 1:1 into an FBO, read it back, and compare against what was uploaded.
//
// It is deliberately self-contained so it can be built for the guest (against virgl) AND natively
// on the host (against zink-on-KosmicKrisp), which splits guest-virgl+vrend from KK.
//
// Verification is DEFERRED, which is the point: each iteration verifies a texture uploaded many
// iterations ago, after unrelated textures have been created, destroyed and drawn with in between.
// Verifying immediately (the obvious way) both forces a sync and only ever tests upload->sample; it
// found nothing in 2000 iterations. The real workload uploads a glyph once and samples it across
// many later frames, so content that is destroyed or aliased LATER can only be caught this way.
//
//   texupload [iterations] [tex-size] [live-textures]
// Prints one line per mismatching iteration and a final tally.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "FAIL %s @%d\n", #x, __LINE__); exit(2); } } while (0)

static const char *VS =
    "#version 300 es\n"
    "out vec2 uv;\n"
    "void main(){ vec2 p = vec2((gl_VertexID<<1)&2, gl_VertexID&2);\n"
    "  uv = p; gl_Position = vec4(p*2.0-1.0, 0.0, 1.0); }\n";
static const char *FS =
    "#version 300 es\n"
    "precision highp float;\n"
    "uniform sampler2D t; in vec2 uv; out vec4 c;\n"
    "void main(){ c = vec4(texture(t, uv).r, 0.0, 0.0, 1.0); }\n";

static GLuint compile(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048]; glGetShaderInfoLog(s, sizeof log, NULL, log); fprintf(stderr, "shader: %s\n", log); exit(2); }
    return s;
}

// The value a texel must hold. Deterministic, so a mismatch names the exact texel and what it holds
// instead of what it should — zero (blank row), or something else entirely (stale bytes).
static unsigned char expect(int x, int y, int seed) { return (unsigned char)((x * 7 + y * 13 + seed * 31) & 0xFF); }

int main(int argc, char **argv) {
    int iters = argc > 1 ? atoi(argv[1]) : 2000;
    int size  = argc > 2 ? atoi(argv[2]) : 64;     // glyph-cache scale, not full-screen
    int live  = argc > 3 ? atoi(argv[3]) : 24;     // textures alive at once, to force recycling

    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    CHECK(dpy != EGL_NO_DISPLAY);
    CHECK(eglInitialize(dpy, NULL, NULL));
    CHECK(eglBindAPI(EGL_OPENGL_ES_API));
    EGLint cfgattr[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE };
    EGLConfig cfg; EGLint n = 0;
    CHECK(eglChooseConfig(dpy, cfgattr, &cfg, 1, &n) && n > 0);
    EGLint ctxattr[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxattr);
    CHECK(ctx != EGL_NO_CONTEXT);
    CHECK(eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx));
    printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));

    GLuint prog = glCreateProgram();
    glAttachShader(prog, compile(GL_VERTEX_SHADER, VS));
    glAttachShader(prog, compile(GL_FRAGMENT_SHADER, FS));
    glLinkProgram(prog); glUseProgram(prog);
    glUniform1i(glGetUniformLocation(prog, "t"), 0);
    GLuint vao; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    GLuint fbo, target;
    glGenTextures(1, &target);
    glBindTexture(GL_TEXTURE_2D, target);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, size, size);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
    glViewport(0, 0, size, size);

    GLuint *pool = calloc(live, sizeof *pool);
    int *seed = calloc(live, sizeof *seed);
    for (int i = 0; i < live; i++) seed[i] = -1;
    unsigned char *src = malloc(size * size), *back = malloc(size * size * 4);
    int bad = 0, checked = 0;

    for (int i = 0; i < iters; i++) {
        int slot = i % live;

        // Verify the texture that has been sitting in this slot since `live` iterations ago —
        // before it is retired — so every check spans a full round of allocation churn.
        if (pool[slot] && seed[slot] >= 0) {
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D, pool[slot]);
            glClearColor(0, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glReadPixels(0, 0, size, size, GL_RGBA, GL_UNSIGNED_BYTE, back);
            int mism = 0, zeros = 0, fx = -1, fy = -1, got = 0, want = 0;
            for (int y = 0; y < size; y++) for (int x = 0; x < size; x++) {
                unsigned char e = expect(x, y, seed[slot]), a = back[(y * size + x) * 4];
                if (a != e) { if (!mism) { fx = x; fy = y; got = a; want = e; } mism++; if (!a) zeros++; }
            }
            checked++;
            if (mism) {
                bad++;
                printf("iter %d MISMATCH %d/%d texels (%d zero) first (%d,%d) got %d want %d\n",
                       i, mism, size * size, zeros, fx, fy, got, want);
                fflush(stdout);
            }
        }

        // Retire and re-create, so allocations recycle underneath.
        if (pool[slot]) glDeleteTextures(1, &pool[slot]);
        glGenTextures(1, &pool[slot]);
        seed[slot] = i;
        glBindTexture(GL_TEXTURE_2D, pool[slot]);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8, size, size);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

        for (int y = 0; y < size; y++)
            for (int x = 0; x < size; x++) src[y * size + x] = expect(x, y, i);

        glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
        const int band = 8;
        for (int y = 0; y < size; y += band) {
            glPixelStorei(GL_UNPACK_ROW_LENGTH, size);
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, y, size, band, GL_RED, GL_UNSIGNED_BYTE, src + y * size);
        }
        glPixelStorei(GL_UNPACK_ROW_LENGTH, 0);

        // Unrelated GPU work between upload and the eventual check, so the two are not adjacent.
        glBindFramebuffer(GL_FRAMEBUFFER, fbo);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, pool[(slot + 1) % live] ? pool[(slot + 1) % live] : pool[slot]);
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        glFlush();
    }
    printf("checked=%d mismatching=%d\n", checked, bad);
    return bad ? 1 : 0;
}
