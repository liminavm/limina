// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* zink shadow-attachment blit recursion oracle (the 2026-08-10 Epiphany crash).
 *
 * WebKit renders its MSAA layers with EXT_multisampled_render_to_texture. On a
 * Vulkan driver WITHOUT VK_EXT_multisampled_render_to_single_sampled — which is
 * every KosmicKrisp today — zink emulates it: it keeps a hidden MSAA "transient"
 * image beside the texture and, at begin_rendering, replicate-blits the texture
 * into the transient (zink_render_attachment_shadow).
 *
 * That blit re-enters. util_blitter rebinds the framebuffer; zink's
 * set_framebuffer_state sees still-pending clears on the same attachment and
 * calls zink_flush_clears -> zink_batch_rp -> begin_rendering, where the
 * transient is STILL !valid (it is only marked valid after the blit returns), so
 * the shadow blit starts again. u_blitter notices ("Caught recursion. This is a
 * driver bug.") but only logs, and the stack runs out.
 *
 * The three ingredients, all of which this probe sets up deliberately:
 *   1. an attachment with pipe_surface.nr_samples != 0 — that is specifically
 *      EXT_multisampled_render_to_texture (gl_renderbuffer_attachment::NumSamples
 *      -> rb->rtt_nr_samples -> rb->surface.nr_samples), NOT a plain MSAA
 *      renderbuffer, which is multisampled in the resource instead;
 *   2. a PARTIAL (scissored) clear pending on that same attachment — a full clear
 *      takes the "skip replicate blit if the image will be full-cleared" branch
 *      and never blits;
 *   3. a draw, to make something actually begin the renderpass.
 *
 * The probe is also the GREEN oracle: it does not merely check that we survived,
 * it checks the pixels. Pass 1 fills the texture red. Pass 2 scissor-clears one
 * corner green and draws a blue quad in the opposite corner. Afterwards all three
 * regions must be right — red in the middle proves the replicate blit really
 * carried the old contents into the transient, which is the whole point of the
 * path being fixed. A "fix" that merely stops the recursion by pretending the
 * transient is valid loses the red and fails here.
 *
 * Host build+run: ./build-and-run.sh (zink-on-KK, surfaceless EGL, no VM).
 * Guest build: gcc -o shadow-recursion shadow-recursion.c -lEGL -lGLESv2
 *
 * Exit status: 0 = pass, 1 = the bug (or a pixel mismatch), 77 = cannot test
 * here (no EGL/GLES, or no EXT_multisampled_render_to_texture).
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define SIZE 256
#define SCISSOR 64 /* green corner: [0,SCISSOR) x [0,SCISSOR) */
#define QUAD 64    /* blue corner: the opposite one */
#define SAMPLES 4

#define SKIP 77

typedef void(GL_APIENTRY *FBTEX2DMS)(GLenum, GLenum, GLenum, GLuint, GLint,
                                     GLsizei);

static const char *vs_src = "attribute vec2 pos;\n"
                            "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *fs_src = "precision mediump float;\n"
                            "void main() { gl_FragColor = vec4(0.0, 0.0, 1.0, 1.0); }\n";

static GLuint compile(GLenum stage, const char *src) {
    GLuint s = glCreateShader(stage);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(s, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        exit(1);
    }
    return s;
}

/* Read back through a plain single-sample FBO wrapping the same texture, i.e.
 * after the MSRTT resolve. Deliberately NOT a read of the multisampled
 * attachment: we want the resolved texels an application would sample. */
static void read_texture(GLuint tex, unsigned char *out) {
    GLuint fb;
    glGenFramebuffers(1, &fb);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           tex, 0);
    GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (st != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "readback FBO incomplete: 0x%x\n", st);
        exit(1);
    }
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, out);
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glDeleteFramebuffers(1, &fb);
}

static bool check_px(const unsigned char *px, int x, int y, int r, int g, int b,
                     const char *what) {
    const unsigned char *p = px + 4 * (y * SIZE + x);
    /* generous tolerance: this asserts WHICH color landed, not exact blending */
    bool ok = abs((int)p[0] - r) <= 8 && abs((int)p[1] - g) <= 8 &&
              abs((int)p[2] - b) <= 8;
    printf("  %-28s (%3d,%3d) = %3d %3d %3d  want %3d %3d %3d  %s\n", what, x, y,
           p[0], p[1], p[2], r, g, b, ok ? "OK" : "MISMATCH");
    return ok;
}

int main(void) {
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        fprintf(stderr, "SKIP: no EGL display (need EGL_PLATFORM=surfaceless)\n");
        return SKIP;
    }
    eglBindAPI(EGL_OPENGL_ES_API);

    EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                          EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                          EGL_ALPHA_SIZE, 8, EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n) || n < 1) {
        fprintf(stderr, "SKIP: no usable EGLConfig\n");
        return SKIP;
    }
    EGLint ctx_attrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT) {
        fprintf(stderr, "SKIP: eglCreateContext failed\n");
        return SKIP;
    }
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        fprintf(stderr, "SKIP: surfaceless eglMakeCurrent failed\n");
        return SKIP;
    }

    printf("GL_RENDERER: %s\n", (const char *)glGetString(GL_RENDERER));
    const char *exts = (const char *)glGetString(GL_EXTENSIONS);
    if (!exts || !strstr(exts, "GL_EXT_multisampled_render_to_texture")) {
        fprintf(stderr, "SKIP: no GL_EXT_multisampled_render_to_texture — this "
                        "driver cannot take the shadow-attachment path\n");
        return SKIP;
    }
    FBTEX2DMS fbtex2dms =
        (FBTEX2DMS)eglGetProcAddress("glFramebufferTexture2DMultisampleEXT");
    if (!fbtex2dms) {
        fprintf(stderr, "SKIP: glFramebufferTexture2DMultisampleEXT missing\n");
        return SKIP;
    }

    GLuint tex;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, SIZE, SIZE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    /* The MSRTT framebuffer: samples on the ATTACHMENT, single-sample texture. */
    GLuint fb;
    glGenFramebuffers(1, &fb);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    fbtex2dms(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0,
              SAMPLES);
    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (status != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "SKIP: MSRTT framebuffer incomplete: 0x%x\n", status);
        return SKIP;
    }
    glViewport(0, 0, SIZE, SIZE);

    /* A second, ordinary FBO to bind away to. Binding framebuffer 0 will not do:
     * under surfaceless EGL there is no default framebuffer, so the bind never
     * reaches set_framebuffer_state and the MSRTT attachment is never unbound —
     * which is exactly why the transient stayed valid in earlier passes. */
    GLuint tex2, fb2;
    glGenTextures(1, &tex2);
    glBindTexture(GL_TEXTURE_2D, tex2);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, SIZE, SIZE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fb2);
    glBindFramebuffer(GL_FRAMEBUFFER, fb2);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           tex2, 0);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);

    /* Pass 1 — establish content the shadow blit must later preserve. */
    glDisable(GL_SCISSOR_TEST);
    glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();
    printf("pass 1 (full red clear) done\n");

    GLuint prog = glCreateProgram();
    glAttachShader(prog, compile(GL_VERTEX_SHADER, vs_src));
    glAttachShader(prog, compile(GL_FRAGMENT_SHADER, fs_src));
    glBindAttribLocation(prog, 0, "pos");
    glLinkProgram(prog);
    glUseProgram(prog);

    /* Pass 2 — the trigger. A SCISSORED clear leaves a partial clear pending on
     * the very attachment whose transient is invalid, so the draw's
     * begin_rendering enters zink_render_attachment_shadow with that clear still
     * enabled. This is the point the process used to die. */
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glEnable(GL_SCISSOR_TEST);
    glScissor(0, 0, SCISSOR, SCISSOR);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_SCISSOR_TEST);

    /* blue quad in the far corner, in NDC */
    const float q0 = 1.0f - 2.0f * (float)QUAD / (float)SIZE;
    const GLfloat verts[] = {q0, q0, 1.0f, q0, q0, 1.0f, 1.0f, 1.0f};
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, verts);
    fflush(stdout);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glFinish();
    printf("pass 2 (scissored clear + draw) done — NO RECURSION\n");

    /* Pass 3 — the crash's ACTUAL entry point. In the Epiphany backtrace the
     * cycle starts at zink_set_framebuffer_state, not at a draw: the app leaves a
     * clear pending and then binds a different framebuffer, so the flush (not the
     * draw) is what opens the renderpass that enters the shadow blit. Leave a
     * scissored clear pending and rebind. */
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glEnable(GL_SCISSOR_TEST);
    glScissor(SIZE - SCISSOR, 0, SCISSOR, SCISSOR);
    glClearColor(0.0f, 1.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_SCISSOR_TEST);
    fflush(stdout);
    glBindFramebuffer(GL_FRAMEBUFFER, fb2); /* fb change flushes the pending clear */
    glFinish();
    printf("pass 3 (pending clear + fb change) done — NO RECURSION\n");

    /* Pass 4 — the real trigger, and the reason passes 2 and 3 are not enough.
     *
     * The transient is only marked INVALID when the MSRTT attachment is UNBOUND
     * (unbind_fb_surface: `if (surf->nr_samples && res->transient)
     * res->transient->valid = false`). After passes 1-3 it is valid, so the
     * shadow path is entered but takes the `if (transient->valid) continue`
     * early-out and never blits. A browser compositor unbinds and rebinds its
     * layer FBOs constantly, which is why WebKit hits this and a naive probe
     * does not.
     *
     * So: bind away (invalidating the transient), bind back, leave a PARTIAL
     * clear pending, and draw. Now begin_rendering enters the shadow blit with
     * an invalid transient AND a live clear on the same attachment — and the
     * blit's own set_framebuffer_state flushes that clear straight back into
     * begin_rendering, where the transient is still invalid. That is the loop. */
    glBindFramebuffer(GL_FRAMEBUFFER, fb2);
    /* The bind alone is not enough: the state tracker validates the framebuffer
     * lazily, at the next draw/clear, so a bind/rebind pair with nothing between
     * them never reaches set_framebuffer_state and never unbinds anything. Do
     * real work here so the MSRTT attachment is genuinely unbound and its
     * transient marked invalid. */
    glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);
    glEnable(GL_SCISSOR_TEST);
    glScissor(0, SIZE - SCISSOR, SCISSOR, SCISSOR);
    glClearColor(1.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_SCISSOR_TEST);
    fflush(stdout);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glFinish();
    printf("pass 4 (invalid transient + pending clear + draw) done — NO RECURSION\n");

    unsigned char *px = malloc(SIZE * SIZE * 4);
    if (!px)
        return 1;
    read_texture(tex, px);

    printf("pixels:\n");
    bool ok = true;
    /* the load-bearing one: content from pass 1, only present if the replicate
     * blit actually populated the transient before pass 2 rendered into it */
    ok &= check_px(px, SIZE / 2, SIZE / 2, 255, 0, 0, "preserved (pass-1 red)");
    ok &= check_px(px, SCISSOR / 2, SCISSOR / 2, 0, 255, 0, "scissored clear (green)");
    ok &= check_px(px, SIZE - QUAD / 2, SIZE - QUAD / 2, 0, 0, 255, "draw (blue)");
    ok &= check_px(px, SIZE - SCISSOR / 2, SCISSOR / 2, 0, 255, 255,
                   "flushed clear (cyan)");
    ok &= check_px(px, SCISSOR / 2, SIZE - SCISSOR / 2, 255, 0, 255,
                   "pass-4 clear (magenta)");

    free(px);
    printf("%s\n", ok ? "RESULT PASS" : "RESULT FAIL (pixels)");
    return ok ? 0 : 1;
}
