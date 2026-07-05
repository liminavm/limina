// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Real ARB_timer_query exercise on zink-on-KK: glQueryCounter(GL_TIMESTAMP) +
// GL_TIME_ELAPSED around actual GPU work. Verifies the KK timestamp impl, not
// just the advertised extension.
#include <EGL/egl.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
typedef void (*GLGENQUERIES)(GLsizei,GLuint*);
typedef void (*GLQUERYCOUNTER)(GLuint,GLenum);
typedef void (*GLBEGINQUERY)(GLenum,GLuint);
typedef void (*GLENDQUERY)(GLenum);
typedef void (*GLGETQUERYOBJECTUI64V)(GLuint,GLenum,uint64_t*);
typedef void (*GLGETQUERYOBJECTIV)(GLuint,GLenum,GLint*);
typedef void (*GLGETINTEGER64V)(GLenum,int64_t*);
typedef void (*GLCLEAR)(GLbitfield);
typedef void (*GLCLEARCOLOR)(GLfloat,GLfloat,GLfloat,GLfloat);
typedef void (*GLFLUSH)(void);
typedef void (*GLFINISH)(void);
#define GL_TIMESTAMP 0x8E28
#define GL_TIME_ELAPSED 0x88BF
#define GL_QUERY_RESULT 0x8866
#define GL_QUERY_RESULT_AVAILABLE 0x8867
#define L(t,n) t n=(t)eglGetProcAddress(#n); if(!n){fprintf(stderr,"miss %s\n",#n);return 3;}
int main(void){
  EGLDisplay dpy=eglGetDisplay(EGL_DEFAULT_DISPLAY); eglInitialize(dpy,0,0);
  eglBindAPI(EGL_OPENGL_API);
  EGLint ca[]={EGL_RENDERABLE_TYPE,EGL_OPENGL_BIT,EGL_SURFACE_TYPE,EGL_PBUFFER_BIT,EGL_NONE};
  EGLConfig cfg; EGLint n; eglChooseConfig(dpy,ca,&cfg,1,&n);
  EGLint cx[]={EGL_CONTEXT_MAJOR_VERSION,3,EGL_CONTEXT_MINOR_VERSION,3,EGL_CONTEXT_OPENGL_PROFILE_MASK,EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,EGL_NONE};
  EGLContext ctx=eglCreateContext(dpy,cfg,EGL_NO_CONTEXT,cx);
  if(ctx==EGL_NO_CONTEXT){fprintf(stderr,"no 3.3 ctx\n");return 2;}
  EGLint pb[]={EGL_WIDTH,256,EGL_HEIGHT,256,EGL_NONE};
  EGLSurface s=eglCreatePbufferSurface(dpy,cfg,pb); eglMakeCurrent(dpy,s,s,ctx);
  L(GLGENQUERIES,glGenQueries) L(GLQUERYCOUNTER,glQueryCounter)
  L(GLBEGINQUERY,glBeginQuery) L(GLENDQUERY,glEndQuery)
  L(GLGETQUERYOBJECTUI64V,glGetQueryObjectui64v) L(GLGETQUERYOBJECTIV,glGetQueryObjectiv)
  L(GLGETINTEGER64V,glGetInteger64v) L(GLCLEAR,glClear) L(GLCLEARCOLOR,glClearColor)
  L(GLFLUSH,glFlush) L(GLFINISH,glFinish)
  // glGetInteger64v(GL_TIMESTAMP): immediate host-side timestamp
  int64_t ts_now=0; glGetInteger64v(GL_TIMESTAMP,&ts_now);
  printf("glGetInteger64v(GL_TIMESTAMP) = %lld ns\n",(long long)ts_now);
  // glQueryCounter around GPU work
  GLuint q[2]; glGenQueries(2,q);
  glQueryCounter(q[0],GL_TIMESTAMP);
  for(int i=0;i<200;i++){glClearColor((i&1)?0.2f:0.8f,0,0,1);glClear(GL_COLOR_BUFFER_BIT);}
  glQueryCounter(q[1],GL_TIMESTAMP);
  // GL_TIME_ELAPSED around more work
  GLuint te; glGenQueries(1,&te);
  glBeginQuery(GL_TIME_ELAPSED,te);
  for(int i=0;i<200;i++){glClearColor(0,(i&1)?0.2f:0.8f,0,1);glClear(GL_COLOR_BUFFER_BIT);}
  glEndQuery(GL_TIME_ELAPSED);
  glFinish();
  uint64_t t0=0,t1=0,elapsed=0;
  glGetQueryObjectui64v(q[0],GL_QUERY_RESULT,&t0);
  glGetQueryObjectui64v(q[1],GL_QUERY_RESULT,&t1);
  glGetQueryObjectui64v(te,GL_QUERY_RESULT,&elapsed);
  printf("glQueryCounter t0      = %llu ns\n",(unsigned long long)t0);
  printf("glQueryCounter t1      = %llu ns\n",(unsigned long long)t1);
  printf("  t1-t0 (200 clears)   = %lld ns   monotonic? %s\n",(long long)(t1-t0), t1>t0?"YES":"NO");
  printf("GL_TIME_ELAPSED        = %llu ns  (nonzero? %s)\n",(unsigned long long)elapsed, elapsed>0?"YES":"NO");
  int pass = (t1>t0) && (elapsed>0) && (t0>0);
  printf("RESULT: %s\n", pass?"PASS — timer queries return sane GPU nanoseconds":"FAIL");
  return pass?0:1;
}
