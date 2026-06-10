// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* #32 stencil oracle: does basic GL stencil clipping WORK under zink/venus/MoltenVK?
 * cogl's multi-rect region clip relies on the stencil buffer (cogl-clip-stack-gl.c);
 * if stencil writes/tests silently no-op, every multi-rect clipped redraw paints
 * UNCLIPPED — gnome-shell's background floods the frame and correctly-culled widgets
 * vanish: the seated-desktop decay.
 *
 * Method: stencil=1 on the LEFT half, then a stencil-tested (EQUAL 1) full-screen green
 * draw over a red base. PASS = green left, red right. Stencil broken = green everywhere.
 *
 * Build (guest): gcc -o stencil-test stencil-test.c -lgbm -lEGL -lGLESv2
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <fcntl.h>
#include <gbm.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define W 256
#define H 256

static const char *vs_src =
    "attribute vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *fs_src =
    "precision mediump float;\n"
    "uniform vec4 color;\n"
    "void main() { gl_FragColor = color; }\n";

static GLuint make_prog(void) {
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint p = glCreateProgram();
    glAttachShader(p, vs); glAttachShader(p, fs);
    glBindAttribLocation(p, 0, "pos");
    glLinkProgram(p);
    return p;
}

static void draw_rect(float x0, float y0, float x1, float y1) {
    float v[12] = { x0,y0, x1,y0, x1,y1, x0,y0, x1,y1, x0,y1 };
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, v);
    glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES, 0, 6);
}

int main(int argc, char **argv) {
    setbuf(stdout, NULL); /* venus waits can abort (vn_relax) — keep output */
    const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    int fd = open(node, O_RDWR);
    if (fd < 0) { perror(node); return 1; }
    struct gbm_device *gbm = gbm_create_device(fd);
    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        fprintf(stderr, "egl init failed\n"); return 1;
    }
    eglBindAPI(EGL_OPENGL_ES_API);
    /* Same shape as cogl's request: stencil >= 2, depth >= 1. */
    EGLint cfg_attrs[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
                           EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                           EGL_RED_SIZE, 1, EGL_GREEN_SIZE, 1, EGL_BLUE_SIZE, 1,
                           EGL_DEPTH_SIZE, 1, EGL_STENCIL_SIZE, 2, EGL_NONE };
    /* Pick a config whose native visual matches the GBM surface format we'll
     * create — a mismatched pair makes eglCreatePlatformWindowSurface fail and
     * an unchecked surface turns the test silently SURFACELESS (fb UNDEFINED,
     * 0 bits everywhere) — which is a broken oracle, not a stencil verdict. */
    EGLConfig cfgs[64]; EGLint ncfg = 0;
    eglChooseConfig(dpy, cfg_attrs, cfgs, 64, &ncfg);
    if (!ncfg) { fprintf(stderr, "no stencil config\n"); return 1; }
    EGLConfig cfg = NULL;
    EGLint visual = 0;
    for (EGLint i = 0; i < ncfg; i++) {
        eglGetConfigAttrib(dpy, cfgs[i], EGL_NATIVE_VISUAL_ID, &visual);
        if (visual == (EGLint)GBM_FORMAT_ARGB8888 ||
            visual == (EGLint)GBM_FORMAT_XRGB8888) { cfg = cfgs[i]; break; }
    }
    if (!cfg) { fprintf(stderr, "no gbm-visual config among %d\n", ncfg); return 1; }
    EGLint cfg_stencil = 0, cfg_depth = 0;
    eglGetConfigAttrib(dpy, cfg, EGL_STENCIL_SIZE, &cfg_stencil);
    eglGetConfigAttrib(dpy, cfg, EGL_DEPTH_SIZE, &cfg_depth);
    printf("config: depth=%d stencil=%d visual=0x%x\n", cfg_depth, cfg_stencil, visual);

    struct gbm_surface *surf =
        gbm_surface_create(gbm, W, H, (uint32_t)visual,
                           GBM_BO_USE_RENDERING | GBM_BO_USE_SCANOUT);
    if (!surf) { fprintf(stderr, "gbm_surface_create failed\n"); return 1; }
    EGLSurface esurf = eglCreatePlatformWindowSurface(dpy, cfg, surf, NULL);
    if (esurf == EGL_NO_SURFACE) {
        fprintf(stderr, "eglCreatePlatformWindowSurface FAILED 0x%x\n", eglGetError());
        return 1;
    }
    EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (!eglMakeCurrent(dpy, esurf, esurf, ctx)) { fprintf(stderr, "mc failed\n"); return 1; }

    GLint bits = -1, dbits = -1;
    glGetIntegerv(GL_STENCIL_BITS, &bits);
    glGetIntegerv(GL_DEPTH_BITS, &dbits);
    printf("GL_STENCIL_BITS=%d GL_DEPTH_BITS=%d fb-status=0x%x glErr=0x%x renderer=%s\n",
           bits, dbits, glCheckFramebufferStatus(GL_FRAMEBUFFER), glGetError(),
           glGetString(GL_RENDERER));

    GLuint prog = make_prog();
    glUseProgram(prog);
    GLint ucolor = glGetUniformLocation(prog, "color");

    /* Base: red everywhere, stencil cleared to 0. */
    glClearColor(1, 0, 0, 1);
    glClearStencil(0);
    glClear(GL_COLOR_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);

    /* Write stencil=1 on the LEFT half (no color writes) — cogl's add_stencil pattern. */
    glEnable(GL_STENCIL_TEST);
    glStencilFunc(GL_ALWAYS, 1, 0xff);
    glStencilOp(GL_REPLACE, GL_REPLACE, GL_REPLACE);
    glColorMask(GL_FALSE, GL_FALSE, GL_FALSE, GL_FALSE);
    draw_rect(-1, -1, 0, 1);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);

    /* Stencil-tested full-screen green. */
    glStencilFunc(GL_EQUAL, 1, 0xff);
    glStencilOp(GL_KEEP, GL_KEEP, GL_KEEP);
    glUniform4f(ucolor, 0, 1, 0, 1);
    draw_rect(-1, -1, 1, 1);
    glDisable(GL_STENCIL_TEST);
    glFinish();

    unsigned char l[4], r[4];
    glReadPixels(W / 4, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, l);
    glReadPixels(3 * W / 4, H / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, r);
    printf("left(in-stencil)=[%u %u %u] right(out)=[%u %u %u]\n",
           l[0], l[1], l[2], r[0], r[1], r[2]);

    int left_green = l[1] > 200 && l[0] < 50;
    int right_red = r[0] > 200 && r[1] < 50;
    if (left_green && right_red) { printf("STENCIL WORKS\n"); return 0; }
    printf("STENCIL BROKEN (%s)\n",
           !left_green ? "stencil-tested draw missing" : "stencil test not clipping");
    return 2;
}
