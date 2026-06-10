// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* #32 content-persistence oracle: with correct buffer ages (proven by buffer-age-test.c) and
 * zink emitting loadOp=LOAD, does the UNTOUCHED part of the back buffer actually survive the
 * triple-buffer round trip? Mimics mutter's clipped-redraw pattern: paint each buffer fully RED
 * once, then per frame scissor-clear only a small moving GREEN square; pixels outside the square
 * must read back RED forever. Any non-red probe = content loss between store (frame N-3) and
 * load (frame N) — the tier-2 decay reproduced standalone.
 *
 * Build (guest): gcc -o buffer-age-content-test buffer-age-content-test.c -lgbm -lEGL -lGLESv2
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

#ifndef EGL_BUFFER_AGE_EXT
#define EGL_BUFFER_AGE_EXT 0x313D
#endif

#define W 1280
#define H 800
#define FRAMES 18
#define HOLD 2

static int check_probe(int frame, int x, int y) {
    unsigned char px[4] = {0};
    glReadPixels(x, y, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    int red = px[0] > 200 && px[1] < 50 && px[2] < 50;
    if (!red)
        printf("  frame %2d probe (%4d,%3d) = [%3u %3u %3u %3u]  <-- NOT RED\n",
               frame, x, y, px[0], px[1], px[2], px[3]);
    return !red;
}

int main(int argc, char **argv) {
    const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    int fd = open(node, O_RDWR);
    if (fd < 0) { perror(node); return 1; }
    struct gbm_device *gbm = gbm_create_device(fd);
    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        fprintf(stderr, "egl init failed\n"); return 1;
    }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cfg_attrs[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
                           EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                           EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                           EGL_ALPHA_SIZE, 8, EGL_NONE };
    EGLConfig cfg; EGLint ncfg = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &ncfg);
    struct gbm_surface *surf =
        gbm_surface_create(gbm, W, H, GBM_FORMAT_ARGB8888, GBM_BO_USE_RENDERING);
    EGLSurface esurf = eglCreatePlatformWindowSurface(dpy, cfg, surf, NULL);
    EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (!eglMakeCurrent(dpy, esurf, esurf, ctx)) { fprintf(stderr, "makecurrent failed\n"); return 1; }
    printf("GL_RENDERER: %s\n", (const char *)glGetString(GL_RENDERER));

    struct gbm_bo *held[HOLD] = {0};
    int bad = 0;

    for (int f = 1; f <= FRAMES; f++) {
        EGLint age = -1;
        eglQuerySurface(dpy, esurf, EGL_BUFFER_AGE_EXT, &age);

        if (f <= 3) {
            /* Bootstrap: each of the three buffers gets one full RED paint. */
            glDisable(GL_SCISSOR_TEST);
            glClearColor(1, 0, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
        } else {
            /* mutter-style clipped redraw: ONLY a small square is touched. The rest of the
             * buffer must come back from the LOAD as the red painted 3 swaps ago. */
            printf("frame %2d age=%d probes:\n", f, age);
            bad += check_probe(f, 1000, 400);
            bad += check_probe(f, 200, 600);
            bad += check_probe(f, 640, 100);
            glEnable(GL_SCISSOR_TEST);
            glScissor(50 + f * 12, 50, 64, 64);
            glClearColor(0, 1, 0, 1);
            glClear(GL_COLOR_BUFFER_BIT);
            glDisable(GL_SCISSOR_TEST);
        }
        eglSwapBuffers(dpy, esurf);

        struct gbm_bo *front = gbm_surface_lock_front_buffer(surf);
        if (held[(f - 1) % HOLD]) gbm_surface_release_buffer(surf, held[(f - 1) % HOLD]);
        held[(f - 1) % HOLD] = front;
    }
    printf("\n%s (%d bad probes)\n", bad ? "CONTENT LOST" : "content persists", bad);
    return bad ? 2 : 0;
}
