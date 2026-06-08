// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Minimal GBM+EGL+GLES2+KMS draw probe for bug A (#31).
// Renders a BLUE clear + ONE big RED 2D triangle, NO depth buffer, then drmModeSetCrtc
// and holds, so the frame stays scanned out (→ venus SET_SCANOUT_BLOB → global IOSurface)
// for host-side `iosdump` to read.
//   blue only        => a simple 2D no-depth draw ALSO produces no fragments (bug A is broad)
//   blue + red tri    => 2D draws DO land  => bug A is depth/3D-specific
// Build: gcc tri.c -o tri -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm
// Run (patched zink env, gdm stopped so card0 master is free): ./tri
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <gbm.h>
#include <EGL/egl.h>
#include <GLES2/gl2.h>

static void check_shader(GLuint s, const char *tag) {
    GLint ok = 0; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    char log[1024]; GLsizei n = 0; glGetShaderInfoLog(s, sizeof log, &n, log);
    printf("shader %s: compile=%d %.*s\n", tag, ok, n, log);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0); // unbuffered so output survives a crash
    int fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("open card0"); return 1; }

    drmModeRes *res = drmModeGetResources(fd);
    drmModeConnector *conn = NULL;
    for (int i = 0; i < res->count_connectors; i++) {
        drmModeConnector *c = drmModeGetConnector(fd, res->connectors[i]);
        if (c && c->connection == DRM_MODE_CONNECTED && c->count_modes > 0) { conn = c; break; }
        if (c) drmModeFreeConnector(c);
    }
    if (!conn) { fprintf(stderr, "no connected connector\n"); return 1; }

    // Pick the 1280x800 mode our scanout actually honors (else modes[0]).
    drmModeModeInfo mode = conn->modes[0];
    for (int i = 0; i < conn->count_modes; i++)
        if (conn->modes[i].hdisplay == 1280 && conn->modes[i].vdisplay == 800) { mode = conn->modes[i]; break; }

    drmModeEncoder *enc = drmModeGetEncoder(fd, conn->encoder_id ? conn->encoder_id : conn->encoders[0]);
    uint32_t crtc_id = (enc && enc->crtc_id) ? enc->crtc_id : res->crtcs[0];
    uint32_t conn_id = conn->connector_id;
    printf("mode %dx%d crtc %u conn %u\n", mode.hdisplay, mode.vdisplay, crtc_id, conn_id);

    struct gbm_device *gbm = gbm_create_device(fd);
    struct gbm_surface *gs = gbm_surface_create(gbm, mode.hdisplay, mode.vdisplay,
        GBM_FORMAT_XRGB8888, GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING);
    if (!gs) { fprintf(stderr, "gbm_surface_create failed\n"); return 1; }

    EGLDisplay dpy = eglGetDisplay((EGLNativeDisplayType)gbm);
    eglInitialize(dpy, NULL, NULL);
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cfga[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8,
                      EGL_BLUE_SIZE, 8, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE };
    // NOTE: no EGL_DEPTH_SIZE requested => no depth buffer.
    EGLConfig cfgs[64]; EGLint n = 0;
    eglChooseConfig(dpy, cfga, cfgs, 64, &n);
    printf("eglChooseConfig n=%d\n", n);
    // Pick the config whose native visual matches the GBM surface format (XRGB8888) — else swap
    // produces no front buffer and lock_front_buffer crashes.
    EGLConfig cfg = cfgs[0];
    for (int i = 0; i < n; i++) {
        EGLint vid = 0;
        eglGetConfigAttrib(dpy, cfgs[i], EGL_NATIVE_VISUAL_ID, &vid);
        if (vid == (EGLint)GBM_FORMAT_XRGB8888) { cfg = cfgs[i]; printf("matched config %d\n", i); break; }
    }
    EGLint ctxa[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxa);
    EGLSurface surf = eglCreateWindowSurface(dpy, cfg, (EGLNativeWindowType)gs, NULL);
    eglMakeCurrent(dpy, surf, surf, ctx);
    printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));

    const char *vs = "attribute vec2 p; void main(){ gl_Position = vec4(p, 0.0, 1.0); }";
    const char *fs = "precision mediump float; void main(){ gl_FragColor = vec4(0.0, 1.0, 0.0, 1.0); }"; // green
    GLuint v = glCreateShader(GL_VERTEX_SHADER);   glShaderSource(v, 1, &vs, 0); glCompileShader(v); check_shader(v, "vs");
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER); glShaderSource(f, 1, &fs, 0); glCompileShader(f); check_shader(f, "fs");
    GLuint prog = glCreateProgram();
    glAttachShader(prog, v); glAttachShader(prog, f);
    glBindAttribLocation(prog, 0, "p");
    glLinkProgram(prog);
    GLint linked = 0; glGetProgramiv(prog, GL_LINK_STATUS, &linked); printf("link=%d\n", linked);
    glUseProgram(prog);

    // FULLSCREEN quad (two triangles spanning all of NDC) — covers the whole framebuffer, so it
    // cannot be "missed" by position; disambiguates no-fragments-at-all vs geometry-off-screen.
    GLfloat quad[] = { -1.f,-1.f,  1.f,-1.f,  -1.f,1.f,   -1.f,1.f,  1.f,-1.f,  1.f,1.f };
    glViewport(0, 0, mode.hdisplay, mode.vdisplay);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);
    GLint vp[4] = {0}, sb[4] = {0};
    glGetIntegerv(GL_VIEWPORT, vp);
    glGetIntegerv(GL_SCISSOR_BOX, sb);
    printf("GL_VIEWPORT=%d,%d,%d,%d  GL_SCISSOR_BOX=%d,%d,%d,%d  scissor_test=%d\n",
           vp[0],vp[1],vp[2],vp[3], sb[0],sb[1],sb[2],sb[3], glIsEnabled(GL_SCISSOR_TEST));
    glClearColor(0.0f, 0.0f, 1.0f, 1.0f); // BLUE clear
    glClear(GL_COLOR_BUFFER_BIT);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, quad);
    glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES, 0, 6); // GREEN fullscreen quad
    glFinish();
    printf("glGetError=0x%x\n", glGetError());

    EGLBoolean sw = eglSwapBuffers(dpy, surf);
    printf("eglSwapBuffers=%d eglErr=0x%x\n", sw, eglGetError());
    struct gbm_bo *bo = gbm_surface_lock_front_buffer(gs);
    if (!bo) { fprintf(stderr, "lock_front_buffer returned NULL (swap produced no buffer)\n"); return 1; }
    uint32_t handle = gbm_bo_get_handle(bo).u32, stride = gbm_bo_get_stride(bo), fb = 0;
    int r = drmModeAddFB(fd, mode.hdisplay, mode.vdisplay, 24, 32, stride, handle, &fb);
    printf("addfb=%d fb=%u stride=%u\n", r, fb, stride);
    r = drmModeSetCrtc(fd, crtc_id, fb, 0, 0, &conn_id, 1, &mode);
    printf("setcrtc=%d\n", r);

    printf("holding 600s for eyeball/iosdump...\n"); fflush(stdout);
    sleep(600);
    return 0;
}
