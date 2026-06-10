// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* #32 buffer-age oracle: does EGL_BUFFER_AGE_EXT match the REAL gbm bo reuse distance?
 *
 * Mimics mutter's native-backend pattern: gbm_surface + EGL, swap, lock the front bo and
 * hold the last TWO locked bos (a scanout in flight + the displayed one) before releasing —
 * which forces triple-buffer rotation, the same strict a->b->c cycle the host instrument
 * sees from gnome-shell. Each frame we record the age EGL reports for the back buffer and,
 * once the cycle repeats, the bo's actual reuse distance. A mismatch = mutter is told to
 * accumulate too little damage = the tier-2 decay (#32).
 *
 * Build (guest): gcc -o buffer-age-test buffer-age-test.c -lgbm -lEGL -lGLESv2
 * Run with the session's zink env (LD_LIBRARY_PATH=/opt/mesa-zink/... etc).
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

#define FRAMES 16
#define HOLD 2 /* bos held locked after swap, mutter-style */

int main(int argc, char **argv) {
    const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    int fd = open(node, O_RDWR);
    if (fd < 0) { perror(node); return 1; }
    struct gbm_device *gbm = gbm_create_device(fd);
    if (!gbm) { fprintf(stderr, "gbm_create_device failed\n"); return 1; }

    EGLDisplay dpy = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) {
        fprintf(stderr, "egl init failed\n"); return 1;
    }
    printf("EGL_VERSION: %s\nEGL_VENDOR: %s\n", eglQueryString(dpy, EGL_VERSION),
           eglQueryString(dpy, EGL_VENDOR));
    const char *exts = eglQueryString(dpy, EGL_EXTENSIONS);
    printf("EGL_EXT_buffer_age: %s\n", strstr(exts, "EGL_EXT_buffer_age") ? "yes" : "NO");

    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cfg_attrs[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT,
                           EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                           EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                           EGL_ALPHA_SIZE, 8, EGL_NONE };
    EGLConfig cfg; EGLint ncfg = 0;
    if (!eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &ncfg) || !ncfg) {
        fprintf(stderr, "no egl config\n"); return 1;
    }
    struct gbm_surface *surf =
        gbm_surface_create(gbm, 1280, 800, GBM_FORMAT_ARGB8888, GBM_BO_USE_RENDERING);
    if (!surf) { fprintf(stderr, "gbm_surface_create failed\n"); return 1; }
    EGLSurface esurf = eglCreatePlatformWindowSurface(dpy, cfg, surf, NULL);
    if (esurf == EGL_NO_SURFACE) { fprintf(stderr, "egl surface failed (0x%x)\n", eglGetError()); return 1; }
    EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, esurf, esurf, ctx)) {
        fprintf(stderr, "egl context failed\n"); return 1;
    }
    printf("GL_RENDERER: %s\n\n", (const char *)glGetString(GL_RENDERER));

    struct gbm_bo *held[HOLD] = {0};
    struct gbm_bo *seen[16] = {0};
    int seen_frame[16] = {0};
    int mismatches = 0;

    printf("frame  reported_age  front_bo        actual_reuse_dist\n");
    for (int f = 1; f <= FRAMES; f++) {
        EGLint age = -1;
        eglQuerySurface(dpy, esurf, EGL_BUFFER_AGE_EXT, &age);
        glClearColor((f % 8) / 8.0f, 0.2f, 0.4f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        eglSwapBuffers(dpy, esurf);

        struct gbm_bo *front = gbm_surface_lock_front_buffer(surf);
        int dist = 0; /* 0 = first use */
        for (int i = 0; i < 16; i++) {
            if (seen[i] == front) { dist = f - seen_frame[i]; break; }
        }
        for (int i = 0; i < 16; i++) {
            if (seen[i] == front || !seen[i]) { seen[i] = front; seen_frame[i] = f; break; }
        }
        printf("%4d   %12d  %p  %d%s\n", f, age, (void *)front, dist,
               (dist && age != dist) ? "   <-- MISMATCH" : "");
        if (dist && age != dist) mismatches++;

        if (held[(f - 1) % HOLD]) gbm_surface_release_buffer(surf, held[(f - 1) % HOLD]);
        held[(f - 1) % HOLD] = front;
    }
    printf("\n%s (%d mismatches)\n", mismatches ? "AGE LIES" : "ages consistent", mismatches);
    return mismatches ? 2 : 0;
}
