// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Controlled vehicle stage 2: render-to-FBO + TEXTURED composite — the two things the gnome-shell
// compositor does that quads.c (pure geometry) does not. quads.c proved geometry/coherency/streaming
// are clean static AND streamed; the desktop still breaks (one vertex per small quad — "top-right" —
// stretched, dynamically). So the trigger is in the texture / offscreen-FBO path.
//
//   Pass 1: render a green grid into an OFFSCREEN FBO (a texture).
//   Pass 2: to the scanout, draw an NxN grid of small TEXTURED quads (interleaved pos+uv, stride 16 —
//           the multi-attribute layout the desktop's [LIMINA-VTX] showed) each sampling the FBO texture.
//   Expected: the composited scanout reproduces the green grid. Any stretched / missing / corner-fanned
//   quad = reproduced; we've localized the defect to FBO/texture, and can subdivide.
//
// Env: FBOTEX_N=<n> grid (default 12). FBOTEX_STREAM=1 re-upload the composite verts each frame, 240
//      frames (texture + streaming combined).
// Build (guest): gcc fbotex.c -o fbotex -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm -lm
// Run (patched zink env, multi-user/gdm-stopped): ./fbotex ; then host iosdump <scanout id>.
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
#include <GLES2/gl2ext.h>

static GLuint mkprog(const char* vs, const char* fs) {
    GLuint v=glCreateShader(GL_VERTEX_SHADER); glShaderSource(v,1,&vs,0); glCompileShader(v);
    GLuint f=glCreateShader(GL_FRAGMENT_SHADER); glShaderSource(f,1,&fs,0); glCompileShader(f);
    GLint ok=0; glGetShaderiv(v,GL_COMPILE_STATUS,&ok); if(!ok){char l[512];glGetShaderInfoLog(v,512,0,l);printf("vs err %s\n",l);}
    glGetShaderiv(f,GL_COMPILE_STATUS,&ok); if(!ok){char l[512];glGetShaderInfoLog(f,512,0,l);printf("fs err %s\n",l);}
    GLuint p=glCreateProgram(); glAttachShader(p,v); glAttachShader(p,f);
    glBindAttribLocation(p,0,"p"); glBindAttribLocation(p,1,"t"); glLinkProgram(p);
    GLint ln=0; glGetProgramiv(p,GL_LINK_STATUS,&ln); if(!ln){char l[512];glGetProgramInfoLog(p,512,0,l);printf("link err %s\n",l);}
    return p;
}

int main(void) {
    setvbuf(stdout,NULL,_IONBF,0);
    int N = getenv("FBOTEX_N") ? atoi(getenv("FBOTEX_N")) : 12;
    int stream = getenv("FBOTEX_STREAM") ? 1 : 0;
    if (N<1) N=1;
    printf("fbotex N=%d quads=%d%s\n", N, N*N, stream?" +stream":"");

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

    GLuint progSolid=mkprog("attribute vec2 p; void main(){gl_Position=vec4(p,0.,1.);}",
                            "precision mediump float; void main(){gl_FragColor=vec4(0.,1.,0.,1.);}");
    GLuint progTex=mkprog("attribute vec2 p; attribute vec2 t; varying vec2 uv; void main(){uv=t; gl_Position=vec4(p,0.,1.);}",
                          "precision mediump float; varying vec2 uv; uniform sampler2D s; void main(){gl_FragColor=texture2D(s,uv);}");

    // Offscreen FBO + color texture.
    GLuint tex=0; glGenTextures(1,&tex); glBindTexture(GL_TEXTURE_2D,tex);
    glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA,W,H,0,GL_RGBA,GL_UNSIGNED_BYTE,NULL);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_WRAP_S,GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_WRAP_T,GL_CLAMP_TO_EDGE);
    GLuint fbo=0; glGenFramebuffers(1,&fbo); glBindFramebuffer(GL_FRAMEBUFFER,fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,tex,0);
    printf("FBO status=0x%x (complete=0x%x)\n", glCheckFramebufferStatus(GL_FRAMEBUFFER), GL_FRAMEBUFFER_COMPLETE);

    // Offscreen grid geometry (solid, pos only).
    int Q=N*N;
    float* og=malloc(Q*4*2*sizeof(float)); uint16_t* oi=malloc(Q*6*sizeof(uint16_t));
    // Composite grid geometry (textured: interleaved pos.xy, uv.xy → stride 16).
    float* cg=malloc(Q*4*4*sizeof(float)); uint16_t* ci=malloc(Q*6*sizeof(uint16_t));
    float cell=2.f/N, gap=cell*0.18f, s=cell-gap;
    int oviN=0,oiN=0,cviN=0,ciN=0;
    for(int gy=0;gy<N;gy++)for(int gx=0;gx<N;gx++){
        float x0=-1.f+gx*cell+gap*.5f, y0=-1.f+gy*cell+gap*.5f, x1=x0+s, y1=y0+s;
        // uv maps each quad to its matching region of the FBO so the composite reproduces the grid 1:1
        float u0=(x0+1.f)*.5f, v0=(y0+1.f)*.5f, u1=(x1+1.f)*.5f, v1=(y1+1.f)*.5f;
        float pc[4][2]={{x0,y1},{x1,y1},{x0,y0},{x1,y0}};
        float uc[4][2]={{u0,v1},{u1,v1},{u0,v0},{u1,v0}};
        int base=(gy*N+gx)*4; int o[6]={0,1,2,2,1,3};
        for(int k=0;k<4;k++){ og[oviN++]=pc[k][0]; og[oviN++]=pc[k][1]; }
        for(int k=0;k<4;k++){ cg[cviN++]=pc[k][0]; cg[cviN++]=pc[k][1]; cg[cviN++]=uc[k][0]; cg[cviN++]=uc[k][1]; }
        for(int k=0;k<6;k++){ oi[oiN++]=base+o[k]; ci[ciN++]=base+o[k]; }
    }
    GLuint ovbo,oibo,cvbo,cibo;
    glGenBuffers(1,&ovbo); glBindBuffer(GL_ARRAY_BUFFER,ovbo); glBufferData(GL_ARRAY_BUFFER,Q*4*2*sizeof(float),og,GL_STATIC_DRAW);
    glGenBuffers(1,&oibo); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,oibo); glBufferData(GL_ELEMENT_ARRAY_BUFFER,Q*6*sizeof(uint16_t),oi,GL_STATIC_DRAW);
    glGenBuffers(1,&cvbo); glBindBuffer(GL_ARRAY_BUFFER,cvbo); glBufferData(GL_ARRAY_BUFFER,Q*4*4*sizeof(float),cg,stream?GL_DYNAMIC_DRAW:GL_STATIC_DRAW);
    glGenBuffers(1,&cibo); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,cibo); glBufferData(GL_ELEMENT_ARRAY_BUFFER,Q*6*sizeof(uint16_t),ci,GL_STATIC_DRAW);

    int frames = stream ? 240 : 1;
    for(int fr=0; fr<frames; fr++){
        // Pass 1: green grid → FBO.
        glBindFramebuffer(GL_FRAMEBUFFER,fbo); glViewport(0,0,W,H);
        glClearColor(0.,0.,1.,1.); glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(progSolid);
        glBindBuffer(GL_ARRAY_BUFFER,ovbo); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,oibo);
        glEnableVertexAttribArray(0); glDisableVertexAttribArray(1);
        glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,0);
        glDrawElements(GL_TRIANGLES,Q*6,GL_UNSIGNED_SHORT,0);
        // Pass 2: textured composite → scanout.
        glBindFramebuffer(GL_FRAMEBUFFER,0); glViewport(0,0,W,H);
        glClearColor(0.,0.,1.,1.); glClear(GL_COLOR_BUFFER_BIT);
        glUseProgram(progTex);
        glActiveTexture(GL_TEXTURE0); glBindTexture(GL_TEXTURE_2D,tex);
        glUniform1i(glGetUniformLocation(progTex,"s"),0);
        glBindBuffer(GL_ARRAY_BUFFER,cvbo);
        if(stream){ glBufferData(GL_ARRAY_BUFFER,Q*4*4*sizeof(float),NULL,GL_DYNAMIC_DRAW);
                    glBufferSubData(GL_ARRAY_BUFFER,0,Q*4*4*sizeof(float),cg); }
        glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,cibo);
        glEnableVertexAttribArray(0); glEnableVertexAttribArray(1);
        glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,16,(void*)0);
        glVertexAttribPointer(1,2,GL_FLOAT,GL_FALSE,16,(void*)8);
        glDrawElements(GL_TRIANGLES,Q*6,GL_UNSIGNED_SHORT,0);
    }
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
