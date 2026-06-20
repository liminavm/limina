// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Minimal headless GL-context probe for zink-on-KosmicKrisp via Mesa's SURFACELESS EGL
// platform on macOS. No window, no DRM/GBM: get a surfaceless display, init, make a GLES2
// context on a tiny pbuffer, glClear to a known color, read it back. If GL_RENDERER names
// zink/KosmicKrisp and the pixel matches, host GL on zink-on-KK works — the load-bearing
// unknown for virgl-over-zink-on-KK (see README.md). Pixel-verify; proxies lie.
//
// Build/run via run-probe.sh (sets VK_DRIVER_FILES=KK ICD, MESA_LOADER_DRIVER_OVERRIDE=zink).
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <stdio.h>
#include <stdlib.h>

#define CHECK(cond, msg)                                                                   \
    do {                                                                                   \
        if (!(cond)) {                                                                     \
            fprintf(stderr, "FAIL: %s (egl error 0x%x)\n", msg, eglGetError());            \
            return 2;                                                                      \
        }                                                                                  \
    } while (0)

int main(void) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC getPlatformDisplay =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy;
    if (getPlatformDisplay) {
        dpy = getPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    } else {
        fprintf(stderr, "note: eglGetPlatformDisplayEXT missing, trying eglGetDisplay\n");
        dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    }
    CHECK(dpy != EGL_NO_DISPLAY, "get surfaceless display");

    EGLint major = 0, minor = 0;
    CHECK(eglInitialize(dpy, &major, &minor), "eglInitialize");
    printf("EGL %d.%d\n", major, minor);
    printf("EGL_VENDOR     : %s\n", eglQueryString(dpy, EGL_VENDOR));
    printf("EGL_VERSION    : %s\n", eglQueryString(dpy, EGL_VERSION));

    CHECK(eglBindAPI(EGL_OPENGL_ES_API), "eglBindAPI(GLES)");

    const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                                EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                                EGL_ALPHA_SIZE, 8, EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    CHECK(eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n) && n > 0, "eglChooseConfig");

    const EGLint ctx_attrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    CHECK(ctx != EGL_NO_CONTEXT, "eglCreateContext");

    const EGLint pb_attrs[] = {EGL_WIDTH, 64, EGL_HEIGHT, 64, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, pb_attrs);
    CHECK(surf != EGL_NO_SURFACE, "eglCreatePbufferSurface");

    CHECK(eglMakeCurrent(dpy, surf, surf, ctx), "eglMakeCurrent");

    printf("GL_VENDOR      : %s\n", glGetString(GL_VENDOR));
    printf("GL_RENDERER    : %s\n", glGetString(GL_RENDERER));
    printf("GL_VERSION     : %s\n", glGetString(GL_VERSION));

    glClearColor(0.0f, 0.5f, 1.0f, 1.0f); // expect read-back ~ (0,128,255,255)
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();

    unsigned char px[4] = {0, 0, 0, 0};
    glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    printf("readback pixel : (%u, %u, %u, %u)\n", px[0], px[1], px[2], px[3]);

    int ok = (px[2] > 200 && px[1] > 100 && px[1] < 160 && px[0] < 40);
    printf("RESULT         : %s\n", ok ? "PASS — accelerated host GL clear on zink-on-KK"
                                       : "render mismatch (context came up, pixels wrong)");

    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglTerminate(dpy);
    return ok ? 0 : 1;
}
