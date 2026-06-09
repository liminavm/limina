// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Controlled vehicle: MAP-PER-FRAME indexed multi-quad draw — the one combination none of the other
// vehicles hit, and the last standing delta from the broken seated-desktop path (theory C).
//
// Bug recap: dock/panel/icons render as ONLY the bottom-left triangle of each quad (top-right missing,
// clean TL-BR diagonal). `COGL_DEBUG=disable-batching` fixes the desktop. Three data points narrow it:
//   real batched     = MAPPED dynamic buffer + MANY quads + indexed TRIANGLES -> BROKEN
//   disable-batching = mapped + ONE quad + TRIANGLE_FAN                       -> clean
//   texfan MODE=batch= STATIC (glBufferData) + many + indexed                 -> clean
// So the suspect is a LARGE per-frame *mapped* multi-quad buffer fetched incoherently by the host GPU.
// cogl's upload_vertices (cogl-journal.c:1162) maps a DYNAMIC CoglAttributeBuffer EVERY frame
// (_cogl_buffer_map_range_for_fill_or_fallback = glMapBufferRange WRITE|INVALIDATE), writes the 2->4
// expanded verts, unmaps, then draws indexed. My static vehicles never map. This reproduces THAT:
//   MAP=1 (default): glMapBufferRange(WRITE|INVALIDATE_BUFFER) the verts EVERY frame, unmap, draw.
//   MAP=0 (control): glBufferData(DYNAMIC) the verts every frame (a copy upload, not a host map).
// Loops FRAMES (default 60) so the GPU keeps reading the buffer while the guest re-writes it, like a
// live desktop. If MAP=1 shows the bottom-left-triangle bug and MAP=0 is clean -> theory C confirmed:
// the host GPU fetches the per-frame mapped multi-quad buffer incoherently (a #28 residual).
//
// Env: N=<grid> (default 8 -> 64 quads). MAP=0|1 (default 1). FRAMES=<n> (default 60). IDX8=0|1 (uint8).
// Build (guest): gcc mapquad.c -o mapquad -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm -lm
// Run (zink env, gdm stopped): MAP=1 ./mapquad ; host: iosdump <scanout id>.
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
#include <GLES3/gl3.h>   // glMapBufferRange / GL_MAP_* / glUnmapBuffer (ES3 entrypoints)

int main(void){
    setvbuf(stdout,NULL,_IONBF,0);
    int N = getenv("N")?atoi(getenv("N")):8; if(N<1)N=1;
    int use_map = getenv("MAP")?atoi(getenv("MAP")):1;
    int frames = getenv("FRAMES")?atoi(getenv("FRAMES")):60;
    int idx8 = getenv("IDX8")?atoi(getenv("IDX8")):0;
    printf("mapquad N=%d MAP=%d FRAMES=%d IDX8=%d\n", N, use_map, frames, idx8);

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
    // Request an ES3 context so glMapBufferRange is available (falls back to ES2 if unavailable).
    EGLint ctxa3[]={EGL_CONTEXT_CLIENT_VERSION,3,EGL_NONE};
    EGLContext ctx=eglCreateContext(dpy,cfg,EGL_NO_CONTEXT,ctxa3);
    if(ctx==EGL_NO_CONTEXT){ EGLint ctxa2[]={EGL_CONTEXT_CLIENT_VERSION,2,EGL_NONE}; ctx=eglCreateContext(dpy,cfg,EGL_NO_CONTEXT,ctxa2); printf("WARN: ES3 ctx failed, ES2 (no glMapBufferRange)\n"); }
    EGLSurface surf=eglCreateWindowSurface(dpy,cfg,(EGLNativeWindowType)gs,NULL);
    eglMakeCurrent(dpy,surf,surf,ctx);
    printf("GL_RENDERER: %s  GL_VERSION: %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION));

    const char* vs="attribute vec2 p; attribute vec2 t; varying vec2 uv; void main(){uv=t; gl_Position=vec4(p,0.,1.);}";
    const char* fs="precision mediump float; varying vec2 uv; uniform sampler2D s; void main(){gl_FragColor=texture2D(s,uv);}";
    GLuint v=glCreateShader(GL_VERTEX_SHADER); glShaderSource(v,1,&vs,0); glCompileShader(v);
    GLuint f=glCreateShader(GL_FRAGMENT_SHADER); glShaderSource(f,1,&fs,0); glCompileShader(f);
    GLuint pr=glCreateProgram(); glAttachShader(pr,v); glAttachShader(pr,f);
    glBindAttribLocation(pr,0,"p"); glBindAttribLocation(pr,1,"t"); glLinkProgram(pr);
    GLint ln=0; glGetProgramiv(pr,GL_LINK_STATUS,&ln); printf("link=%d\n",ln); glUseProgram(pr);

    unsigned char tex[8*8*4];
    for(int y=0;y<8;y++)for(int x=0;x<8;x++){int o=(y*8+x)*4; tex[o]=x*36; tex[o+1]=(7-x)*36; tex[o+2]=(y<2)?255:0; tex[o+3]=255;}
    GLuint t; glGenTextures(1,&t); glBindTexture(GL_TEXTURE_2D,t);
    glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA,8,8,0,GL_RGBA,GL_UNSIGNED_BYTE,tex);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_NEAREST);
    glUniform1i(glGetUniformLocation(pr,"s"),0);

    glViewport(0,0,W,H); glDisable(GL_DEPTH_TEST); glDisable(GL_CULL_FACE);
    glEnableVertexAttribArray(0); glEnableVertexAttribArray(1);

    int Q=N*N;
    // Build the expanded vertex data ONCE in CPU memory (4 verts/quad, interleaved pos.xy+uv.xy),
    // in cogl's corner order TL,BL,BR,TR. We re-upload it every frame (mapped or copy).
    int floats_per_vert=4, verts=Q*4;
    float* cpu=malloc(verts*floats_per_vert*sizeof(float)); int vi=0;
    float cell=2.f/N, gap=cell*0.18f, s=cell-gap;
    for(int gy=0;gy<N;gy++)for(int gx=0;gx<N;gx++){
        float x0=-1.f+gx*cell+gap*.5f, y0=-1.f+gy*cell+gap*.5f, x1=x0+s, y1=y0+s;
        float TL[4]={x0,y1,0,0},BL[4]={x0,y0,0,1},BR[4]={x1,y0,1,1},TR[4]={x1,y1,1,0};
        memcpy(cpu+vi,TL,16);memcpy(cpu+vi+4,BL,16);memcpy(cpu+vi+8,BR,16);memcpy(cpu+vi+12,TR,16); vi+=16;
    }
    size_t vbytes=(size_t)verts*floats_per_vert*sizeof(float);
    // index buffer {0,1,2, 0,2,3} per quad
    unsigned char*  i8 =malloc(Q*6);
    unsigned short* i16=malloc(Q*6*sizeof(short));
    for(int q=0;q<Q;q++){int b=q*4,o=q*6; int seq[6]={b+0,b+1,b+2,b+0,b+2,b+3};
        for(int k=0;k<6;k++){i16[o+k]=seq[k]; i8[o+k]=seq[k];}}
    GLenum itype=idx8?GL_UNSIGNED_BYTE:GL_UNSIGNED_SHORT; int isz=idx8?1:2;

    GLuint vbo; glGenBuffers(1,&vbo); glBindBuffer(GL_ARRAY_BUFFER,vbo);
    glBufferData(GL_ARRAY_BUFFER, vbytes, NULL, GL_DYNAMIC_DRAW);   // dynamic, like cogl
    GLuint ibo; glGenBuffers(1,&ibo); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,ibo);
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, Q*6*isz, idx8?(void*)i8:(void*)i16, GL_STATIC_DRAW);

    int map_fail=0;
    for(int frame=0; frame<frames; frame++){
        glBindBuffer(GL_ARRAY_BUFFER,vbo);
        if(use_map){
            // cogl's path: map WRITE|INVALIDATE_BUFFER, memcpy the verts, unmap. EVERY frame.
            void* p=glMapBufferRange(GL_ARRAY_BUFFER,0,vbytes,GL_MAP_WRITE_BIT|GL_MAP_INVALIDATE_BUFFER_BIT);
            if(!p){ if(!map_fail){printf("glMapBufferRange NULL err=0x%x (frame %d)\n",glGetError(),frame); map_fail=1;} }
            else { memcpy(p,cpu,vbytes); glUnmapBuffer(GL_ARRAY_BUFFER); }
        } else {
            glBufferData(GL_ARRAY_BUFFER, vbytes, cpu, GL_DYNAMIC_DRAW);  // control: copy upload
        }
        glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,4*sizeof(float),0);
        glVertexAttribPointer(1,2,GL_FLOAT,GL_FALSE,4*sizeof(float),(void*)(2*sizeof(float)));
        glClearColor(0.1,0.1,0.12,1.); glClear(GL_COLOR_BUFFER_BIT);
        glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,ibo);
        glDrawElements(GL_TRIANGLES, Q*6, itype, 0);
        eglSwapBuffers(dpy,surf);   // swap each frame so the GPU keeps consuming, like a live desktop
    }
    glFinish();
    printf("glGetError=0x%x map_fail=%d\n", glGetError(), map_fail);

    struct gbm_bo* bo=gbm_surface_lock_front_buffer(gs); if(!bo){fprintf(stderr,"lock NULL\n");return 1;}
    uint32_t handle=gbm_bo_get_handle(bo).u32, stride=gbm_bo_get_stride(bo), fb=0;
    drmModeAddFB(fd,W,H,24,32,stride,handle,&fb);
    int r=drmModeSetCrtc(fd,crtc_id,fb,0,0,&conn_id,1,&mode_i);
    printf("setcrtc=%d holding 600s...\n",r);
    sleep(600);
    return 0;
}
