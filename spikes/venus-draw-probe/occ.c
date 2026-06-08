// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Occlusion-query probe for bug A (#31): does the draw RASTERIZE any samples?
// Uses tri.c's known-good EGL setup (ES2-renderable config matched to XRGB8888) but creates an
// ES3 CONTEXT so GL_ANY_SAMPLES_PASSED is available. Wraps a fullscreen-quad draw in an occlusion
// query and reads the count. This is independent of the scanout/IOSurface present path.
//   samples_passed > 0  => fragments ARE rasterized => the loss is DOWNSTREAM (blend/write/store/present)
//   samples_passed == 0 => nothing rasterizes        => the loss is UPSTREAM (vtx shader/clip/raster/topology)
// Build: gcc occ.c -o occ -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <gbm.h>
#include <EGL/egl.h>
#include <GLES3/gl3.h>

static void csh(GLuint s, const char *t) {
    GLint ok=0; glGetShaderiv(s,GL_COMPILE_STATUS,&ok);
    char log[1024]; GLsizei n=0; glGetShaderInfoLog(s,sizeof log,&n,log);
    printf("shader %s compile=%d %.*s\n", t, ok, n, log);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("open card0"); return 1; }
    drmModeRes *res = drmModeGetResources(fd);
    drmModeConnector *conn = NULL;
    for (int i=0;i<res->count_connectors;i++){ drmModeConnector *c=drmModeGetConnector(fd,res->connectors[i]);
        if(c && c->connection==DRM_MODE_CONNECTED && c->count_modes>0){conn=c;break;} if(c)drmModeFreeConnector(c);}
    if(!conn){fprintf(stderr,"no connector\n");return 1;}
    drmModeModeInfo mode = conn->modes[0];
    for(int i=0;i<conn->count_modes;i++) if(conn->modes[i].hdisplay==1280&&conn->modes[i].vdisplay==800){mode=conn->modes[i];break;}

    struct gbm_device *gbm=gbm_create_device(fd);
    struct gbm_surface *gs=gbm_surface_create(gbm,mode.hdisplay,mode.vdisplay,GBM_FORMAT_XRGB8888,
        GBM_BO_USE_SCANOUT|GBM_BO_USE_RENDERING);
    if(!gs){fprintf(stderr,"gbm_surface_create failed\n");return 1;}
    EGLDisplay dpy=eglGetDisplay((EGLNativeDisplayType)gbm);
    eglInitialize(dpy,0,0); eglBindAPI(EGL_OPENGL_ES_API);
    // Same ES2-renderable config as tri.c (works), matched to XRGB8888.
    EGLint ca[]={EGL_SURFACE_TYPE,EGL_WINDOW_BIT,EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,
                 EGL_RENDERABLE_TYPE,EGL_OPENGL_ES2_BIT,EGL_NONE};
    EGLConfig cfgs[64]; EGLint n=0; eglChooseConfig(dpy,ca,cfgs,64,&n);
    EGLConfig cfg=cfgs[0];
    for(int i=0;i<n;i++){EGLint v=0; eglGetConfigAttrib(dpy,cfgs[i],EGL_NATIVE_VISUAL_ID,&v); if(v==(EGLint)GBM_FORMAT_XRGB8888){cfg=cfgs[i];break;}}
    // ...but request a version-3 CONTEXT so ES3 occlusion queries exist.
    EGLint xa[]={EGL_CONTEXT_CLIENT_VERSION,3,EGL_NONE};
    EGLContext ctx=eglCreateContext(dpy,cfg,EGL_NO_CONTEXT,xa);
    if(ctx==EGL_NO_CONTEXT){fprintf(stderr,"no ES3 ctx eglErr=0x%x\n",eglGetError());return 1;}
    EGLSurface surf=eglCreateWindowSurface(dpy,cfg,(EGLNativeWindowType)gs,0);
    eglMakeCurrent(dpy,surf,surf,ctx);
    printf("GL_VERSION: %s\n", glGetString(GL_VERSION));

    const char *vs="#version 300 es\nin vec2 p; void main(){ gl_Position=vec4(p,0.0,1.0); }";
    const char *fs="#version 300 es\nprecision mediump float; out vec4 c; void main(){ c=vec4(0.0,1.0,0.0,1.0); }";
    GLuint v=glCreateShader(GL_VERTEX_SHADER); glShaderSource(v,1,&vs,0); glCompileShader(v); csh(v,"vs");
    GLuint f=glCreateShader(GL_FRAGMENT_SHADER); glShaderSource(f,1,&fs,0); glCompileShader(f); csh(f,"fs");
    GLuint prog=glCreateProgram(); glAttachShader(prog,v); glAttachShader(prog,f);
    glBindAttribLocation(prog,0,"p"); glLinkProgram(prog);
    GLint ln=0; glGetProgramiv(prog,GL_LINK_STATUS,&ln); printf("link=%d\n",ln); glUseProgram(prog);

    GLfloat quad[]={-1.f,-1.f, 1.f,-1.f, -1.f,1.f,  -1.f,1.f, 1.f,-1.f, 1.f,1.f};
    GLuint vao=0; glGenVertexArrays(1,&vao); glBindVertexArray(vao);
    glViewport(0,0,mode.hdisplay,mode.vdisplay);
    glDisable(GL_DEPTH_TEST); glDisable(GL_CULL_FACE); glDisable(GL_SCISSOR_TEST);
    glClearColor(0,0,1,1); glClear(GL_COLOR_BUFFER_BIT);

    GLuint q=0; glGenQueries(1,&q);
    glBeginQuery(GL_ANY_SAMPLES_PASSED,q);
    glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,quad);
    glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES,0,6);
    glEndQuery(GL_ANY_SAMPLES_PASSED);
    glFinish();
    GLuint avail=0, passed=0;
    glGetQueryObjectuiv(q,GL_QUERY_RESULT_AVAILABLE,&avail);
    glGetQueryObjectuiv(q,GL_QUERY_RESULT,&passed);
    printf("OCCLUSION: avail=%u any_samples_passed=%u  glGetError=0x%x\n", avail, passed, glGetError());

    // Also scan it out so we can cross-check against the IOSurface oracle as before.
    eglSwapBuffers(dpy,surf);
    struct gbm_bo *bo=gbm_surface_lock_front_buffer(gs);
    if(bo){ uint32_t h=gbm_bo_get_handle(bo).u32,st=gbm_bo_get_stride(bo),fb=0;
        drmModeAddFB(fd,mode.hdisplay,mode.vdisplay,24,32,st,h,&fb);
        drmModeEncoder *enc=drmModeGetEncoder(fd,conn->encoder_id?conn->encoder_id:conn->encoders[0]);
        uint32_t crtc=(enc&&enc->crtc_id)?enc->crtc_id:res->crtcs[0], cid=conn->connector_id;
        printf("setcrtc=%d\n", drmModeSetCrtc(fd,crtc,fb,0,0,&cid,1,&mode)); }
    printf("done\n");
    return 0;
}
