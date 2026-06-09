// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Controlled vehicle stage 4: TEXTURED TRIANGLE_FAN.
//
// Bug 2 (still open after the uint8 fix): seated-desktop dock icons render as the BOTTOM-LEFT
// triangle of each quad — top-right triangle missing, clean TL-BR diagonal, and the TEXTURE is
// correct in the half that does draw. quads.c/fbotex.c (indexed TRIANGLE LIST) and primtest.c
// (UNTEXTURED fan/strip/tris) all render BOTH triangles fine. The one variable those vehicles
// never combined is the icon's actual shape: a TEXTURED 4-vertex TRIANGLE_FAN (cogl's single-rect
// path). This draws an NxN grid of textured fans; each cell samples a gradient texture so a dropped
// triangle is unmistakable (and we can confirm the surviving half's texels are correct).
//
// Env: TEXTEST_N=<n> grid (default 8). MODE=fan|tris (default fan; tris = indexed list control).
// Build (guest): gcc texfan.c -o texfan -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm -lm
// Run (zink env, gdm stopped): ./texfan ; host: iosdump <scanout id>.
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
    int N = getenv("TEXTEST_N")?atoi(getenv("TEXTEST_N")):8; if(N<1)N=1;
    // MODE=fan (per-rect TRIANGLE_FAN), tris (per-rect indexed list), or batch (cogl-faithful:
    // ONE glDrawElements, uint16 indices, vertex order TL,BL,BR,TR, pattern {0,1,2, 0,2,3} per
    // quad across the whole grid — exactly cogl-journal.c:1216 + cogl-indices.c).
    const char* mode = getenv("MODE"); if(!mode) mode="fan";
    int use_tris = !strcmp(mode,"tris");
    int use_batch = !strcmp(mode,"batch");
    printf("texfan N=%d mode=%s\n", N, mode);

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

    // p=pos (vec2), t=texcoord (vec2). Texture: horizontal gradient (red->green) so the TL-BR
    // diagonal split and per-texel correctness are both visible.
    const char* vs="attribute vec2 p; attribute vec2 t; varying vec2 uv; void main(){uv=t; gl_Position=vec4(p,0.,1.);}";
    const char* fs="precision mediump float; varying vec2 uv; uniform sampler2D s; void main(){gl_FragColor=texture2D(s,uv);}";
    GLuint v=glCreateShader(GL_VERTEX_SHADER); glShaderSource(v,1,&vs,0); glCompileShader(v);
    GLuint f=glCreateShader(GL_FRAGMENT_SHADER); glShaderSource(f,1,&fs,0); glCompileShader(f);
    GLuint pr=glCreateProgram(); glAttachShader(pr,v); glAttachShader(pr,f);
    glBindAttribLocation(pr,0,"p"); glBindAttribLocation(pr,1,"t"); glLinkProgram(pr);
    GLint ln=0; glGetProgramiv(pr,GL_LINK_STATUS,&ln); printf("link=%d\n",ln); glUseProgram(pr);

    // 8x8 texture: x-gradient red->green, with a blue stripe near the top so a dropped TOP triangle
    // is obvious by missing blue.
    unsigned char tex[8*8*4];
    for(int y=0;y<8;y++)for(int x=0;x<8;x++){int o=(y*8+x)*4; tex[o]=x*36; tex[o+1]=(7-x)*36; tex[o+2]=(y<2)?255:0; tex[o+3]=255;}
    GLuint t; glGenTextures(1,&t); glBindTexture(GL_TEXTURE_2D,t);
    glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA,8,8,0,GL_RGBA,GL_UNSIGNED_BYTE,tex);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_NEAREST);
    glUniform1i(glGetUniformLocation(pr,"s"),0);

    glViewport(0,0,W,H); glDisable(GL_DEPTH_TEST); glDisable(GL_CULL_FACE);
    glClearColor(0.1,0.1,0.12,1.); glClear(GL_COLOR_BUFFER_BIT);
    glEnableVertexAttribArray(0); glEnableVertexAttribArray(1);

    int Q=N*N;
    float cell=2.f/N, gap=cell*0.18f, s=cell-gap;
    if (use_batch) {
        // cogl-faithful: 4 verts/quad in order TL,BL,BR,TR (cogl-journal.c:1216-1223),
        // indexed glDrawElements with pattern {0,1,2, 0,2,3} per quad (cogl-indices.c).
        // IDX8=1 -> uint8 indices (cogl uses uint8 for <=64 rects). OFFSET=1 -> theory O: draw the
        // grid in TWO groups, the SECOND via glDrawElements at a NON-ZERO first_vertex (index) offset
        // 6*(Q/2), exactly like cogl's per-batch current_vertex*6/4. If only the offset group breaks,
        // the non-zero index-buffer base offset is the venus/MoltenVK bug.
        int idx8 = getenv("IDX8")?atoi(getenv("IDX8")):0;
        int off  = getenv("OFFSET")?atoi(getenv("OFFSET")):0;
        printf("batch: idx8=%d offset=%d Q=%d\n", idx8, off, Q);
        float* all=malloc(Q*4*4*sizeof(float));
        unsigned char*  ind8 =malloc(Q*6);
        unsigned short* ind16=malloc(Q*6*sizeof(short));
        int vi=0, ii=0, base=0;
        for(int gy=0;gy<N;gy++)for(int gx=0;gx<N;gx++){
            float x0=-1.f+gx*cell+gap*.5f, y0=-1.f+gy*cell+gap*.5f, x1=x0+s, y1=y0+s;
            // TL=(x0,y1) BL=(x0,y0) BR=(x1,y0) TR=(x1,y1); uv: TL(0,0) BL(0,1) BR(1,1) TR(1,0)
            float TL[4]={x0,y1,0,0},BL[4]={x0,y0,0,1},BR[4]={x1,y0,1,1},TR[4]={x1,y1,1,0};
            memcpy(all+vi,TL,16);memcpy(all+vi+4,BL,16);memcpy(all+vi+8,BR,16);memcpy(all+vi+12,TR,16); vi+=16;
            int seq[6]={base+0,base+1,base+2, base+0,base+2,base+3}; // tri1 BL-half, tri2 TR-half
            for(int k=0;k<6;k++){ ind16[ii+k]=seq[k]; ind8[ii+k]=seq[k]; }
            ii+=6; base+=4;
        }
        GLuint vbo; glGenBuffers(1,&vbo); glBindBuffer(GL_ARRAY_BUFFER,vbo);
        glBufferData(GL_ARRAY_BUFFER, Q*4*4*sizeof(float), all, GL_STATIC_DRAW);
        GLuint ibo; glGenBuffers(1,&ibo); glBindBuffer(GL_ELEMENT_ARRAY_BUFFER,ibo);
        GLenum itype = idx8?GL_UNSIGNED_BYTE:GL_UNSIGNED_SHORT;
        int isz = idx8?1:2;
        glBufferData(GL_ELEMENT_ARRAY_BUFFER, Q*6*isz, idx8?(void*)ind8:(void*)ind16, GL_STATIC_DRAW);
        glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,4*sizeof(float),0);
        glVertexAttribPointer(1,2,GL_FLOAT,GL_FALSE,4*sizeof(float),(void*)(2*sizeof(float)));
        if (off) {
            int half=Q/2;
            glDrawElements(GL_TRIANGLES, half*6, itype, 0);                     // group A: offset 0
            glDrawElements(GL_TRIANGLES, (Q-half)*6, itype, (void*)(half*6*isz)); // group B: NON-ZERO offset
        } else {
            glDrawElements(GL_TRIANGLES, Q*6, itype, 0);                        // ONE batched indexed draw
        }
    } else {
        int vpq = use_tris?6:4;
        float* all=malloc(Q*vpq*4*sizeof(float)); int idx=0;
        for(int gy=0;gy<N;gy++)for(int gx=0;gx<N;gx++){
            float x0=-1.f+gx*cell+gap*.5f, y0=-1.f+gy*cell+gap*.5f, x1=x0+s, y1=y0+s;
            // corner: pos + uv. TL uv(0,0) TR uv(1,0) BR uv(1,1) BL uv(0,1)
            float TL[4]={x0,y1,0,0},TR[4]={x1,y1,1,0},BR[4]={x1,y0,1,1},BL[4]={x0,y0,0,1};
            if(!use_tris){            // FAN: TL,TR,BR,BL
                float q[16]; memcpy(q,TL,16);memcpy(q+4,TR,16);memcpy(q+8,BR,16);memcpy(q+12,BL,16);
                memcpy(all+idx,q,sizeof q); idx+=16;
            } else {                  // 6-vert list: TL,TR,BL, BL,TR,BR
                float q[24]; memcpy(q,TL,16);memcpy(q+4,TR,16);memcpy(q+8,BL,16);
                memcpy(q+12,BL,16);memcpy(q+16,TR,16);memcpy(q+20,BR,16);
                memcpy(all+idx,q,sizeof q); idx+=24;
            }
        }
        GLuint vbo; glGenBuffers(1,&vbo); glBindBuffer(GL_ARRAY_BUFFER,vbo);
        glBufferData(GL_ARRAY_BUFFER, Q*vpq*4*sizeof(float), all, GL_STATIC_DRAW);
        glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,4*sizeof(float),0);
        glVertexAttribPointer(1,2,GL_FLOAT,GL_FALSE,4*sizeof(float),(void*)(2*sizeof(float)));
        GLenum gm = use_tris?GL_TRIANGLES:GL_TRIANGLE_FAN;
        for(int q=0;q<Q;q++) glDrawArrays(gm, q*vpq, vpq);  // per-rect, non-zero first vertex
    }
    glFinish();
    printf("glGetError=0x%x\n", glGetError());
    eglSwapBuffers(dpy,surf);
    struct gbm_bo* bo=gbm_surface_lock_front_buffer(gs); if(!bo){fprintf(stderr,"lock NULL\n");return 1;}
    uint32_t handle=gbm_bo_get_handle(bo).u32, stride=gbm_bo_get_stride(bo), fb=0;
    drmModeAddFB(fd,W,H,24,32,stride,handle,&fb);
    int r=drmModeSetCrtc(fd,crtc_id,fb,0,0,&conn_id,1,&mode_i);
    printf("setcrtc=%d holding 600s...\n",r);
    sleep(600);
    return 0;
}
