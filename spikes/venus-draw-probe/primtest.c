// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Controlled vehicle stage 3: PRIMITIVE TYPE — does a quad drawn as TRIANGLE_FAN / TRIANGLE_STRIP
// lose one triangle through zink->venus->MoltenVK?
//
// The seated desktop renders dock icons as the BOTTOM-LEFT TRIANGLE of each quad, top-right missing,
// texture correct in the half that draws — i.e. exactly one of the two triangles isn't rasterized.
// quads.c/fbotex.c (indexed TRIANGLE LIST) render both triangles fine, AND the [LIMINA-IDX] probe only
// sees list draws because MVKCmdDrawIndexed::encode reroutes TRIANGLE_FAN to encodeIndexedIndirect and
// returns BEFORE that probe. So fan/strip is the untested path. This draws an NxN grid where each quad
// is one glDrawArrays in the chosen mode; a half-rendered cell = reproduced.
//
// Env: PRIM=tris|fan|strip (default fan). PRIMTEST_N=<n> grid (default 8).
// Build (guest): gcc primtest.c -o primtest -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm -lm
// Run (zink env, gdm stopped): PRIM=fan ./primtest ; host iosdump <scanout id>.
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
    const char* pm = getenv("PRIM"); if(!pm) pm="fan";
    int N = getenv("PRIMTEST_N")?atoi(getenv("PRIMTEST_N")):8; if(N<1)N=1;
    GLenum mode = !strcmp(pm,"strip")?GL_TRIANGLE_STRIP : !strcmp(pm,"tris")?GL_TRIANGLES : GL_TRIANGLE_FAN;
    printf("primtest N=%d mode=%s\n", N, pm);

    int fd=open("/dev/dri/card0",O_RDWR|O_CLOEXEC); if(fd<0){perror("card0");return 1;}
    drmModeRes* res=drmModeGetResources(fd); drmModeConnector* conn=NULL;
    for(int i=0;i<res->count_connectors;i++){drmModeConnector* c=drmModeGetConnector(fd,res->connectors[i]);
        if(c&&c->connection==DRM_MODE_CONNECTED&&c->count_modes>0){conn=c;break;} if(c)drmModeFreeConnector(c);}
    if(!conn){fprintf(stderr,"no connector\n");return 1;}
    drmModeModeInfo mode_i=conn->modes[0];
    for(int i=0;i<conn->count_modes;i++) if(conn->modes[i].hdisplay==1280&&conn->modes[i].vdisplay==800){mode_i=conn->modes[i];break;}
    drmModeEncoder* enc=drmModeGetEncoder(fd,conn->encoder_id?conn->encoder_id:conn->encoders[0]);
    uint32_t crtc_id=(enc&&enc->crtc_id)?enc->crtc_id:res->crtcs[0]; uint32_t conn_id=conn->connector_id;
    int W=mode_i.hdisplay, H=mode_i.vdisplay;

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

    glViewport(0,0,W,H); glDisable(GL_DEPTH_TEST);
    // PRIM_CULL=1 enables back-face culling (cogl draws with culling ON). PRIM_FRONT=cw|ccw sets
    // front-face (default ccw). The winding/cull hypothesis: a strip's per-triangle winding flip,
    // mishandled under culling in zink->venus->MoltenVK, culls the odd triangle of each quad.
    if (getenv("PRIM_CULL")) {
        glEnable(GL_CULL_FACE); glCullFace(GL_BACK);
        const char* ff = getenv("PRIM_FRONT");
        glFrontFace(ff && !strcmp(ff,"cw") ? GL_CW : GL_CCW);
        printf("cull=ON front=%s\n", ff?ff:"ccw");
    } else { glDisable(GL_CULL_FACE); printf("cull=off\n"); }
    glClearColor(0.,0.,1.,1.); glClear(GL_COLOR_BUFFER_BIT);
    glEnableVertexAttribArray(0);

    // Like cogl: ONE shared VBO holds all quads; each rect is drawn with glDrawArrays at a NON-ZERO
    // first-vertex offset (q*vpq). cogl's single-rect path uses TRIANGLE_FAN at first=current_vertex.
    int Q=N*N; int vpq = (mode==GL_TRIANGLES)?6:4;
    float* all=malloc(Q*vpq*2*sizeof(float)); int idx=0;
    float cell=2.f/N, gap=cell*0.18f, s=cell-gap;
    for(int gy=0;gy<N;gy++)for(int gx=0;gx<N;gx++){
        float x0=-1.f+gx*cell+gap*.5f, y0=-1.f+gy*cell+gap*.5f, x1=x0+s, y1=y0+s;
        float TL[2]={x0,y1},TR[2]={x1,y1},BL[2]={x0,y0},BR[2]={x1,y0};
        if(mode==GL_TRIANGLE_FAN){      // TL,TR,BR,BL
            float q[8]={TL[0],TL[1],TR[0],TR[1],BR[0],BR[1],BL[0],BL[1]}; memcpy(all+idx,q,sizeof q); idx+=8;
        } else if(mode==GL_TRIANGLE_STRIP){ // TL,TR,BL,BR
            float q[8]={TL[0],TL[1],TR[0],TR[1],BL[0],BL[1],BR[0],BR[1]}; memcpy(all+idx,q,sizeof q); idx+=8;
        } else {                        // 6-vert list
            float q[12]={TL[0],TL[1],TR[0],TR[1],BL[0],BL[1],BL[0],BL[1],TR[0],TR[1],BR[0],BR[1]}; memcpy(all+idx,q,sizeof q); idx+=12;
        }
    }
    GLuint vbo; glGenBuffers(1,&vbo); glBindBuffer(GL_ARRAY_BUFFER,vbo);
    glBufferData(GL_ARRAY_BUFFER, Q*vpq*2*sizeof(float), all, GL_STATIC_DRAW);
    glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,0);
    for(int q=0;q<Q;q++) glDrawArrays(mode, q*vpq, vpq);   // <-- non-zero first vertex per rect
    glFinish();
    printf("glGetError=0x%x\n", glGetError());
    EGLBoolean sw=eglSwapBuffers(dpy,surf); printf("swap=%d\n",sw);
    struct gbm_bo* bo=gbm_surface_lock_front_buffer(gs); if(!bo){fprintf(stderr,"lock NULL\n");return 1;}
    uint32_t handle=gbm_bo_get_handle(bo).u32, stride=gbm_bo_get_stride(bo), fb=0;
    drmModeAddFB(fd,W,H,24,32,stride,handle,&fb);
    int r=drmModeSetCrtc(fd,crtc_id,fb,0,0,&conn_id,1,&mode_i);
    printf("setcrtc=%d holding 600s...\n",r);
    sleep(600);
    return 0;
}
