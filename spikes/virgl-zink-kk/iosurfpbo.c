// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// vrend-IOSurface-scanout spike (docs/design/vrend-iosurface-scanout.md, plan A1):
// prove that on zink-on-KosmicKrisp (surfaceless EGL, GLES — the exact vrend host config)
// a glReadPixels into a GL_AMD_pinned_memory PBO whose client memory is an IOSurface's
// base address lands the rendered pixels in the IOSurface. Pixel-verify via
// IOSurfaceLock readback (in-process; run under LIMINA_GLOBAL_SCANOUT for the iosdump
// cross-process oracle too). Also probes the two fence paths vrend can use
// (EGL_ANDROID_native_fence_sync vs glFenceSync — the former is suspected
// advertised-but-broken on KK), and times the A1 blit at 2560×1440.
//
// Build/run: run-probe.sh iosurfpbo
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <IOSurface/IOSurfaceRef.h>
#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD
#define GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD 0x9160
#endif

#define CHECK(cond, msg)                                                        \
    do {                                                                        \
        if (!(cond)) {                                                          \
            fprintf(stderr, "FAIL: %s (egl 0x%x gl 0x%x)\n", msg,               \
                    eglGetError(), glGetError());                               \
            return 2;                                                           \
        }                                                                       \
    } while (0)

static IOSurfaceRef iosurface_create(int w, int h) {
    int bpr = w * 4;
    CFMutableDictionaryRef d = CFDictionaryCreateMutable(
        NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
#define SETI(k, v)                                                              \
    do {                                                                        \
        int val = (v);                                                          \
        CFNumberRef n = CFNumberCreate(NULL, kCFNumberIntType, &val);           \
        CFDictionarySetValue(d, k, n);                                          \
        CFRelease(n);                                                           \
    } while (0)
    SETI(kIOSurfaceWidth, w);
    SETI(kIOSurfaceHeight, h);
    SETI(kIOSurfaceBytesPerElement, 4);
    SETI(kIOSurfaceBytesPerRow, bpr);
    SETI(kIOSurfacePixelFormat, 'RGBA'); // matches GLES glReadPixels(GL_RGBA)
#undef SETI
    if (getenv("LIMINA_GLOBAL_SCANOUT"))
        CFDictionarySetValue(d, kIOSurfaceIsGlobal, kCFBooleanTrue);
    IOSurfaceRef io = IOSurfaceCreate(d);
    CFRelease(d);
    return io;
}

static double mach_ms(uint64_t dt) {
    static mach_timebase_info_data_t tb;
    if (!tb.denom)
        mach_timebase_info(&tb);
    return (double)dt * tb.numer / tb.denom / 1e6;
}

// Render a frame with distinct regions into the currently bound FBO.
static void draw_pattern(int w, int h) {
    glDisable(GL_SCISSOR_TEST);
    glClearColor(0.2f, 0.4f, 0.6f, 1.0f); // body: (51, 102, 153, 255)
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_SCISSOR_TEST);
    glScissor(0, 0, w / 4, h / 4); // bottom-left quarter: (255, 26, 26, 255)
    glClearColor(1.0f, 0.1f, 0.1f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDisable(GL_SCISSOR_TEST);
}

static int px_eq(const unsigned char *p, int r, int g, int b) {
    // Allow ±2 for rounding in any blit path.
    return abs(p[0] - r) <= 2 && abs(p[1] - g) <= 2 && abs(p[2] - b) <= 2;
}

int main(void) {
    // --- context bring-up: identical shape to eglprobe (surfaceless, GLES3) ---
    PFNEGLGETPLATFORMDISPLAYEXTPROC getPlatformDisplay =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    CHECK(getPlatformDisplay, "eglGetPlatformDisplayEXT");
    EGLDisplay dpy =
        getPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    CHECK(dpy != EGL_NO_DISPLAY, "surfaceless display");
    CHECK(eglInitialize(dpy, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_ES_API), "bind GLES");
    static const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE};
    EGLConfig cfg;
    EGLint ncfg = 0;
    CHECK(eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &ncfg) && ncfg, "choose config");
    static const EGLint ctx_attrs[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    CHECK(ctx != EGL_NO_CONTEXT, "create context");
    CHECK(eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx), "make current");
    printf("GL_RENDERER    : %s\n", glGetString(GL_RENDERER));
    printf("GL_VERSION     : %s\n", glGetString(GL_VERSION));

    // --- Q1: is pinned memory exposed? ---
    const char *exts = (const char *)glGetString(GL_EXTENSIONS);
    int pinned = exts && strstr(exts, "GL_AMD_pinned_memory") != NULL;
    printf("GL_AMD_pinned_memory : %s\n", pinned ? "EXPOSED" : "MISSING");

    // --- Q3: fence paths (report both; vrend picks EGL when ANDROID_native_fence_sync) ---
    const char *eglexts = eglQueryString(dpy, EGL_EXTENSIONS);
    int has_native_fence =
        eglexts && strstr(eglexts, "EGL_ANDROID_native_fence_sync") != NULL;
    printf("EGL_ANDROID_native_fence_sync : %s\n",
           has_native_fence ? "ADVERTISED" : "absent");
    PFNEGLCREATESYNCKHRPROC pCreateSync =
        (PFNEGLCREATESYNCKHRPROC)eglGetProcAddress("eglCreateSyncKHR");
    PFNEGLCLIENTWAITSYNCKHRPROC pWaitSync =
        (PFNEGLCLIENTWAITSYNCKHRPROC)eglGetProcAddress("eglClientWaitSyncKHR");
    PFNEGLDESTROYSYNCKHRPROC pDestroySync =
        (PFNEGLDESTROYSYNCKHRPROC)eglGetProcAddress("eglDestroySyncKHR");
    if (has_native_fence && pCreateSync) {
        EGLSyncKHR s = pCreateSync(dpy, EGL_SYNC_NATIVE_FENCE_ANDROID, NULL);
        if (s == EGL_NO_SYNC_KHR) {
            printf("EGL native fence create : RETURNS EGL_NO_SYNC (broken as suspected, "
                   "egl 0x%x) -> vrend must use glFenceSync\n",
                   eglGetError());
        } else {
            glFlush();
            EGLint r = pWaitSync(dpy, s, EGL_SYNC_FLUSH_COMMANDS_BIT_KHR, 100000000ull);
            printf("EGL native fence create : OK, wait=0x%x (%s)\n", r,
                   r == EGL_CONDITION_SATISFIED_KHR ? "SATISFIED" : "not satisfied");
            pDestroySync(dpy, s);
        }
    }
    GLsync gs = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    if (gs) {
        glFlush();
        GLenum r = glClientWaitSync(gs, GL_SYNC_FLUSH_COMMANDS_BIT, 100000000ull);
        printf("glFenceSync             : OK, wait=0x%x (%s)\n", r,
               (r == GL_ALREADY_SIGNALED || r == GL_CONDITION_SATISFIED) ? "SATISFIED"
                                                                         : "NOT SIGNALED");
        glDeleteSync(gs);
    } else {
        printf("glFenceSync             : FAILED to create\n");
    }

    if (!pinned) {
        printf("RESULT: PINNED-MISSING (A1 needs GL_AMD_pinned_memory; stop here)\n");
        return 1;
    }

    // --- Q2: render, readback into pinned PBO over IOSurface bytes, verify ---
    const int W = 512, H = 512;
    IOSurfaceRef io = iosurface_create(W, H);
    CHECK(io, "IOSurfaceCreate");
    void *base = IOSurfaceGetBaseAddress(io);
    size_t bpr = IOSurfaceGetBytesPerRow(io);
    size_t alloc = IOSurfaceGetAllocSize(io);
    printf("IOSurface id=%u base=%p bytesPerRow=%zu alloc=%zu\n", IOSurfaceGetID(io),
           base, bpr, alloc);

    GLuint fbo, tex;
    glGenFramebuffers(1, &fbo);
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, W, H);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "fbo complete");
    glViewport(0, 0, W, H);
    draw_pattern(W, H);

    GLuint pbo;
    glGenBuffers(1, &pbo);
    glBindBuffer(GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD, pbo);
    glBufferData(GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD, (GLsizeiptr)alloc, base,
                 GL_DYNAMIC_DRAW); // AMD_pinned_memory: client memory IS the store
    GLenum err = glGetError();
    if (err != GL_NO_ERROR) {
        printf("RESULT: PINNED-BUFFERDATA-FAILED (gl 0x%x — alignment %p? alloc %zu)\n",
               err, base, alloc);
        return 1;
    }
    glBindBuffer(GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD, 0);

    glBindBuffer(GL_PIXEL_PACK_BUFFER, pbo);
    glPixelStorei(GL_PACK_ROW_LENGTH, (GLint)(bpr / 4));
    glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, (void *)0);
    CHECK(glGetError() == GL_NO_ERROR, "glReadPixels into pinned PBO");
    glFinish();

    IOSurfaceLock(io, kIOSurfaceLockReadOnly, NULL);
    const unsigned char *p = base;
    // GL reads bottom-up; row 0 of the readback = GL y=0 = the scissored corner.
    const unsigned char *corner = p + 4 * 8 + bpr * 8;               // in the quarter
    const unsigned char *body = p + 4 * (W - 8) + bpr * (H - 8);     // opposite corner
    int ok_corner = px_eq(corner, 255, 26, 26);
    int ok_body = px_eq(body, 51, 102, 153);
    printf("pixel corner(8,8)=(%u,%u,%u,%u) expect ~(255,26,26) %s\n", corner[0],
           corner[1], corner[2], corner[3], ok_corner ? "OK" : "MISMATCH");
    printf("pixel body(%d,%d)=(%u,%u,%u,%u) expect ~(51,102,153) %s\n", W - 8, H - 8,
           body[0], body[1], body[2], body[3], ok_body ? "OK" : "MISMATCH");
    IOSurfaceUnlock(io, kIOSurfaceLockReadOnly, NULL);

    // --- Q4: A1 blit cost at 2560×1440 ---
    const int BW = 2560, BH = 1440;
    IOSurfaceRef bio = iosurface_create(BW, BH);
    CHECK(bio, "big IOSurfaceCreate");
    GLuint bfbo, btex, bpbo;
    glGenFramebuffers(1, &bfbo);
    glGenTextures(1, &btex);
    glBindTexture(GL_TEXTURE_2D, btex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, BW, BH);
    glBindFramebuffer(GL_FRAMEBUFFER, bfbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, btex, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "big fbo complete");
    glViewport(0, 0, BW, BH);
    glGenBuffers(1, &bpbo);
    glBindBuffer(GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD, bpbo);
    glBufferData(GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD,
                 (GLsizeiptr)IOSurfaceGetAllocSize(bio), IOSurfaceGetBaseAddress(bio),
                 GL_DYNAMIC_DRAW);
    CHECK(glGetError() == GL_NO_ERROR, "big pinned BufferData");
    glBindBuffer(GL_EXTERNAL_VIRTUAL_MEMORY_BUFFER_AMD, 0);
    glBindBuffer(GL_PIXEL_PACK_BUFFER, bpbo);
    glPixelStorei(GL_PACK_ROW_LENGTH, (GLint)(IOSurfaceGetBytesPerRow(bio) / 4));

    const int N = 60;
    draw_pattern(BW, BH);
    glFinish();
    uint64_t t0 = mach_absolute_time();
    for (int i = 0; i < N; i++) {
        draw_pattern(BW, BH); // fresh GPU work each frame, like a compositor
        glReadPixels(0, 0, BW, BH, GL_RGBA, GL_UNSIGNED_BYTE, (void *)0);
        glFinish(); // worst case: full sync per frame (fence-present would overlap)
    }
    uint64_t t1 = mach_absolute_time();
    printf("A1 blit 2560x1440 (draw+readpixels+finish): %.2f ms/frame over %d frames\n",
           mach_ms(t1 - t0) / N, N);

    int pass = ok_corner && ok_body;
    printf("RESULT: %s\n", pass ? "PASS" : "PIXEL-MISMATCH");
    return pass ? 0 : 1;
}
