/* Minimal repro: render to a GL texture, export it via EGL_MESA_image_dma_buf_export,
 * then read the exported dmabuf back and compare with glReadPixels of the same FBO.
 * Isolates the virgl dmabuf export path from any toolkit.
 *   cc dmabuf_export_test.c -o dmabuf_export_test -lEGL -lGLESv2 -lgbm -ldrm
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <gbm.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define W 256
#define H 256
#define DIE(msg) do { fprintf(stderr, "FAIL: %s (line %d)\n", msg, __LINE__); exit(1); } while (0)

int main(void)
{
    int drm_fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (drm_fd < 0) DIE("open renderD128");
    struct gbm_device *gbm = gbm_create_device(drm_fd);
    if (!gbm) DIE("gbm_create_device");

    PFNEGLGETPLATFORMDISPLAYEXTPROC getdpy =
        (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = getdpy(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    if (dpy == EGL_NO_DISPLAY) DIE("eglGetPlatformDisplay");
    if (!eglInitialize(dpy, NULL, NULL)) DIE("eglInitialize");
    printf("EGL_VENDOR   : %s\n", eglQueryString(dpy, EGL_VENDOR));

    const char *exts = eglQueryString(dpy, EGL_EXTENSIONS);
    printf("dma_buf_export extension: %s\n",
           strstr(exts, "EGL_MESA_image_dma_buf_export") ? "yes" : "NO");

    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint cattr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
    if (ctx == EGL_NO_CONTEXT) DIE("eglCreateContext");
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) DIE("eglMakeCurrent");
    printf("GL_RENDERER  : %s\n\n", glGetString(GL_RENDERER));

    /* Render a known pattern into a texture via an FBO. */
    GLuint tex, fbo;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, W, H, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) DIE("fbo");
    glClearColor(0.25f, 0.5f, 0.75f, 1.0f);   /* -> roughly 40 80 BF FF */
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();

    unsigned char expect[4];
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, expect);
    printf("glReadPixels of FBO : %02X %02X %02X %02X  <- ground truth\n",
           expect[0], expect[1], expect[2], expect[3]);

    /* Export that same texture as a dmabuf. */
    PFNEGLCREATEIMAGEKHRPROC createimg = (void *)eglGetProcAddress("eglCreateImageKHR");
    PFNEGLEXPORTDMABUFIMAGEQUERYMESAPROC q =
        (void *)eglGetProcAddress("eglExportDMABUFImageQueryMESA");
    PFNEGLEXPORTDMABUFIMAGEMESAPROC ex =
        (void *)eglGetProcAddress("eglExportDMABUFImageMESA");
    if (!createimg || !q || !ex) DIE("missing entrypoints");

    EGLImageKHR img = createimg(dpy, ctx, EGL_GL_TEXTURE_2D_KHR,
                                (EGLClientBuffer)(uintptr_t)tex, NULL);
    if (img == EGL_NO_IMAGE_KHR) DIE("eglCreateImageKHR");

    int fourcc = 0, nplanes = 0;
    EGLuint64KHR mods[4] = {0};
    if (!q(dpy, img, &fourcc, &nplanes, mods)) DIE("eglExportDMABUFImageQueryMESA");
    printf("\nquery: fourcc=%.4s planes=%d modifier=0x%016llx\n",
           (char *)&fourcc, nplanes, (unsigned long long)mods[0]);

    int fds[4] = { -1, -1, -1, -1 };
    EGLint strides[4] = {0}, offsets[4] = {0};
    EGLBoolean ok = ex(dpy, img, fds, strides, offsets);
    printf("eglExportDMABUFImageMESA returned %s\n", ok ? "EGL_TRUE" : "EGL_FALSE");
    printf("  fd=%d stride=%d offset=%d\n", fds[0], strides[0], offsets[0]);

    if (!ok || fds[0] < 0) {
        printf("\n==> EXPORT FAILED. No valid dmabuf fd was produced.\n");
        return 2;
    }

    off_t size = lseek(fds[0], 0, SEEK_END);
    printf("  lseek(SEEK_END) = %lld (expected >= %d)\n", (long long)size, W * H * 4);
    if (size <= 0) { printf("\n==> dmabuf fd is not seekable/sized.\n"); return 3; }

    unsigned char *map = mmap(NULL, size, PROT_READ, MAP_SHARED, fds[0], 0);
    if (map == MAP_FAILED) { perror("  mmap"); return 4; }
    printf("  dmabuf first pixel  : %02X %02X %02X %02X\n",
           map[0], map[1], map[2], map[3]);

    int mismatch = 0;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            unsigned char *p = map + (size_t)y * strides[0] + x * 4;
            if (memcmp(p, expect, 4)) mismatch++;
        }
    printf("  mismatching pixels  : %d / %d\n", mismatch, W * H);
    printf("\n==> %s\n", mismatch ? "DMABUF CONTENTS ARE WRONG" : "dmabuf matches glReadPixels");
    return mismatch ? 5 : 0;
}
