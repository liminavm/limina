// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Controlled vehicle stage 4: UNSIGNED_BYTE INDICES — cogl's exact rectangle path.
//
// Reading cogl 49.5 (cogl-indices.c cogl_context_get_rectangle_indices, cogl-journal.c): batched
// rectangles are drawn as indexed GL_TRIANGLES with the per-quad pattern {0,1,2, 0,2,3}, and for
// small batches (<=64 rects) the index buffer is UNSIGNED_BYTE (uint8). Metal/core-Vulkan have NO
// uint8 index type, so zink->venus->MoltenVK must convert uint8->uint16 (the [LIMINA-IDX] probe showed
// idxType=uint16 reaching Metal). Every previous vehicle used uint16 or non-indexed indices and was
// clean. This is the untested path: a bug in that uint8->uint16 conversion would corrupt one triangle
// per quad — exactly the dock-icon symptom (bottom-left triangle renders, top-right dropped).
//
// Env: U8=1 (default) uint8 indices; U8=0 uint16 control. Both use cogl's {0,1,2,0,2,3} pattern.
//      U8TEST_N=<n> grid (default 8 = 64 quads = 256 verts, max uint8 index 255 — the cogl limit).
// Build (guest): gcc u8test.c -o u8test -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm -lm
// Run (zink env, gdm stopped): U8=1 ./u8test ; host iosdump <scanout id>.  Half-rendered quads = repro.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdint.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <gbm.h>
#include <EGL/egl.h>
#include <GLES2/gl2.h>

int main(void){
    setvbuf(stdout,NULL,_IONBF,0);
    int N = getenv("U8TEST_N")?atoi(getenv("U8TEST_N")):8; if(N<1)N=1;
    int u8 = getenv("U8")? atoi(getenv("U8")) : 1;
    printf("u8test N=%d quads=%d idx=%s pattern={0,1,2,0,2,3}\n", N, N*N, u8?"UNSIGNED_BYTE":"UNSIGNED_SHORT");

    int fd=open("/dev/dri/card0",O_RDWR|O_CLOEXEC); if(fd<0){perror("card0");return 1;}
    drmModeRes* res=drmModeGetResources(fd); drmModeConnector* conn=NULL;
    for(int i=0;i<res->count_connectors;i++){drmModeConnector* c=drmModeGetConnector(fd,res->connectors[i]);
        if(c&&c->connection==DRM_MODE_CONNECTED&&c->count_modes>0){conn=c;break;} if(c)drmModeFreeConnector(c);}
    if(!conn){fprintf(stderr,"no connector\n");return 1;}
    drmModeModeInfo mode=conn->modes[0];
    for(int i=0;i<conn->count_modes;i++) if(conn->modes[i].hdisplay==1280&&conn->modes[i].vdisplay==800){mode=conn->modes[i];break;}
    drmModeEncoder* enc=drmModeGetEncoder(fd,conn->encoder_id?conn->encoder_id:conn->encoders[0]);
    uint32_t crtc_id=(enc&&enc->crtc_id)?enc->crtc_id:res->crtcs[0]; uint32_t conn_id=conn->connector_id;
    int W=mode.hdisplay, H=mode.vdisplay;

    struct gbm_device* gbm=gbm_create_device(fd);
    struct gbm_surface* gs=gbm_surface_create(gbm,W,H,GBM_FORMAT_XRGB8888,GBM_BO_USE_SCANOUT|GBM_BO_USE_RENDERING);
    EGLDisplay dpy=eglGetDisplay((EGLNativeDisplayType)gbm); eglInitialize(dpy,0,0); eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cfga[]={EGL_SURFACE_TYPE,EGL_WINDOW_BIT,EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,EGL_RENDERABLE_TYPE,EGL_OPENGL_ES2_BIT,EGL_NONE};
    EGLConfig cfgs[64]; EGLint n=0; eglChooseConfig(dpy,cfga,cfgs,64,&n); EGLConfig cfg=cfgs[0];
    for(int i=0;i<n;i++){EGLint vid=0;eglGetConfigAttrib(dpy,cfgs[i],EGL_NATIVE_VISUAL_ID,&vid); if(vid==(EGLint)GBM_FORMAT_XRGB8888){cfg=cfgs[i];break;}}
    EGLint ctxa[]={EGL_CONTEXT_CLIENT_VERSION,2,EGL_NONE};
    EGLContext ctx=eglCreateContext(dpy,cfg,EGL_NO_CONTEXT,ctxa);
    EGLSurface surf=eglCreateWindowSurface(dpy,cfg,(EGLNativeWindowType)gs,NULL);
    eglMakeCurrent(dpy,surf,surf,ctx);
    printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));

    const char* vs="attribute vec2 p; void main(){gl_Position=vec4(p,0.,1.);}";
    const char* fs="precision mediump float; void main(){gl_FragColor=vec4(0.,1.,0.,1.);}";
    GLuint v=glCreateShader(GL_VERTEX_SHADER); glShaderSource(v,1,&vs,0); glCompileShader(v);
    GLuint f=glCreateShader(GL_FRAGMENT_SHADER); glShaderSource(f,1,&fs,0); glCompileShader(f);
    GLuint pr=glCreateProgram(); glAttachShader(pr,v); glAttachShader(pr,f); glBindAttribLocation(pr,0,"p"); glLinkProgram(pr);
    GLint ln=0; glGetProgramiv(pr,GL_LINK_STATUS,&ln); printf("link=%d\n",ln); glUseProgram(pr);

    int Q=N*N;
    float* varr=malloc(Q*4*2*sizeof(float));
    uint8_t* i8=malloc(Q*6); uint16_t* i16=malloc(Q*6*sizeof(uint16_t));
    float cell=2.f/N, gap=cell*0.18f, s=cell-gap;
    int vi=0, ii=0;
    for(int gy=0;gy<N;gy++)for(int gx=0;gx<N;gx++){
        float x0=-1.f+gx*cell+gap*.5f, y0=-1.f+gy*cell+gap*.5f, x1=x0+s, y1=y0+s;
        // pattern {0,1,2,0,2,3} shares diagonal v0-v2, so verts must go AROUND the quad: TL,TR,BR,BL.
        float c4[4][2]={{x0,y1},{x1,y1},{x1,y0},{x0,y0}};
        int base=(gy*N+gx)*4;
        for(int k=0;k<4;k++){ varr[vi++]=c4[k][0]; varr[vi++]=c4[k][1]; }
        int pat[6]={0,1,2,0,2,3};
        for(int k=0;k<6;k++){ i8[ii]=(uint8_t)(base+pat[k]); i16[ii]=(uint16_t)(base+pat[k]); ii++; }
    }

    GLuint vbo,ibo;
    glGenBuffers(1,&vbo); glBindBuffer(GL_ARRAY_BUFFER,vbo); glBufferData(GL_ARRAY_BUFFER,Q*4*2*sizeof(float),varr,GL_STATIC_DRAW);
    glGenBuffers(1,&ibo); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,ibo);
    if(u8) glBufferData(GL_ELEMENT_ARRAY_BUFFER,Q*6,i8,GL_STATIC_DRAW);
    else   glBufferData(GL_ELEMENT_ARRAY_BUFFER,Q*6*sizeof(uint16_t),i16,GL_STATIC_DRAW);
    glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,0); glEnableVertexAttribArray(0);

    glViewport(0,0,W,H); glDisable(GL_DEPTH_TEST); glDisable(GL_CULL_FACE);
    glClearColor(0.,0.,1.,1.); glClear(GL_COLOR_BUFFER_BIT);
    glDrawElements(GL_TRIANGLES, Q*6, u8?GL_UNSIGNED_BYTE:GL_UNSIGNED_SHORT, 0);
    glFinish();
    printf("glGetError=0x%x\n", glGetError());

    EGLBoolean sw=eglSwapBuffers(dpy,surf); printf("swap=%d\n",sw);
    struct gbm_bo* bo=gbm_surface_lock_front_buffer(gs); if(!bo){fprintf(stderr,"lock NULL\n");return 1;}
    uint32_t handle=gbm_bo_get_handle(bo).u32, stride=gbm_bo_get_stride(bo), fb=0;
    drmModeAddFB(fd,W,H,24,32,stride,handle,&fb);
    int r=drmModeSetCrtc(fd,crtc_id,fb,0,0,&conn_id,1,&mode);
    printf("setcrtc=%d holding 600s...\n",r);
    sleep(600);
    return 0;
}
