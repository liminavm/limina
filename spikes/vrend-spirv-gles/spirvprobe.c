// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Does the host Mesa take SPIR-V in vrend's kind of context -- EGL surfaceless, GLES 3.1, zink on
// KosmicKrisp? Loads tri.vert.spv / tri.frag.spv with glShaderBinary(SPIR-V) + glSpecializeShader,
// links, draws into an FBO and reads the pixels back: inside the triangle must be the uniform
// block's colour, outside the clear colour. Exit 0 only when both are right.
//
// Usage: spirvprobe <dir holding the .spv files>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#define GL_SHADER_BINARY_FORMAT_SPIR_V 0x9551
typedef void (*SpecializeFn)(GLuint, const GLchar *, GLuint, const GLuint *, const GLuint *);

static unsigned char *slurp(const char *path, long *len)
{
    FILE *f = fopen(path, "rb");
    if (!f) {
        perror(path);
        exit(2);
    }
    fseek(f, 0, SEEK_END);
    *len = ftell(f);
    fseek(f, 0, SEEK_SET);
    unsigned char *buf = malloc(*len);
    if (fread(buf, 1, *len, f) != (size_t)*len) {
        perror(path);
        exit(2);
    }
    fclose(f);
    return buf;
}

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

static GLuint spirv_shader(GLenum stage, const char *path, SpecializeFn specialize)
{
    long len;
    unsigned char *bin = slurp(path, &len);
    GLuint sh = glCreateShader(stage);
    glShaderBinary(1, &sh, GL_SHADER_BINARY_FORMAT_SPIR_V, bin, (GLsizei)len);
    GLenum err = glGetError();
    if (err != GL_NO_ERROR) {
        printf("glShaderBinary(SPIR-V, %s): GL error 0x%x\n", path, err);
        return 0;
    }
    specialize(sh, "main", 0, NULL, NULL);
    err = glGetError();
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (err != GL_NO_ERROR || !ok) {
        char log[1024] = "";
        glGetShaderInfoLog(sh, sizeof log, NULL, log);
        printf("glSpecializeShader(%s): GL error 0x%x, compile status %d: %s\n", path, err, ok, log);
        return 0;
    }
    free(bin);
    return sh;
}

int main(int argc, char **argv)
{
    const char *dir = argc > 1 ? argv[1] : ".";
    char vs_path[1024], fs_path[1024];
    snprintf(vs_path, sizeof vs_path, "%s/tri.vert.spv", dir);
    snprintf(fs_path, sizeof fs_path, "%s/tri.frag.spv", dir);

    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = get_display(EGL_PLATFORM_SURFACELESS_MESA, NULL, NULL);
    if (!eglInitialize(dpy, NULL, NULL) || !eglBindAPI(EGL_OPENGL_ES_API)) {
        printf("EGL init failed: 0x%x\n", eglGetError());
        return 2;
    }
    const EGLint ctx_attrs[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 1,
                                EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        printf("GLES 3.1 context failed: 0x%x\n", eglGetError());
        return 2;
    }
    printf("GL_VERSION  = %s\nGL_RENDERER = %s\n", glGetString(GL_VERSION),
           glGetString(GL_RENDERER));

    GLint nformats = 0;
    glGetIntegerv(GL_NUM_SHADER_BINARY_FORMATS, &nformats);
    printf("shader binary formats: %d\n", nformats);

    SpecializeFn specialize = (SpecializeFn)eglGetProcAddress("glSpecializeShader");
    if (!specialize)
        specialize = (SpecializeFn)eglGetProcAddress("glSpecializeShaderARB");
    if (!specialize) {
        printf("FAIL: no glSpecializeShader entry point\n");
        return 1;
    }

    double t0 = now_ms();
    GLuint vs = spirv_shader(GL_VERTEX_SHADER, vs_path, specialize);
    GLuint fs = spirv_shader(GL_FRAGMENT_SHADER, fs_path, specialize);
    if (!vs || !fs) {
        printf("FAIL: the host does not take SPIR-V in a GLES context\n");
        return 1;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    GLint linked = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = "";
        glGetProgramInfoLog(prog, sizeof log, NULL, log);
        printf("FAIL: link: %s\n", log);
        return 1;
    }
    printf("SPIR-V shaders + link: %.2f ms\n", now_ms() - t0);

    GLuint tex, fbo, ubo;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, 64, 64);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    const float color[4] = {0.0f, 1.0f, 0.0f, 1.0f};
    glGenBuffers(1, &ubo);
    glBindBuffer(GL_UNIFORM_BUFFER, ubo);
    glBufferData(GL_UNIFORM_BUFFER, sizeof color, color, GL_STATIC_DRAW);
    glBindBufferBase(GL_UNIFORM_BUFFER, 80, ubo);

    GLuint vao;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glViewport(0, 0, 64, 64);
    glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    // consts[1] = (1,1,1,1): location 0 is the array's first element, so element 1 is location 1.
    const GLuint ones[4] = {1, 1, 1, 1};
    glUniform4uiv(1, 1, ones);
    if (glGetError() != GL_NO_ERROR) {
        printf("FAIL: glUniform4uiv by location\n");
        return 1;
    }
    glDrawArrays(GL_TRIANGLES, 0, 3);

    unsigned char inside[4], outside[4];
    glReadPixels(32, 28, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, inside);
    glReadPixels(2, 60, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, outside);
    printf("inside  = %u %u %u %u (want 0 255 0 255)\n", inside[0], inside[1], inside[2],
           inside[3]);
    printf("outside = %u %u %u %u (want 255 0 0 255)\n", outside[0], outside[1], outside[2],
           outside[3]);
    int pass = inside[0] == 0 && inside[1] == 255 && outside[0] == 255 && outside[1] == 0;
    printf("%s\n", pass ? "PASS" : "FAIL: wrong pixels");
    return pass ? 0 : 1;
}
