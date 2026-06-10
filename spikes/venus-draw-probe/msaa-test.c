// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* Firefox-WebGL MSAA oracle: reproduce MozFramebuffer::CreateImpl
 * (color RENDERBUFFER + DEPTH24_STENCIL8, samples=4) on zink/venus and report
 * exactly which attachment breaks completeness. Firefox saw 0x8cd6
 * (FRAMEBUFFER_INCOMPLETE_ATTACHMENT) with no Mesa-side error.
 *
 * Build (guest): gcc -o msaa-test msaa-test.c -lgbm -lEGL -lGLESv2
 * Run with the zink session env. Surfaceless context (no winsys needed).
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <stdio.h>
#include <string.h>

#define W 1024
#define H 512

typedef void(GL_APIENTRY *RSM)(GLenum, GLsizei, GLenum, GLsizei, GLsizei);

static const char *statusname(GLenum s) {
    switch (s) {
    case GL_FRAMEBUFFER_COMPLETE: return "COMPLETE";
    case GL_FRAMEBUFFER_INCOMPLETE_ATTACHMENT: return "INCOMPLETE_ATTACHMENT";
    case GL_FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT: return "MISSING_ATTACHMENT";
    case GL_FRAMEBUFFER_INCOMPLETE_DIMENSIONS: return "INCOMPLETE_DIMENSIONS";
    case GL_FRAMEBUFFER_UNSUPPORTED: return "UNSUPPORTED";
    case 0x8D56: return "INCOMPLETE_MULTISAMPLE";
    default: return "?";
    }
}

static void try_fbo(RSM rsm, GLsizei samples, GLenum colorfmt, GLenum dsfmt) {
    GLuint fb, crb, dsrb;
    glGenFramebuffers(1, &fb);
    glBindFramebuffer(GL_FRAMEBUFFER, fb);

    glGenRenderbuffers(1, &crb);
    glBindRenderbuffer(GL_RENDERBUFFER, crb);
    if (samples && rsm) rsm(GL_RENDERBUFFER, samples, colorfmt, W, H);
    else glRenderbufferStorage(GL_RENDERBUFFER, colorfmt, W, H);
    GLenum e1 = glGetError();
    GLint csamp = -1, cw = -1;
    glGetRenderbufferParameteriv(GL_RENDERBUFFER, 0x8CAB /*SAMPLES*/, &csamp);
    glGetRenderbufferParameteriv(GL_RENDERBUFFER, GL_RENDERBUFFER_WIDTH, &cw);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, crb);

    glGenRenderbuffers(1, &dsrb);
    glBindRenderbuffer(GL_RENDERBUFFER, dsrb);
    if (samples && rsm) rsm(GL_RENDERBUFFER, samples, dsfmt, W, H);
    else glRenderbufferStorage(GL_RENDERBUFFER, dsfmt, W, H);
    GLenum e2 = glGetError();
    GLint dsamp = -1, dw = -1;
    glGetRenderbufferParameteriv(GL_RENDERBUFFER, 0x8CAB, &dsamp);
    glGetRenderbufferParameteriv(GL_RENDERBUFFER, GL_RENDERBUFFER_WIDTH, &dw);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, dsrb);
    glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_STENCIL_ATTACHMENT, GL_RENDERBUFFER, dsrb);

    GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    printf("samples=%d color=0x%x(err=0x%x got_samples=%d w=%d) ds=0x%x(err=0x%x got_samples=%d w=%d) -> %s (0x%x)\n",
           samples, colorfmt, e1, csamp, cw, dsfmt, e2, dsamp, dw, statusname(st), st);

    /* color-only variant when the combined one fails */
    if (st != GL_FRAMEBUFFER_COMPLETE) {
        glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, 0);
        glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_STENCIL_ATTACHMENT, GL_RENDERBUFFER, 0);
        printf("  color-only -> %s\n", statusname(glCheckFramebufferStatus(GL_FRAMEBUFFER)));
        glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, dsrb);
        glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_STENCIL_ATTACHMENT, GL_RENDERBUFFER, dsrb);
        glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, 0);
        printf("  ds-only    -> %s\n", statusname(glCheckFramebufferStatus(GL_FRAMEBUFFER)));
    }
    glDeleteFramebuffers(1, &fb);
    glDeleteRenderbuffers(1, &crb);
    glDeleteRenderbuffers(1, &dsrb);
}

int main(void) {
    setbuf(stdout, NULL);
    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA,
                                           EGL_DEFAULT_DISPLAY, NULL);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        fprintf(stderr, "surfaceless egl init failed\n");
        return 1;
    }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE};
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    EGLint ctx_attrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, n ? cfg : NULL, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT) { fprintf(stderr, "ctx fail 0x%x\n", eglGetError()); return 1; }
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        fprintf(stderr, "mc fail 0x%x\n", eglGetError()); return 1;
    }

    printf("renderer: %s\n", glGetString(GL_RENDERER));
    const char *exts = (const char *)glGetString(GL_EXTENSIONS);
    printf("multisample exts:");
    for (const char *p = exts; (p = strstr(p, "multisample")); p++) {
        const char *s = p; while (s > exts && s[-1] != ' ') s--;
        const char *e = strchr(p, ' '); if (!e) e = p + strlen(p);
        printf(" %.*s", (int)(e - s), s);
    }
    printf("\n");
    GLint maxsamp = -1;
    glGetIntegerv(0x8D57 /*GL_MAX_SAMPLES*/, &maxsamp);
    printf("GL_MAX_SAMPLES=%d err=0x%x\n", maxsamp, glGetError());

    RSM rsm = (RSM)eglGetProcAddress("glRenderbufferStorageMultisampleEXT");
    printf("glRenderbufferStorageMultisampleEXT=%p\n", (void *)rsm);

    /* Firefox shape: RGBA8 color RB + DEPTH24_STENCIL8 DS RB */
    try_fbo(rsm, 0, 0x8058 /*RGBA8_OES*/, 0x88F0 /*DEPTH24_STENCIL8*/);
    try_fbo(rsm, 4, 0x8058, 0x88F0);
    /* D32F_S8 alternative the emu substitutes to, in case D24S8 is the issue */
    try_fbo(rsm, 4, 0x8058, 0x8CAD /*DEPTH32F_STENCIL8*/);
    return 0;
}
