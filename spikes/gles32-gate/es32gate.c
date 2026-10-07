// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Which of Mesa's GLES 3.2 requirements host zink-on-KK misses.
//
// Mesa grants an ES 3.2 context only when every extension flag in compute_version_es2()
// (src/mesa/main/version.c, `ver_3_2`) is set. This creates the best ES context the driver
// offers and a desktop GL context, prints both versions, and reports each flag by the
// extension string that exposes it: the OES/KHR/EXT names from the ES context, the ARB names
// from the desktop one (ES 3.1 core absorbs those and lists no string for them).
//
// Build and run with run.sh (it points EGL at the shared zink-on-KK build).
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl32.h>
#include <stdio.h>
#include <string.h>

static PFNGLGETSTRINGPROC pGetString;
static PFNGLGETSTRINGIPROC pGetStringi;
static PFNGLGETINTEGERVPROC pGetIntegerv;

#define MAX_EXTS 1024
static const char *es_exts[MAX_EXTS], *gl_exts[MAX_EXTS];
static int n_es, n_gl;

static int collect(const char **out) {
    GLint n = 0;
    pGetIntegerv(GL_NUM_EXTENSIONS, &n);
    int k = 0;
    for (GLint i = 0; i < n && k < MAX_EXTS; i++)
        out[k++] = strdup((const char *)pGetStringi(GL_EXTENSIONS, i));
    return k;
}

static int has(const char **list, int n, const char *name) {
    for (int i = 0; i < n; i++)
        if (!strcmp(list[i], name))
            return 1;
    return 0;
}

static EGLContext make(EGLDisplay dpy, EGLenum api, EGLint renderable, const EGLint *const *tries,
                       int ntries) {
    if (!eglBindAPI(api))
        return EGL_NO_CONTEXT;
    const EGLint cfga[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, renderable,
                           EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfga, &cfg, 1, &n) || n == 0)
        return EGL_NO_CONTEXT;
    for (int i = 0; i < ntries; i++) {
        EGLContext c = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, tries[i]);
        if (c != EGL_NO_CONTEXT)
            return c;
    }
    return EGL_NO_CONTEXT;
}

struct req {
    const char *mesa_flag;
    const char *es_name;  // looked up in the ES context, or NULL
    const char *gl_name;  // looked up in the desktop context, or NULL
};

static const struct req reqs[] = {
    {"ARB_shader_atomic_counters", NULL, "GL_ARB_shader_atomic_counters"},
    {"ARB_shader_image_load_store", NULL, "GL_ARB_shader_image_load_store"},
    {"ARB_shader_image_size", NULL, "GL_ARB_shader_image_size"},
    {"ARB_shader_storage_buffer_object", NULL, "GL_ARB_shader_storage_buffer_object"},
    {"EXT_color_buffer_float", "GL_EXT_color_buffer_float", NULL},
    {"EXT_draw_buffers2", "GL_OES_draw_buffers_indexed", "GL_EXT_draw_buffers2"},
    {"KHR_blend_equation_advanced", "GL_KHR_blend_equation_advanced", NULL},
    {"KHR_robustness", "GL_KHR_robustness", NULL},
    {"KHR_texture_compression_astc_ldr", "GL_KHR_texture_compression_astc_ldr", NULL},
    {"OES_copy_image", "GL_OES_copy_image", NULL},
    {"ARB_draw_buffers_blend", "GL_OES_draw_buffers_indexed", "GL_ARB_draw_buffers_blend"},
    {"ARB_draw_elements_base_vertex", "GL_OES_draw_elements_base_vertex",
     "GL_ARB_draw_elements_base_vertex"},
    {"OES_geometry_shader", "GL_OES_geometry_shader", NULL},
    {"OES_primitive_bounding_box", "GL_OES_primitive_bounding_box", NULL},
    {"OES_sample_variables", "GL_OES_sample_variables", NULL},
    {"ARB_tessellation_shader", "GL_OES_tessellation_shader", "GL_ARB_tessellation_shader"},
    {"OES_texture_buffer", "GL_OES_texture_buffer", NULL},
    {"OES_texture_cube_map_array", "GL_OES_texture_cube_map_array", NULL},
    {"ARB_texture_stencil8", "GL_OES_texture_stencil8", "GL_ARB_texture_stencil8"},
};

int main(void) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC getDpy =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = getDpy ? getDpy(EGL_PLATFORM_SURFACELESS_MESA, (void *)0, NULL)
                            : eglGetDisplay(EGL_DEFAULT_DISPLAY);
    EGLint maj, min;
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, &maj, &min)) {
        fprintf(stderr, "FAIL: EGL init (0x%x)\n", eglGetError());
        return 2;
    }
    pGetString = (PFNGLGETSTRINGPROC)eglGetProcAddress("glGetString");
    pGetStringi = (PFNGLGETSTRINGIPROC)eglGetProcAddress("glGetStringi");
    pGetIntegerv = (PFNGLGETINTEGERVPROC)eglGetProcAddress("glGetIntegerv");

    const EGLint es32[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 2, EGL_NONE};
    const EGLint es3[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE};
    const EGLint *es_tries[] = {es32, es3};
    EGLContext es = make(dpy, EGL_OPENGL_ES_API, EGL_OPENGL_ES3_BIT, es_tries, 2);
    if (es == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, es)) {
        fprintf(stderr, "FAIL: no ES 3 context (0x%x)\n", eglGetError());
        return 2;
    }
    printf("ES context : %s\n", (const char *)pGetString(GL_VERSION));
    printf("renderer   : %s\n", (const char *)pGetString(GL_RENDERER));
    n_es = collect(es_exts);

    const EGLint core[] = {EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 6,
                           EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                           EGL_NONE};
    const EGLint core32[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 2,
                             EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                             EGL_NONE};
    const EGLint bare[] = {EGL_NONE};
    const EGLint *gl_tries[] = {core, core32, bare};
    EGLContext gl = make(dpy, EGL_OPENGL_API, EGL_OPENGL_BIT, gl_tries, 3);
    if (gl != EGL_NO_CONTEXT && eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, gl)) {
        printf("GL context : %s\n", (const char *)pGetString(GL_VERSION));
        n_gl = collect(gl_exts);
    } else {
        printf("GL context : none (0x%x); ARB rows read as missing\n", eglGetError());
    }
    printf("extensions : %d ES, %d GL\n\n", n_es, n_gl);

    int missing = 0;
    for (size_t i = 0; i < sizeof reqs / sizeof reqs[0]; i++) {
        const struct req *r = &reqs[i];
        int ok = (r->es_name && has(es_exts, n_es, r->es_name)) ||
                 (r->gl_name && has(gl_exts, n_gl, r->gl_name));
        missing += !ok;
        printf("%-4s %-34s (%s%s%s)\n", ok ? "ok" : "MISS", r->mesa_flag,
               r->es_name ? r->es_name : "", r->es_name && r->gl_name ? " | " : "",
               r->gl_name ? r->gl_name : "");
    }
    printf("\n%d of %zu ES 3.2 requirements missing\n", missing, sizeof reqs / sizeof reqs[0]);
    return 0;
}
