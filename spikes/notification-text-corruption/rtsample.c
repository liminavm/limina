// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Reproduce, with no compositor and no toolkit, the shape the mutter ladder ended on:
//
//   content rendered into a FRESHLY CREATED offscreen is not visible to the draw that samples it,
//   in the same command batch, unless the batch is ended.
//
// That is what gnome-shell does per notification: every card's label gets its own new texture and
// FBO, is drawn into, and is sampled into the stage in the same frame. Forcing a bare glFlush after
// the label draw cured it 10/10; a 5 ms sleep in the same place cured nothing, so the active
// ingredient is the submission boundary, not latency.
//
// Two things here are load-bearing and both are what texupload.c got wrong:
//   - the target is FRESH each iteration (a new texture + FBO), because a recycled one has already
//     been through the transition that is suspected missing;
//   - verification is DEFERRED to after every iteration, because reading back is itself a batch
//     boundary and would cure the very thing being measured.
//
// Each iteration renders a two-part image into its fresh offscreen -- a full clear in BLUE, then a
// quad over the middle half in RED -- and samples it 1:1 into one cell of a persistent grid. The
// two parts are what make a failure legible rather than merely present: a cell that is all blue
// says the clear survived and the draw did not, which is exactly the card-with-no-text shape.
//
// Builds for the guest (virgl) and natively on the host (zink-on-KosmicKrisp), which splits
// guest-virgl+vrend from KK.
//
//   rtsample [cells] [size]
// Arms, all self-evidencing (each prints that it engaged):
//   RTS_FLUSH=1     glFlush after the offscreen is drawn        -- the predicted cure
//   RTS_FINISH=1    glFinish after the offscreen is drawn       -- the heavy control
//   RTS_SLEEP_US=n  delay only, submitting nothing              -- the lateness control
//   RTS_REUSE=1     one texture+FBO reused for every cell       -- is FRESHNESS the variable?
//   RTS_NODRAW=1    clear the offscreen, never draw into it     -- is a CLEAR enough to trip it?
//
// The plain shape above does NOT reproduce, so these carry the ways the real case differs. They are
// separate knobs rather than one faithful mock on purpose: a mock that reproduces names nothing,
// and the point is to find the smallest ingredient that does.
//   RTS_W, RTS_H    offscreen dimensions (labels are wide and short, 968x44, not square)
//   RTS_TEXDRAW=1   the offscreen draw SAMPLES a texture, blended, as a glyph draw does -- so the
//                   pass that writes the offscreen also reads one
//   RTS_BLEND=1     blend the composite, as the stage does
//   RTS_REBIND=1    interleave another target between the offscreen's clear and its draw, so the
//                   offscreen is written by TWO render passes rather than one -- the shape a
//                   load/store action can be got wrong on
//   RTS_VBO=1       feed the offscreen draw from a VERTEX BUFFER re-uploaded every iteration,
//                   instead of generating vertices from gl_VertexID. This is the ingredient every
//                   other arm structurally lacks: with no vertex buffer there is nothing for a
//                   buffer upload to be late for, so no arm here could ever hit an
//                   upload-versus-draw hazard. The host probe says the failing draw rasterises
//                   nothing at all (samples=0) with identical state, program and source texture,
//                   which is what a draw fetching stale or empty geometry looks like.
//   RTS_TWICE=1     render the offscreen, sample it, then render THE SAME target again and sample
//                   again. This is the ingredient the host probe named: a card's label offscreen is
//                   painted twice per post, the first render lands and the second leaves the
//                   texture empty, and the composite that reaches the screen is the second one.
//                   Every other arm here renders each offscreen exactly once, which is why they
//                   are all clean.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "FAIL %s @%d\n", #x, __LINE__); exit(2); } } while (0)

// One vertex shader for both draws: a full-viewport triangle scaled by `rect` (zw = half-size in
// NDC), so the same code paints the whole offscreen and the middle-half quad.
static const char *VS =
    "#version 300 es\n"
    "uniform vec4 rect;\n"
    "out vec2 uv;\n"
    "void main(){ vec2 p = vec2(float((gl_VertexID<<1)&2), float(gl_VertexID&2));\n"
    "  uv = p; gl_Position = vec4(rect.xy + (p*2.0-1.0)*rect.zw, 0.0, 1.0); }\n";
// An attribute-fed vertex shader, for the RTS_VBO arm.
static const char *VS_ATTR =
    "#version 300 es\n"
    "layout(location = 0) in vec2 pos;\n"
    "out vec2 uv;\n"
    "void main(){ uv = pos * 0.5 + 0.5; gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *FS_SOLID =
    "#version 300 es\n"
    "precision highp float;\n"
    "uniform vec4 col; in vec2 uv; out vec4 c;\n"
    "void main(){ c = col; }\n";
static const char *FS_TEX =
    "#version 300 es\n"
    "precision highp float;\n"
    "uniform sampler2D t; in vec2 uv; out vec4 c;\n"
    "void main(){ c = texture(t, uv); }\n";

static GLuint compile(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[2048]; glGetShaderInfoLog(s, sizeof log, NULL, log); fprintf(stderr, "shader: %s\n", log); exit(2); }
    return s;
}

static GLuint link_prog_vs(const char *vs_src, const char *fs_src) {
    GLuint p = glCreateProgram();
    glAttachShader(p, compile(GL_VERTEX_SHADER, vs_src));
    glAttachShader(p, compile(GL_FRAGMENT_SHADER, fs_src));
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) { char log[2048]; glGetProgramInfoLog(p, sizeof log, NULL, log); fprintf(stderr, "link: %s\n", log); exit(2); }
    return p;
}

static GLuint link_prog(const char *fs_src) { return link_prog_vs(VS, fs_src); }

int main(int argc, char **argv) {
    int cells = argc > 1 ? atoi(argv[1]) : 256;
    int size  = argc > 2 ? atoi(argv[2]) : 64;
    int flush = getenv("RTS_FLUSH") != NULL;
    int finish = getenv("RTS_FINISH") != NULL;
    int reuse = getenv("RTS_REUSE") != NULL;
    int nodraw = getenv("RTS_NODRAW") != NULL;
    int sleep_us = getenv("RTS_SLEEP_US") ? atoi(getenv("RTS_SLEEP_US")) : 0;
    int texdraw = getenv("RTS_TEXDRAW") != NULL;
    int blend = getenv("RTS_BLEND") != NULL;
    int rebind = getenv("RTS_REBIND") != NULL;
    int twice = getenv("RTS_TWICE") != NULL;
    int usevbo = getenv("RTS_VBO") != NULL;
    int ow = getenv("RTS_W") ? atoi(getenv("RTS_W")) : size;
    int oh = getenv("RTS_H") ? atoi(getenv("RTS_H")) : size;

    // The grid cell stays `size`; a wide offscreen is sampled down into it, which is what the
    // stage does to a label anyway.
    int grid = 1; while (grid * grid < cells) grid++;
    int aw = grid * size, ah = grid * size;

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

    printf("rtsample: %s | %d cells, offscreen %dx%d -> cell %d (grid %dx%d) | arms:%s%s%s%s%s%s%s%s%s%s\n",
           (const char *)glGetString(GL_RENDERER), cells, ow, oh, size, grid, grid,
           flush ? " FLUSH" : "", finish ? " FINISH" : "", reuse ? " REUSE" : "",
           nodraw ? " NODRAW" : "", sleep_us ? " SLEEP" : "", texdraw ? " TEXDRAW" : "",
           blend ? " BLEND" : "", rebind ? " REBIND" : "", twice ? " TWICE" : "",
           usevbo ? " VBO" : "",
           (!flush && !finish && !reuse && !nodraw && !sleep_us && !texdraw && !blend && !rebind
            && !twice && !usevbo) ? " none (baseline)" : "");
    if (sleep_us) printf("rtsample: sleeping %d us per iteration, submitting nothing\n", sleep_us);

    GLuint p_solid = link_prog(FS_SOLID), p_tex = link_prog(FS_TEX);
    GLuint p_attr = link_prog_vs(VS_ATTR, FS_SOLID);
    GLint u_col_a = glGetUniformLocation(p_attr, "col");
    GLuint vbo = 0;

    if (usevbo)
        glGenBuffers(1, &vbo);
    GLint u_rect_s = glGetUniformLocation(p_solid, "rect"), u_col = glGetUniformLocation(p_solid, "col");
    GLint u_rect_t = glGetUniformLocation(p_tex, "rect");

    // The accumulation grid is persistent and cleared to transparent black, so a cell that never
    // received anything is distinguishable from one that received only the clear.
    GLuint atex, afbo;
    glGenTextures(1, &atex);
    glBindTexture(GL_TEXTURE_2D, atex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, aw, ah, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &afbo);
    glBindFramebuffer(GL_FRAMEBUFFER, afbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, atex, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
    glClearColor(0, 0, 0, 0);
    glClear(GL_COLOR_BUFFER_BIT);

    // A stand-in for the glyph atlas: one persistent texture the offscreen pass READS while it is
    // being written to, which the plain shape never does.
    GLuint atlas = 0;
    if (texdraw) {
        unsigned char *ap = malloc(64 * 64 * 4);
        CHECK(ap);
        for (int i = 0; i < 64 * 64; i++) { ap[i*4+0] = 255; ap[i*4+1] = 0; ap[i*4+2] = 0; ap[i*4+3] = 255; }
        glGenTextures(1, &atlas);
        glBindTexture(GL_TEXTURE_2D, atlas);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, ap);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        free(ap);
    }

    // A third target, so REBIND can put a real render pass between the offscreen's clear and its
    // draw instead of merely rebinding.
    GLuint stex = 0, sfbo = 0;
    if (rebind) {
        glGenTextures(1, &stex);
        glBindTexture(GL_TEXTURE_2D, stex);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, size, size, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glGenFramebuffers(1, &sfbo);
        glBindFramebuffer(GL_FRAMEBUFFER, sfbo);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, stex, 0);
        CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
    }

    // Held, never freed inside the loop: deleting a texture is its own synchronisation event, and
    // the sample that reads it has not been submitted yet.
    GLuint *texs = calloc(cells, sizeof *texs), *fbos = calloc(cells, sizeof *fbos);
    CHECK(texs && fbos);

    for (int i = 0; i < cells; i++) {
        int slot = reuse ? 0 : i;

        if (!reuse || i == 0) {
            glGenTextures(1, &texs[slot]);
            glBindTexture(GL_TEXTURE_2D, texs[slot]);
            glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, ow, oh, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            glGenFramebuffers(1, &fbos[slot]);
            glBindFramebuffer(GL_FRAMEBUFFER, fbos[slot]);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texs[slot], 0);
            CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE);
        }

        for (int pass = 0; pass < (twice ? 2 : 1); pass++) {
        // Render into the offscreen: a clear that covers it, then a draw that does not. Both must
        // arrive for the cell to be correct, and which one is missing names the failure.
        glBindFramebuffer(GL_FRAMEBUFFER, fbos[slot]);
        glViewport(0, 0, ow, oh);
        glClearColor(0, 0, 1, 1);
        glClear(GL_COLOR_BUFFER_BIT);

        // Split the offscreen's rendering across two render passes, with real work on another
        // target in between.
        if (rebind) {
            glBindFramebuffer(GL_FRAMEBUFFER, sfbo);
            glViewport(0, 0, size, size);
            glUseProgram(p_solid);
            glUniform4f(u_rect_s, 0, 0, 1, 1);
            glUniform4f(u_col, 0, 1, 0, 1);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glBindFramebuffer(GL_FRAMEBUFFER, fbos[slot]);
            glViewport(0, 0, ow, oh);
        }

        if (!nodraw) {
            glDisable(GL_BLEND);
            if (texdraw) {
                glEnable(GL_BLEND);
                glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
                glUseProgram(p_tex);
                glUniform4f(u_rect_t, 0, 0, 0.5f, 0.5f);
                glBindTexture(GL_TEXTURE_2D, atlas);
            } else if (usevbo) {
                // Re-upload the geometry every iteration and draw straight from it, which is what
                // a journal flush does: fill a stream buffer, then immediately consume it.
                static const float quad[12] = {
                    -0.5f, -0.5f,  0.5f, -0.5f, -0.5f, 0.5f,
                    -0.5f,  0.5f,  0.5f, -0.5f,  0.5f, 0.5f,
                };

                glBindBuffer(GL_ARRAY_BUFFER, vbo);
                glBufferData(GL_ARRAY_BUFFER, sizeof quad, quad, GL_STREAM_DRAW);
                glUseProgram(p_attr);
                glUniform4f(u_col_a, 1, 0, 0, 1);
                glEnableVertexAttribArray(0);
                glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, (void *)0);
                glDrawArrays(GL_TRIANGLES, 0, 6);
                glDisableVertexAttribArray(0);
                glBindBuffer(GL_ARRAY_BUFFER, 0);
                goto drawn;
            } else {
                glUseProgram(p_solid);
                glUniform4f(u_rect_s, 0, 0, 0.5f, 0.5f);
                glUniform4f(u_col, 1, 0, 0, 1);
            }
            glDrawArrays(GL_TRIANGLES, 0, 3);
drawn:;
            glDisable(GL_BLEND);
        }

        if (finish) glFinish();
        else if (flush) glFlush();
        if (sleep_us) usleep(sleep_us);

        // Sample it into its cell, in the same batch, with nothing forced in between.
        glBindFramebuffer(GL_FRAMEBUFFER, afbo);
        glViewport((i % grid) * size, (i / grid) * size, size, size);
        if (blend) { glEnable(GL_BLEND); glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA); }
        glUseProgram(p_tex);
        glUniform4f(u_rect_t, 0, 0, 1, 1);
        glBindTexture(GL_TEXTURE_2D, texs[slot]);
        glDrawArrays(GL_TRIANGLES, 0, 3);
        glDisable(GL_BLEND);
        }
    }

    // ONE readback, after everything. Doing this per iteration is what made the earlier vehicle
    // blind: the readback would have supplied the batch boundary being tested for.
    unsigned char *px = malloc((size_t)aw * ah * 4);
    CHECK(px);
    glBindFramebuffer(GL_FRAMEBUFFER, afbo);
    glReadPixels(0, 0, aw, ah, GL_RGBA, GL_UNSIGNED_BYTE, px);

    int ok = 0, blank = 0, cleared_only = 0, other = 0;
    for (int i = 0; i < cells; i++) {
        int bx = (i % grid) * size, by = (i / grid) * size;
        const unsigned char *ctr = px + (((size_t)(by + size / 2) * aw) + bx + size / 2) * 4;
        const unsigned char *cor = px + (((size_t)(by + 2) * aw) + bx + 2) * 4;
        int ctr_red = ctr[0] > 200 && ctr[2] < 60, ctr_blue = ctr[2] > 200 && ctr[0] < 60;
        int cor_blue = cor[2] > 200 && cor[0] < 60;
        int ctr_zero = ctr[3] < 16 && ctr[0] < 16 && ctr[2] < 16;
        int cor_zero = cor[3] < 16 && cor[0] < 16 && cor[2] < 16;

        if (nodraw ? (ctr_blue && cor_blue) : (ctr_red && cor_blue)) ok++;
        else if (ctr_zero && cor_zero) {
            if (blank < 6) printf("  cell %d BLANK (nothing arrived)\n", i);
            blank++;
        } else if (ctr_blue && cor_blue) {
            if (cleared_only < 6) printf("  cell %d CLEAR-ONLY (clear arrived, draw did not)\n", i);
            cleared_only++;
        } else {
            if (other < 6) printf("  cell %d OTHER centre=%02x%02x%02x%02x corner=%02x%02x%02x%02x\n",
                                  i, ctr[0], ctr[1], ctr[2], ctr[3], cor[0], cor[1], cor[2], cor[3]);
            other++;
        }
    }
    printf("VERDICT: ok=%d blank=%d clear-only=%d other=%d of %d\n",
           ok, blank, cleared_only, other, cells);
    return (ok == cells) ? 0 : 1;
}
