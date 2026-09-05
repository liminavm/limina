// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* planar-transfer-probe — oracle for the planar-YUV transfer bound/access mismatch.
 *
 * vrend registers the planar YUV formats with a four-byte GL triple
 * (GL_RGBA8/GL_RGBA/GL_UNSIGNED_BYTE, vrend_formats.c yuv_planar_formats) so that a
 * converted planar blob can be sampled as RGBA. util_format_get_blocksize() for those
 * same formats is 1. Every bound in the transfer path is computed from the blocksize and
 * every access is performed with the triple, so the access is FOUR TIMES the bound --
 * for any iov size, including a correctly sized one.
 *
 * This drives virglrenderer's own public API, the same entry points rutabaga calls, and
 * makes the over-read fault DETERMINISTICALLY rather than relying on a sanitizer or on
 * whatever happens to sit after a malloc:
 *
 *   mmap 2 pages -> mprotect the second PROT_NONE -> place the iov's 6144 honest NV12
 *   bytes so they END exactly at the page boundary.
 *
 * A correct transfer touches 6144 bytes and returns. The buggy one asks GL for
 * 64 rows x ROW_LENGTH(64) texels x 4 bytes = 16384, walks 10240 bytes into the guard
 * page, and takes SIGSEGV/SIGBUS -- which this catches and reports as the verdict.
 *
 * Two directions, run as separate processes (argv[1] = "write" | "read") because a
 * caught fault leaves the GL driver's state untrustworthy for a second test:
 *
 *   write  TRANSFER_TO_HOST   -- out-of-bounds READ. Guard page gives a hard verdict.
 *   read   TRANSFER_FROM_HOST -- the overflow is a WRITE into a host heap temp
 *                                (vrend_transfer_send_getteximage mallocs w*h and
 *                                glGetTexImage writes w*h*4), so the guard page does
 *                                NOT see it. The honest oracle here is the return code:
 *                                0 pre-fix, EINVAL post-fix. Reported as such.
 *
 * The probe also answers two questions the source alone could not settle, both of which
 * change the severity of the readback direction:
 *   - is this host's vrend desktop GL or GLES? (GLES routes readback to
 *     vrend_transfer_send_readonly, which never allocates and cannot overflow)
 *   - does the NV12 resource create even succeed here? (the Apple last-line refusal in
 *     vrend_renderer.c fires when no planar IOSurface could be allocated)
 *
 * Run via run-probe.sh, which supplies the host GL env the worker gets.
 */
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>

#include "virglrenderer.h"
#include "virgl_hw.h"

/* Generated from u_format.yaml's `alias: NV12`; the canonical spelling is in virgl_hw.h. */
#define PROBE_FORMAT_NV12 VIRGL_FORMAT_Y8_U8V8_420_UNORM

#define W 64
#define H 64
/* What an honest NV12 iov holds: luma W*H + interleaved chroma (W/2)*(H/2)*2. */
#define NV12_BYTES ((size_t)W * H * 3 / 2)
/* What the RGBA8 triple actually makes GL read for the same box. */
#define GL_READ_BYTES ((size_t)W * H * 4)

#define RES_HANDLE 1
#define CTX_ID 1

static sigjmp_buf fault_env;
static volatile sig_atomic_t faulted;
static volatile sig_atomic_t armed;

static void
fault_handler(int sig)
{
   if (!armed)
      _exit(70); /* a fault outside the guarded window is a probe bug, not a verdict */
   faulted = sig;
   siglongjmp(fault_env, 1);
}

static void
install_fault_handler(void)
{
   struct sigaction sa;
   memset(&sa, 0, sizeof(sa));
   sa.sa_handler = fault_handler;
   sigemptyset(&sa.sa_mask);
   sa.sa_flags = SA_NODEFER;
   sigaction(SIGSEGV, &sa, NULL);
   sigaction(SIGBUS, &sa, NULL);
}

static void
log_cb(enum virgl_log_level_flags level, const char *message, void *user_data)
{
   static const char *names[] = { "debug", "info", "warn", "error", "silent" };
   (void)user_data;
   printf("  [virgl %s] %s\n",
          level <= VIRGL_LOG_LEVEL_SILENT ? names[level] : "?", message);
}

static void
write_fence(void *cookie, uint32_t fence)
{
   (void)cookie;
   (void)fence;
}

static struct virgl_renderer_callbacks cbs = {
   .version = 1,
   .write_fence = write_fence,
};

/* Two pages: the first holds the iov, the second is PROT_NONE. The iov's bytes are placed
 * flush against the boundary so an over-read of ANY length lands in the guard. */
static uint8_t *
guarded_iov(size_t len, uint8_t **region_out, size_t *region_len_out)
{
   long page = sysconf(_SC_PAGESIZE);
   size_t span = (size_t)page * 2;
   uint8_t *region = mmap(NULL, span, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANON, -1, 0);
   if (region == MAP_FAILED) {
      perror("mmap");
      return NULL;
   }
   if (mprotect(region + page, (size_t)page, PROT_NONE) != 0) {
      perror("mprotect");
      munmap(region, span);
      return NULL;
   }
   *region_out = region;
   *region_len_out = span;
   /* End the buffer exactly at the guard boundary. */
   return region + page - len;
}

int
main(int argc, char **argv)
{
   /* Unbuffered: a fault anywhere here must not swallow the lines that say how far
    * we got. Block-buffered stdout loses exactly the evidence this probe exists for. */
   setvbuf(stdout, NULL, _IONBF, 0);

   const char *mode = argc > 1 ? argv[1] : "write";
   const int do_write = strcmp(mode, "read") != 0;

   virgl_set_log_callback(log_cb, NULL, NULL);

   printf("== planar-transfer-probe (%s direction) ==\n", do_write ? "TRANSFER_TO_HOST" : "TRANSFER_FROM_HOST");
   printf("resource: %dx%d NV12 (VIRGL_FORMAT_Y8_U8V8_420_UNORM = %d)\n", W, H, PROBE_FORMAT_NV12);
   printf("honest NV12 iov: %zu bytes; what the RGBA8 triple makes GL touch: %zu bytes\n",
          NV12_BYTES, GL_READ_BYTES);

   /* The worker's vrend winsys config (GPU_COEXIST_FLAGS in crates/limina-vmm/src/krun):
    * EGL + surfaceless + GLES. USE_GLES is not optional on macOS -- without it epoxy routes
    * desktop-GL calls to Apple's OpenGL framework and virgl_egl_init dies in glFlush with no
    * CGL context. Venus and the render server are deliberately left out: this probe exercises
    * vrend's transfer path only. */
   static int cookie; /* vrend rejects a NULL cookie outright; the value is never read here */
   int ret = virgl_renderer_init(&cookie,
                                 VIRGL_RENDERER_USE_EGL | VIRGL_RENDERER_USE_SURFACELESS |
                                    VIRGL_RENDERER_USE_GLES,
                                 &cbs);
   if (ret) {
      printf("FAIL: virgl_renderer_init returned %d (missing host GL env? see run-probe.sh)\n", ret);
      return 2;
   }
   printf("virgl_renderer_init: ok\n");

   ret = virgl_renderer_context_create(CTX_ID, strlen("probe"), "probe");
   if (ret) {
      printf("FAIL: context_create returned %d\n", ret);
      return 2;
   }

   struct virgl_renderer_resource_create_args args = {
      .handle = RES_HANDLE,
      .target = 2 /* PIPE_TEXTURE_2D */,
      .format = PROBE_FORMAT_NV12,
      .bind = VIRGL_BIND_SAMPLER_VIEW,
      .width = W,
      .height = H,
      .depth = 1,
      .array_size = 1,
      .last_level = 0,
      .nr_samples = 0,
      .flags = 0,
   };
   ret = virgl_renderer_resource_create(&args, NULL, 0);
   if (ret) {
      printf("INCONCLUSIVE: NV12 resource_create returned %d — this host refuses the planar\n"
             "create, so the transfer path is unreachable HERE and the probe proves nothing\n"
             "about it. That is itself a finding: reachability is narrower than the capset\n"
             "implies. Do not read this as a pass.\n", ret);
      return 3;
   }
   printf("NV12 resource_create: ok (the create the capset's sampler bitmask invites)\n");

   virgl_renderer_ctx_attach_resource(CTX_ID, RES_HANDLE);

   uint8_t *region = NULL;
   size_t region_len = 0;
   uint8_t *buf = guarded_iov(NV12_BYTES, &region, &region_len);
   if (!buf)
      return 2;
   memset(buf, 0x5a, NV12_BYTES);

   struct iovec iov = { .iov_base = buf, .iov_len = NV12_BYTES };
   ret = virgl_renderer_resource_attach_iov(RES_HANDLE, &iov, 1);
   if (ret) {
      printf("FAIL: attach_iov returned %d\n", ret);
      return 2;
   }
   printf("attached a CORRECTLY SIZED NV12 iov (%zu bytes), its last byte flush against a\n"
          "PROT_NONE guard page\n", NV12_BYTES);

   struct virgl_box box = { .x = 0, .y = 0, .z = 0, .w = W, .h = H, .d = 1 };

   install_fault_handler();

   int rc = -12345;
   if (sigsetjmp(fault_env, 1) == 0) {
      armed = 1;
      if (do_write) {
         rc = virgl_renderer_transfer_write_iov(RES_HANDLE, CTX_ID, 0, 0, 0, &box, 0, &iov, 1);
      } else {
         /* A DIFFERENT iov than the attached one on purpose: vrend_transfer_send_readonly
          * short-circuits to success when the iovs match, which would hide the path. */
         uint8_t *dst_region = NULL;
         size_t dst_len = 0;
         uint8_t *dst = guarded_iov(NV12_BYTES, &dst_region, &dst_len);
         if (!dst)
            return 2;
         struct iovec dst_iov = { .iov_base = dst, .iov_len = NV12_BYTES };
         rc = virgl_renderer_transfer_read_iov(RES_HANDLE, CTX_ID, 0, 0, 0, &box, 0, &dst_iov, 1);
      }
      armed = 0;
   }

   if (faulted) {
      printf("\nRED: OVER-READ OBSERVED — %s while transferring a correctly sized NV12 iov.\n",
             faulted == SIGBUS ? "SIGBUS" : "SIGSEGV");
      printf("The guard page was reached, so the access ran past the %zu bytes the bound\n"
             "admitted. Nothing in the guest's request was malformed.\n", NV12_BYTES);
      return 1;
   }

   printf("\nno fault. return code = %d (%s)\n", rc,
          rc == 0 ? "success" : rc == EINVAL ? "EINVAL" : "other");
   if (do_write) {
      if (rc == 0) {
         printf("RED (silent variant): the transfer REPORTED SUCCESS. At this size the\n"
                "over-read stayed mapped; at 2560x1440 the same path faults. A pass here is\n"
                "the bug's quiet face, not its absence.\n");
         return 1;
      }
      printf("GREEN: the transfer was refused rather than performed.\n");
      return 0;
   }

   /* Readback direction. */
   if (rc == 0) {
      printf("RED: the readback was PERFORMED on a planar resource. Whether it overflowed the\n"
             "host heap temp depends on this host's GL: desktop GL routes to\n"
             "vrend_transfer_send_getteximage (malloc w*h, glGetTexImage writes w*h*4);\n"
             "GLES routes to vrend_transfer_send_readonly, which allocates nothing. Read the\n"
             "GL version in the [virgl] log above.\n");
      return 1;
   }
   if (rc == EINVAL) {
      printf("GREEN: the readback was refused rather than performed.\n");
      return 0;
   }
   printf("INCONCLUSIVE: readback returned %d — on GLES vrend_transfer_send_readonly returns\n"
          "-1 for a non-matching iov, which is a refusal by accident, not by the fix.\n", rc);
   return 3;
}
