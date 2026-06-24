// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Minimal gbm probe: confirm zink-on-venus dma-buf EXPORT capability without the
// crashing gnome-shell. gbm_create_device triggers the instrumented LIMINA_IMP cap log;
// gbm_bo_get_fd on a SCANOUT bo is the direct test -- a create_dumb fallback bo (no
// dri_image) cannot export an fd. Build: gcc -o gbm-caps-test gbm-caps-test.c -lgbm
// Run with the zink env (GALLIUM_DRIVER=zink, MESA_LOADER_DRIVER_OVERRIDE=zink, the /opt
// LD/LIBGL paths, VK_DRIVER_FILES=stock venus icd).
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdint.h>
#include <gbm.h>

int main(void) {
   int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
   if (fd < 0) { perror("open renderD128"); return 1; }
   struct gbm_device *gbm = gbm_create_device(fd);     // -> LIMINA_IMP cap log
   fprintf(stderr, "GBMTEST: gbm_device=%p backend=%s\n", (void *)gbm,
           gbm ? gbm_device_get_backend_name(gbm) : "(null)");
   if (!gbm) return 2;

   struct gbm_bo *bo = gbm_bo_create(gbm, 1280, 800, GBM_FORMAT_XRGB8888,
                                     GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING);
   fprintf(stderr, "GBMTEST: scanout bo=%p\n", (void *)bo);
   if (bo) {
      int dfd = gbm_bo_get_fd(bo);                     // dumb bo (no image) -> fails
      fprintf(stderr, "GBMTEST: gbm_bo_get_fd=%d  (>=0 = real dri_image w/ export; <0 = dumb buffer / NO export = the crash condition)\n", dfd);
      if (dfd >= 0) close(dfd);
   }
   return 0;
}
