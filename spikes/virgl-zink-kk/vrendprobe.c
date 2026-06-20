// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Standalone virgl_renderer_init() probe: bring up virglrenderer's OWN vrend EGL winsys in
// surfaceless mode on macOS, backed by zink-on-KK. This is the real init path the worker/rutabaga
// drive — if it returns 0 and a VIRGL2 capset materializes, vrend is alive on zink-on-KK and the
// only remaining work is wiring it into the worker (drop NO_VIRGL, advertise the capset).
//
// flags = USE_EGL | USE_SURFACELESS  → virglrenderer calls its internal virgl_egl_init(surfaceless)
// → eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA) on the loaded libEGL (our zink-kk Mesa).
// No create_gl_context callbacks (those are for caller-provided winsys); get_drm_fd returns -1.
#include "virglrenderer.h"
#include <stdio.h>
#include <stdint.h>

static void write_fence(void *cookie, uint32_t fence) { (void)cookie; (void)fence; }
static int  get_drm_fd(void *cookie) { (void)cookie; return -1; } // surfaceless: no drm fd

int main(void) {
    struct virgl_renderer_callbacks cbs = {0};
    cbs.version = 2;            // v2 = get_drm_fd present
    cbs.write_fence = write_fence;
    cbs.get_drm_fd = get_drm_fd;

    // USE_GLES: drive vrend through a host GLES context (like the ANGLE path). On macOS epoxy
    // routes desktop-GL calls to Apple's system OpenGL framework (→ glFlush segfault, no CGL ctx),
    // but GLES calls dlopen libGLESv2 by name → resolves to our zink-on-KK Mesa. So GLES is the
    // correct host API here. (Guest GL is unaffected; virglrenderer translates guest GL→host GLES.)
    int flags = VIRGL_RENDERER_USE_EGL | VIRGL_RENDERER_USE_SURFACELESS | VIRGL_RENDERER_USE_GLES |
                VIRGL_RENDERER_THREAD_SYNC;
    int cookie_obj = 0; // virglrenderer requires a non-NULL cookie for the vrend path
    int ret = virgl_renderer_init(&cookie_obj, flags, &cbs);
    printf("virgl_renderer_init(USE_EGL|SURFACELESS) = %d\n", ret);
    if (ret != 0) {
        fprintf(stderr, "FAIL: virgl_renderer_init returned %d (vrend EGL winsys did not come up)\n", ret);
        return 1;
    }

    // VIRGL2 capset = 2. A non-zero size proves vrend enumerated GL caps through zink-on-KK.
    uint32_t cap_ver = 0, cap_sz = 0;
    virgl_renderer_get_cap_set(2 /* VIRTIO_GPU_CAPSET_VIRGL2 */, &cap_ver, &cap_sz);
    printf("VIRGL2 capset: version=%u size=%u\n", cap_ver, cap_sz);

    int ok = (cap_sz > 0);
    printf("RESULT: %s\n", ok ? "PASS — vrend initialized on zink-on-KK, VIRGL2 caps present"
                              : "init ok but VIRGL2 capset empty (vrend GL caps not enumerated)");
    virgl_renderer_cleanup(NULL);
    return ok ? 0 : 1;
}
