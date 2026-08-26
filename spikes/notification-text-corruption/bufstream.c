// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Minimal vehicle for the corruption, aimed at the STREAMED VERTEX BUFFER rather than at texture
// content. Cogl's journal pushes every batch of glyph/icon quads through a reused VBO mapped
// WRITE|INVALIDATE_BUFFER at offset 0 (and UNSYNCHRONIZED elsewhere), which is precisely the
// traffic our virglrenderer fork special-cases. Corrupt vertex data explains what texture theories
// could not: a whole text row vanishing (degenerate quads), a few specks at the row's origin
// (surviving quads with garbage coordinates), and several rows dying in one frame (one batch).
//
// No readback between iterations — that is the point. Each iteration renders its quad into its own
// cell of one large FBO and the whole grid is verified once at the end, so nothing forces a sync
// that would paper over an upload/draw race.
//
//   bufstream [passes] [cells-per-side] [cell-px]
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "FAIL %s @%d\n", #x, __LINE__); exit(2); } } while (0)

static const char *VS =
    "#version 300 es\n"
    "layout(location=0) in vec2 pos;\n"
    "layout(location=1) in vec4 col;\n"
    "out vec4 vcol;\n"
    "void main(){ vcol = col; gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *FS =
    "#version 300 es\n"
    "precision highp float;\n"
    "in vec4 vcol; out vec4 c;\n"
    "void main(){ c = vcol; }\n";

static GLuint compile(GLenum t, const char *s) {
    GLuint sh = glCreateShader(t); glShaderSource(sh, 1, &s, NULL); glCompileShader(sh);
    GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) { char l[2048]; glGetShaderInfoLog(sh, sizeof l, NULL, l); fprintf(stderr, "%s\n", l); exit(2); }
    return sh;
}

int main(int argc, char **argv) {
    int passes = argc > 1 ? atoi(argv[1]) : 20;
    int cells  = argc > 2 ? atoi(argv[2]) : 32;     // cells per side
    int cpx    = argc > 3 ? atoi(argv[3]) : 16;     // pixels per cell
    int dim = cells * cpx, ncell = cells * cells;

    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    CHECK(dpy != EGL_NO_DISPLAY); CHECK(eglInitialize(dpy, NULL, NULL));
    CHECK(eglBindAPI(EGL_OPENGL_ES_API));
    EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE };
    EGLConfig cfg; EGLint n = 0; CHECK(eglChooseConfig(dpy, ca, &cfg, 1, &n) && n > 0);
    EGLint xa[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, xa); CHECK(ctx != EGL_NO_CONTEXT);
    CHECK(eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx));
    printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));

    GLuint prog = glCreateProgram();
    glAttachShader(prog, compile(GL_VERTEX_SHADER, VS));
    glAttachShader(prog, compile(GL_FRAGMENT_SHADER, FS));
    glLinkProgram(prog); glUseProgram(prog);
    GLuint vao; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    GLuint tex, fbo;
    glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, dim, dim);
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
    glViewport(0, 0, dim, dim);

    // One reused streaming VBO, exactly as a journal would keep.
    const int STRIDE = 6 * sizeof(float);
    const int QUADBYTES = 4 * STRIDE;
    const int RING = 256 * QUADBYTES;
    GLuint vbo; glGenBuffers(1, &vbo); glBindBuffer(GL_ARRAY_BUFFER, vbo);
    glBufferData(GL_ARRAY_BUFFER, RING, NULL, GL_STREAM_DRAW);
    glEnableVertexAttribArray(0); glEnableVertexAttribArray(1);

    unsigned char *back = malloc((size_t)dim * dim * 4);
    int totalbad = 0;

    for (int p = 0; p < passes; p++) {
        glClearColor(0, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
        int off = 0;
        for (int c = 0; c < ncell; c++) {
            int cx = c % cells, cy = c / cells;
            // Colour encodes the cell, so a mismatch says which batch went wrong and how.
            float r = ((c * 37) & 0xFF) / 255.0f, g = ((c * 91) & 0xFF) / 255.0f, b = ((c * 173) & 0xFF) / 255.0f;
            float x0 = (cx * cpx + 2) * 2.0f / dim - 1.0f, x1 = ((cx + 1) * cpx - 2) * 2.0f / dim - 1.0f;
            float y0 = (cy * cpx + 2) * 2.0f / dim - 1.0f, y1 = ((cy + 1) * cpx - 2) * 2.0f / dim - 1.0f;
            float q[24] = { x0,y0, r,g,b,1,  x1,y0, r,g,b,1,  x0,y1, r,g,b,1,  x1,y1, r,g,b,1 };

            // Wrap the ring at offset 0 with INVALIDATE_BUFFER (orphan), stream UNSYNCHRONIZED
            // elsewhere — the two branches our fork's heuristic distinguishes.
            if (off + QUADBYTES > RING) off = 0;
            GLbitfield flags = GL_MAP_WRITE_BIT | GL_MAP_FLUSH_EXPLICIT_BIT |
                               (off == 0 ? GL_MAP_INVALIDATE_BUFFER_BIT : GL_MAP_UNSYNCHRONIZED_BIT);
            void *m = glMapBufferRange(GL_ARRAY_BUFFER, off, QUADBYTES, flags);
            CHECK(m);
            memcpy(m, q, sizeof q);
            glFlushMappedBufferRange(GL_ARRAY_BUFFER, 0, QUADBYTES);
            glUnmapBuffer(GL_ARRAY_BUFFER);

            glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, STRIDE, (void *)(intptr_t)off);
            glVertexAttribPointer(1, 4, GL_FLOAT, GL_FALSE, STRIDE, (void *)(intptr_t)(off + 2 * sizeof(float)));
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            off += QUADBYTES;
        }
        glReadPixels(0, 0, dim, dim, GL_RGBA, GL_UNSIGNED_BYTE, back);
        int bad = 0;
        for (int c = 0; c < ncell; c++) {
            int cx = c % cells, cy = c / cells;
            int px = cx * cpx + cpx / 2, py = cy * cpx + cpx / 2;
            unsigned char *s = back + ((size_t)py * dim + px) * 4;
            unsigned char er = (c * 37) & 0xFF, eg = (c * 91) & 0xFF, eb = (c * 173) & 0xFF;
            int dr = abs((int)s[0] - er), dg = abs((int)s[1] - eg), db = abs((int)s[2] - eb);
            if (dr > 2 || dg > 2 || db > 2) {
                if (!bad) printf("pass %d cell %d at (%d,%d) got %d,%d,%d want %d,%d,%d%s\n",
                                 p, c, cx, cy, s[0], s[1], s[2], er, eg, eb,
                                 (s[0] | s[1] | s[2]) == 0 ? "  [BLANK]" : "");
                bad++;
            }
        }
        if (bad) { printf("pass %d: %d/%d cells wrong\n", p, bad, ncell); fflush(stdout); }
        totalbad += bad;
    }
    printf("passes=%d cells/pass=%d total-wrong=%d\n", passes, ncell, totalbad);
    return totalbad ? 1 : 0;
}
