// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Companion to tri.c: ATTRIBUTELESS fullscreen triangle (GLES3, positions from gl_VertexID,
// NO vertex buffer / NO vertex attribute). Bisects bug A's draw failure:
//   green => draw produces fragments when there is no vertex input => the fault is vertex-input
//            (attribute/buffer fetch) in zink->venus->MoltenVK
//   blue  => still nothing => fault is downstream of vertex input (raster / fragment-output / pipeline)
// Build: gcc noattr.c -o noattr -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm
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
    if (fd < 0) { perror("open"); return 1; }
    drmModeRes *res = drmModeGetResources(fd);
    drmModeConnector *conn = NULL;
    for (int i=0;i<res->count_connectors;i++){ drmModeConnector *c=drmModeGetConnector(fd,res->connectors[i]);
        if(c && c->connection==DRM_MODE_CONNECTED && c->count_modes>0){conn=c;break;} if(c)drmModeFreeConnector(c);}
    if(!conn){fprintf(stderr,"no connector\n");return 1;}
    drmModeModeInfo mode = conn->modes[0];
    for(int i=0;i<conn->count_modes;i++) if(conn->modes[i].hdisplay==1280&&conn->modes[i].vdisplay==800){mode=conn->modes[i];break;}
    drmModeEncoder *enc=drmModeGetEncoder(fd,conn->encoder_id?conn->encoder_id:conn->encoders[0]);
    uint32_t crtc=(enc&&enc->crtc_id)?enc->crtc_id:res->crtcs[0], cid=conn->connector_id;

    struct gbm_device *gbm=gbm_create_device(fd);
    struct gbm_surface *gs=gbm_surface_create(gbm,mode.hdisplay,mode.vdisplay,GBM_FORMAT_XRGB8888,
        GBM_BO_USE_SCANOUT|GBM_BO_USE_RENDERING);
    EGLDisplay dpy=eglGetDisplay((EGLNativeDisplayType)gbm);
    eglInitialize(dpy,0,0); eglBindAPI(EGL_OPENGL_ES_API);
    EGLint ca[]={EGL_SURFACE_TYPE,EGL_WINDOW_BIT,EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,
                 EGL_RENDERABLE_TYPE,0x00000040 /*EGL_OPENGL_ES3_BIT_KHR*/,EGL_NONE};
    EGLConfig cfgs[64]; EGLint n=0; eglChooseConfig(dpy,ca,cfgs,64,&n);
    EGLConfig cfg=cfgs[0];
    for(int i=0;i<n;i++){EGLint v=0; eglGetConfigAttrib(dpy,cfgs[i],EGL_NATIVE_VISUAL_ID,&v); if(v==(EGLint)GBM_FORMAT_XRGB8888){cfg=cfgs[i];break;}}
    EGLint xa[]={EGL_CONTEXT_CLIENT_VERSION,3,EGL_NONE};
    EGLContext ctx=eglCreateContext(dpy,cfg,EGL_NO_CONTEXT,xa);
    if(ctx==EGL_NO_CONTEXT){fprintf(stderr,"no ES3 context eglErr=0x%x\n",eglGetError());return 1;}
    EGLSurface surf=eglCreateWindowSurface(dpy,cfg,(EGLNativeWindowType)gs,0);
    eglMakeCurrent(dpy,surf,surf,ctx);
    printf("GL_VERSION: %s\n", glGetString(GL_VERSION));

    const char *vs="#version 300 es\nvoid main(){ vec2 p=vec2((gl_VertexID<<1)&2, gl_VertexID&2); gl_Position=vec4(p*2.0-1.0,0.0,1.0); }";
    const char *fs="#version 300 es\nprecision mediump float; out vec4 c; void main(){ c=vec4(0.0,1.0,0.0,1.0); }";
    GLuint v=glCreateShader(GL_VERTEX_SHADER); glShaderSource(v,1,&vs,0); glCompileShader(v); csh(v,"vs");
    GLuint f=glCreateShader(GL_FRAGMENT_SHADER); glShaderSource(f,1,&fs,0); glCompileShader(f); csh(f,"fs");
    GLuint p=glCreateProgram(); glAttachShader(p,v); glAttachShader(p,f); glLinkProgram(p);
    GLint ln=0; glGetProgramiv(p,GL_LINK_STATUS,&ln); printf("link=%d\n",ln); glUseProgram(p);

    // GLES3 core requires a bound VAO for attributeless draws.
    GLuint vao=0; glGenVertexArrays(1,&vao); glBindVertexArray(vao);
    glViewport(0,0,mode.hdisplay,mode.vdisplay);
    glDisable(GL_DEPTH_TEST); glDisable(GL_CULL_FACE);
    glClearColor(0.0f,0.0f,1.0f,1.0f); glClear(GL_COLOR_BUFFER_BIT); // BLUE
    glDrawArrays(GL_TRIANGLES,0,3); // attributeless GREEN fullscreen triangle
    glFinish();
    printf("glGetError=0x%x\n", glGetError());

    eglSwapBuffers(dpy,surf);
    struct gbm_bo *bo=gbm_surface_lock_front_buffer(gs);
    if(!bo){fprintf(stderr,"no front buffer\n");return 1;}
    uint32_t h=gbm_bo_get_handle(bo).u32,st=gbm_bo_get_stride(bo),fb=0;
    printf("addfb=%d\n", drmModeAddFB(fd,mode.hdisplay,mode.vdisplay,24,32,st,h,&fb));
    printf("setcrtc=%d\n", drmModeSetCrtc(fd,crtc,fb,0,0,&cid,1,&mode));
    printf("holding 120s...\n");
    sleep(120);
    return 0;
}
