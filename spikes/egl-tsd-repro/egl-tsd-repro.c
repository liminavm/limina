// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* Minimal reproducer: SIGSEGV in __nptl_deallocate_tsd at thread exit after
 * surfaceless EGL init + eglTerminate, with GL provided by zink on venus.
 *
 * Root cause (confirmed by matching the fault PC against the run's link maps):
 * the venus Vulkan ICD (libvulkan_virtio.so) registers a pthread TLS key whose
 * destructor is vn_tls_free (src/virtio/vulkan/vn_common.c). When zink destroys
 * its VkInstance during eglTerminate, the Vulkan loader dlclose()s the ICD, but
 * the key stays registered. glibc's pthread_key_create does not pin the DSO the
 * destructor lives in (unlike __cxa_thread_atexit_impl), so when the thread that
 * used EGL later exits, __nptl_deallocate_tsd calls vn_tls_free through a
 * pointer into unmapped memory -> SIGSEGV.
 *
 * Observed on: Fedora 44 aarch64 VM (Apple M4 Pro host), mesa 26.1.3
 * (GL_RENDERER: "zink Vulkan 1.3(Virtio-GPU Venus (Apple M4 Pro)
 * (MESA_KOSMICKRISP))"), glibc 2.43. Original sighting: niri's headless
 * `egl_*` tests (smithay GlesRenderer on EGL_PLATFORM_SURFACELESS_MESA),
 * which run each test on its own thread.
 *
 * Build:   cc -o egl-tsd-repro egl-tsd-repro.c -ldl -lpthread
 * Run:     ./egl-tsd-repro          -> segfault after "teardown complete"
 * Workaround (pins the ICD so the destructor stays mapped):
 *          LD_PRELOAD=/usr/lib64/libvulkan_virtio.so ./egl-tsd-repro   -> clean
 *
 * Everything is dlopen/dlsym'd so no EGL headers or -lEGL are needed.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>

typedef void *EGLDisplay;
typedef void *EGLContext;
typedef void *EGLConfig;
typedef unsigned int EGLBoolean;
typedef int32_t EGLint;

#define EGL_NO_DISPLAY ((EGLDisplay)0)
#define EGL_NO_CONTEXT ((EGLContext)0)
#define EGL_NO_SURFACE ((void *)0)
#define EGL_PLATFORM_SURFACELESS_MESA 0x31DD
#define EGL_SURFACE_TYPE 0x3033
#define EGL_RENDERABLE_TYPE 0x3040
#define EGL_OPENGL_ES2_BIT 0x0004
#define EGL_NONE 0x3038
#define EGL_OPENGL_ES_API 0x30A0
#define EGL_CONTEXT_CLIENT_VERSION 0x3098
#define GL_RENDERER 0x1F01
#define GL_VERSION 0x1F02

typedef void (*proc_t)(void);
static proc_t (*GetProcAddress)(const char *);
static EGLBoolean (*Initialize)(EGLDisplay, EGLint *, EGLint *);
static EGLBoolean (*BindAPI)(unsigned int);
static EGLBoolean (*ChooseConfig)(EGLDisplay, const EGLint *, EGLConfig *, EGLint, EGLint *);
static EGLContext (*CreateContext)(EGLDisplay, EGLConfig, EGLContext, const EGLint *);
static EGLBoolean (*MakeCurrent)(EGLDisplay, void *, void *, EGLContext);
static EGLBoolean (*DestroyContext)(EGLDisplay, EGLContext);
static EGLBoolean (*Terminate)(EGLDisplay);

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "FAILED: %s\n", #x); exit(2); } } while (0)

static void *worker(void *arg)
{
    (void)arg;
    void *libegl = dlopen("libEGL.so.1", RTLD_NOW | RTLD_GLOBAL);
    CHECK(libegl);

    GetProcAddress = (proc_t (*)(const char *))dlsym(libegl, "eglGetProcAddress");
    CHECK(GetProcAddress);
    Initialize = (void *)dlsym(libegl, "eglInitialize");
    BindAPI = (void *)dlsym(libegl, "eglBindAPI");
    ChooseConfig = (void *)dlsym(libegl, "eglChooseConfig");
    CreateContext = (void *)dlsym(libegl, "eglCreateContext");
    MakeCurrent = (void *)dlsym(libegl, "eglMakeCurrent");
    DestroyContext = (void *)dlsym(libegl, "eglDestroyContext");
    Terminate = (void *)dlsym(libegl, "eglTerminate");

    EGLDisplay (*GetPlatformDisplayEXT)(unsigned int, void *, const EGLint *) =
        (void *)GetProcAddress("eglGetPlatformDisplayEXT");
    CHECK(GetPlatformDisplayEXT);

    EGLDisplay dpy = GetPlatformDisplayEXT(EGL_PLATFORM_SURFACELESS_MESA, NULL, NULL);
    CHECK(dpy != EGL_NO_DISPLAY);
    CHECK(Initialize(dpy, NULL, NULL));
    CHECK(BindAPI(EGL_OPENGL_ES_API));

    const EGLint cfg_attribs[] = {
        EGL_SURFACE_TYPE, 0,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_NONE,
    };
    EGLConfig cfg;
    EGLint n = 0;
    CHECK(ChooseConfig(dpy, cfg_attribs, &cfg, 1, &n) && n > 0);

    const EGLint ctx_attribs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = CreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attribs);
    CHECK(ctx != EGL_NO_CONTEXT);
    CHECK(MakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx));

    const unsigned char *(*glGetString)(unsigned int) =
        (void *)GetProcAddress("glGetString");
    printf("GL_RENDERER: %s\nGL_VERSION:  %s\n",
           glGetString(GL_RENDERER), glGetString(GL_VERSION));

    CHECK(MakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT));
    CHECK(DestroyContext(dpy, ctx));
    CHECK(Terminate(dpy));

    printf("teardown complete, exiting thread\n");
    fflush(stdout);
    return NULL; /* SIGSEGV here, in __nptl_deallocate_tsd -> vn_tls_free */
}

int main(void)
{
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    pthread_join(t, NULL);
    printf("thread joined cleanly — no repro\n");
    return 0;
}
