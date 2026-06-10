// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * stencil-fbo-test: the stencil-test.c left-half oracle, but on a surfaceless EGL
 * context + user FBO (color + D24S8 renderbuffers) instead of a GBM window surface.
 *
 * Differential vehicle vs stencil-test.c: a GBM window surface's color buffer is
 * scanout-capable, so on the limina KK path it goes through the vkr forced-LINEAR +
 * IOSurface + host-pointer-import scanout plumbing; an FBO renderbuffer does not.
 * stencil-test FAIL + stencil-fbo-test PASS ⟹ the scanout plumbing breaks rendering;
 * both FAIL ⟹ the driver's stencil path itself is broken (a la #32).
 *
 * PASS = green left, red right (rc=0). Green everywhere = stencil no-op (rc=2).
 * Red everywhere = stencil-tested draw missing (rc=2).
 *
 * Build (guest): gcc -o stencil-fbo-test stencil-fbo-test.c -lEGL -lGLESv2
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <stdio.h>
#include <string.h>

#define W 256
#define H 256

static GLuint mkprog(const char *fs_color) {
    const char *vs_src =
        "attribute vec2 p; void main(){ gl_Position = vec4(p, 0.0, 1.0); }";
    char fs_src[128];
    snprintf(fs_src, sizeof(fs_src),
             "precision mediump float; void main(){ gl_FragColor = %s; }", fs_color);
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    const char *fsp = fs_src;
    glShaderSource(fs, 1, &fsp, NULL); glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs); glAttachShader(prog, fs);
    glBindAttribLocation(prog, 0, "p");
    glLinkProgram(prog);
    return prog;
}

static void quad(float x0, float x1) {
    const float v[] = { x0,-1, x1,-1, x0,1, x1,-1, x1,1, x0,1 };
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, v);
    glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES, 0, 6);
}

int main(void) {
    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA,
                                           EGL_DEFAULT_DISPLAY, NULL);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        fprintf(stderr, "no surfaceless EGL display\n");
        return 1;
    }
    static const EGLint cfg_attrs[] = { EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                                        EGL_SURFACE_TYPE, 0, EGL_NONE };
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_ES_API);
    static const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, n ? cfg : NULL, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT) { fprintf(stderr, "no context\n"); return 1; }
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);

    GLuint fbo, crb, dsrb;
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glGenRenderbuffers(1, &crb);
    glBindRenderbuffer(GL_RENDERBUFFER, crb);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8_OES, W, H);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                              GL_RENDERBUFFER, crb);
    glGenRenderbuffers(1, &dsrb);
    glBindRenderbuffer(GL_RENDERBUFFER, dsrb);
    glRenderbufferStorage(GL_RENDERBUFFER, GL_DEPTH24_STENCIL8_OES, W, H);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT,
                              GL_RENDERBUFFER, dsrb);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_STENCIL_ATTACHMENT,
                              GL_RENDERBUFFER, dsrb);
    GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    printf("renderer=%s\nfb-status=0x%x glErr=0x%x\n",
           glGetString(GL_RENDERER), st, glGetError());
    if (st != GL_FRAMEBUFFER_COMPLETE) return 1;

    glViewport(0, 0, W, H);

    /* Base: clear red, stencil 0. */
    glClearColor(1, 0, 0, 1);
    glClearStencil(0);
    glClear(GL_COLOR_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);

    /* Write stencil=1 on the LEFT half (no color writes) — cogl-style clip mask. */
    GLuint red = mkprog("vec4(0.0, 0.0, 1.0, 1.0)"); /* color masked off anyway */
    glUseProgram(red);
    glColorMask(GL_FALSE, GL_FALSE, GL_FALSE, GL_FALSE);
    glEnable(GL_STENCIL_TEST);
    glStencilFunc(GL_ALWAYS, 1, 0xff);
    glStencilOp(GL_REPLACE, GL_REPLACE, GL_REPLACE);
    quad(-1.0f, 0.0f);

    /* Stencil-tested (EQUAL 1) full-screen green draw. */
    GLuint green = mkprog("vec4(0.0, 1.0, 0.0, 1.0)");
    glUseProgram(green);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    glStencilFunc(GL_EQUAL, 1, 0xff);
    glStencilOp(GL_KEEP, GL_KEEP, GL_KEEP);
    quad(-1.0f, 1.0f);
    glDisable(GL_STENCIL_TEST);
    glFinish();

    unsigned char l[4] = {0}, r[4] = {0};
    glReadPixels(W / 4, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, l);
    glReadPixels(3 * W / 4, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, r);
    printf("left(in-stencil)=[%d %d %d] right(out)=[%d %d %d]\n",
           l[0], l[1], l[2], r[0], r[1], r[2]);

    int left_green = l[1] > 200 && l[0] < 50;
    int right_red = r[0] > 200 && r[1] < 50;
    if (left_green && right_red) { printf("STENCIL WORKS\n"); return 0; }
    if (l[1] > 200 && r[1] > 200) { printf("STENCIL BROKEN (no-op: green everywhere)\n"); return 2; }
    printf("STENCIL BROKEN (stencil-tested draw missing)\n");
    return 2;
}
