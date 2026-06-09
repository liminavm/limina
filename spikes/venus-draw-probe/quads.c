// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Controlled vehicle for the tier-2 "stretched-triangle / stale-icon" defect.
//
// tri.c renders ONE quad via glDrawArrays and it is correct. The seated desktop renders many quads
// via INDEXED triangle-list draws (glDrawElements, the [LIMINA-IDX] probe showed prim=triangle,
// uint16, in-bounds, no restart) and SOME come out with one triangle stretched off to a corner.
// Coherency and index buffers are both proven clean, so this isolates the remaining variable: does
// an indexed grid of quads, whose vertex data we KNOW exactly, render correctly through zink->venus?
//
//   GREEN squares on a BLUE clear, on a regular grid → render path is clean for known data.
//   Any stretched / missing / corner-fanned quad  → reproduced; instrument THIS draw (known data).
//
// Env knobs (bisect the desktop's specifics):
//   QUADS_N=<n>     grid is NxN quads (default 8)  — tests batching / buffer size
//   QUADS_ARRAYS=1  use glDrawArrays (6 verts/quad) instead of indexed — tests indexed-vs-array
//   QUADS_DYN=1     re-upload the vertex buffer every frame for 120 frames (streaming) — tests the
//                   dynamic-upload / buffer-recycle path the desktop uses (static VBO otherwise)
// Build (guest): gcc quads.c -o quads -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm
// Run (patched zink env, gdm stopped / multi-user so card0 master is free): ./quads
// Verify: host-side `swift iosdump.swift <printed scanout id>` — expect a clean green grid.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
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
    setvbuf(stdout, NULL, _IONBF, 0);
    int N = getenv("QUADS_N") ? atoi(getenv("QUADS_N")) : 8;
    int use_arrays = getenv("QUADS_ARRAYS") ? 1 : 0;
    int dynamic = getenv("QUADS_DYN") ? 1 : 0;
    // QUADS_STREAM: the REAL Cogl pattern — each quad sub-uploaded into a recycled ring buffer at an
    // advancing offset then IMMEDIATELY drawn, no glFinish, many frames. Tiny writes whose last vertex
    // sits closest to submit → the window where a write-visibility race on the last vertex would bite.
    int stream = getenv("QUADS_STREAM") ? 1 : 0;
    if (stream) use_arrays = 0;             // stream needs the 4-vert-per-quad indexed layout
    if (N < 1) N = 1;
    printf("grid N=%d quads=%d mode=%s%s%s\n", N, N*N, use_arrays?"arrays":"indexed",
           dynamic?" +dynamic":"", stream?" +stream":"");

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
    drmModeModeInfo mode = conn->modes[0];
    for (int i = 0; i < conn->count_modes; i++)
        if (conn->modes[i].hdisplay == 1280 && conn->modes[i].vdisplay == 800) { mode = conn->modes[i]; break; }
    drmModeEncoder *enc = drmModeGetEncoder(fd, conn->encoder_id ? conn->encoder_id : conn->encoders[0]);
    uint32_t crtc_id = (enc && enc->crtc_id) ? enc->crtc_id : res->crtcs[0];
    uint32_t conn_id = conn->connector_id;

    struct gbm_device *gbm = gbm_create_device(fd);
    struct gbm_surface *gs = gbm_surface_create(gbm, mode.hdisplay, mode.vdisplay,
        GBM_FORMAT_XRGB8888, GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING);
    if (!gs) { fprintf(stderr, "gbm_surface_create failed\n"); return 1; }
    EGLDisplay dpy = eglGetDisplay((EGLNativeDisplayType)gbm);
    eglInitialize(dpy, NULL, NULL);
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cfga[] = { EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8,
                      EGL_BLUE_SIZE, 8, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE };
    EGLConfig cfgs[64]; EGLint n = 0;
    eglChooseConfig(dpy, cfga, cfgs, 64, &n);
    EGLConfig cfg = cfgs[0];
    for (int i = 0; i < n; i++) {
        EGLint vid = 0; eglGetConfigAttrib(dpy, cfgs[i], EGL_NATIVE_VISUAL_ID, &vid);
        if (vid == (EGLint)GBM_FORMAT_XRGB8888) { cfg = cfgs[i]; break; }
    }
    EGLint ctxa[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxa);
    EGLSurface surf = eglCreateWindowSurface(dpy, cfg, (EGLNativeWindowType)gs, NULL);
    eglMakeCurrent(dpy, surf, surf, ctx);
    printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));

    const char *vs = "attribute vec2 p; void main(){ gl_Position = vec4(p, 0.0, 1.0); }";
    const char *fs = "precision mediump float; void main(){ gl_FragColor = vec4(0.0,1.0,0.0,1.0); }";
    GLuint v = glCreateShader(GL_VERTEX_SHADER);   glShaderSource(v,1,&vs,0); glCompileShader(v); check_shader(v,"vs");
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER); glShaderSource(f,1,&fs,0); glCompileShader(f); check_shader(f,"fs");
    GLuint prog = glCreateProgram();
    glAttachShader(prog,v); glAttachShader(prog,f); glBindAttribLocation(prog,0,"p"); glLinkProgram(prog);
    GLint linked=0; glGetProgramiv(prog,GL_LINK_STATUS,&linked); printf("link=%d\n",linked);
    glUseProgram(prog);

    // Build the grid. Each quad: 4 verts (TL,TR,BL,BR) at known NDC, indices {0,1,2, 2,1,3}.
    int Q = N*N;
    float *varr = malloc((use_arrays ? Q*6*2 : Q*4*2) * sizeof(float));
    uint16_t *iarr = use_arrays ? NULL : malloc(Q*6 * sizeof(uint16_t));
    float cell = 2.0f / N, gap = cell*0.15f, s = cell - gap; // square side with a gap so quads are distinct
    int vi = 0, ii = 0;
    for (int gy = 0; gy < N; gy++) for (int gx = 0; gx < N; gx++) {
        float x0 = -1.0f + gx*cell + gap*0.5f, y0 = -1.0f + gy*cell + gap*0.5f;
        float x1 = x0 + s, y1 = y0 + s;
        float corners[4][2] = { {x0,y1},{x1,y1},{x0,y0},{x1,y0} }; // TL,TR,BL,BR
        if (use_arrays) {
            int order[6] = {0,1,2, 2,1,3};
            for (int k=0;k<6;k++){ varr[vi++]=corners[order[k]][0]; varr[vi++]=corners[order[k]][1]; }
        } else {
            int base = (gy*N+gx)*4;
            for (int k=0;k<4;k++){ varr[vi++]=corners[k][0]; varr[vi++]=corners[k][1]; }
            int o[6]={0,1,2,2,1,3};
            for (int k=0;k<6;k++) iarr[ii++]=base+o[k];
        }
    }

    GLuint vbo=0, ibo=0;
    // Ring buffer (stream mode) holds RINGQ quads; per-quad sub-uploads advance + wrap to force recycle.
    int RINGQ = 64; size_t ringBytes = (size_t)RINGQ*4*2*sizeof(float);
    glGenBuffers(1,&vbo); glBindBuffer(GL_ARRAY_BUFFER,vbo);
    if (stream) glBufferData(GL_ARRAY_BUFFER, ringBytes, NULL, GL_DYNAMIC_DRAW);
    else        glBufferData(GL_ARRAY_BUFFER, (use_arrays?Q*6*2:Q*4*2)*sizeof(float), varr, dynamic?GL_DYNAMIC_DRAW:GL_STATIC_DRAW);
    if (!use_arrays) {
        glGenBuffers(1,&ibo); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,ibo);
        if (stream) { uint16_t si[6]={0,1,2,2,1,3}; glBufferData(GL_ELEMENT_ARRAY_BUFFER, sizeof si, si, GL_STATIC_DRAW); }
        else        glBufferData(GL_ELEMENT_ARRAY_BUFFER, Q*6*sizeof(uint16_t), iarr, GL_STATIC_DRAW);
    }
    glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,0);
    glEnableVertexAttribArray(0);

    glViewport(0,0,mode.hdisplay,mode.vdisplay);
    glDisable(GL_DEPTH_TEST); glDisable(GL_CULL_FACE);

    int frames = (dynamic||stream) ? 240 : 1;
    int ringq = 0;
    for (int fr=0; fr<frames; fr++) {
        glClearColor(0.0f,0.0f,1.0f,1.0f); glClear(GL_COLOR_BUFFER_BIT);
        if (stream) {
            // Per quad: sub-upload its 4 verts into the ring at an advancing offset, rebind the attrib
            // base to that offset, draw immediately. No glFinish. The last of the 4 verts is written
            // microseconds before the GPU fetches it — exactly the visibility-race window.
            glBindBuffer(GL_ARRAY_BUFFER,vbo);
            for (int q=0; q<Q; q++) {
                size_t off = (size_t)ringq*4*2*sizeof(float);
                glBufferSubData(GL_ARRAY_BUFFER, off, 4*2*sizeof(float), &varr[q*8]);
                glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,(const void*)off);
                glDrawElements(GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, 0);
                ringq = (ringq+1) % RINGQ;   // wrap → recycle the ring
            }
        } else if (dynamic) { // orphan + full refill each frame (coarse u_upload_mgr)
            glBindBuffer(GL_ARRAY_BUFFER,vbo);
            glBufferData(GL_ARRAY_BUFFER, (use_arrays?Q*6*2:Q*4*2)*sizeof(float), NULL, GL_DYNAMIC_DRAW);
            glBufferSubData(GL_ARRAY_BUFFER, 0, (use_arrays?Q*6*2:Q*4*2)*sizeof(float), varr);
            if (use_arrays) glDrawArrays(GL_TRIANGLES, 0, Q*6);
            else            glDrawElements(GL_TRIANGLES, Q*6, GL_UNSIGNED_SHORT, 0);
        } else {
            if (use_arrays) glDrawArrays(GL_TRIANGLES, 0, Q*6);
            else            glDrawElements(GL_TRIANGLES, Q*6, GL_UNSIGNED_SHORT, 0);
        }
    }
    glFinish();
    printf("glGetError=0x%x\n", glGetError());

    EGLBoolean sw = eglSwapBuffers(dpy, surf);
    printf("eglSwapBuffers=%d\n", sw);
    struct gbm_bo *bo = gbm_surface_lock_front_buffer(gs);
    if (!bo) { fprintf(stderr,"lock_front_buffer NULL\n"); return 1; }
    uint32_t handle = gbm_bo_get_handle(bo).u32, stride = gbm_bo_get_stride(bo), fb = 0;
    int r = drmModeAddFB(fd, mode.hdisplay, mode.vdisplay, 24, 32, stride, handle, &fb);
    r = drmModeSetCrtc(fd, crtc_id, fb, 0, 0, &conn_id, 1, &mode);
    printf("addfb/setcrtc done (r=%d). holding 600s for iosdump...\n", r);
    sleep(600);
    return 0;
}
