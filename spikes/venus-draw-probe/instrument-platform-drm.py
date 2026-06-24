#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# ROOT-CAUSE probe + candidate guard for the F44/mutter-50 gnome-shell SIGSEGV, GBM path.
# Confirmed: mutter uses EGL_PLATFORM_GBM (platform_drm), image_mask=BACK. The crash is
# dri2_drm_image_get_buffers doing `buffers->back = bo->image` where the gbm bo exists
# (get_back_bo succeeded) but bo->image (the dri_image) is NULL -> dri2_allocate_textures
# derefs images.back==NULL. bo->image is built during gbm_bo_create_with_modifiers2 ->
# zink resource_create_with_modifiers -> create_image -> venus modifier negotiation.
# This:
#   1. Guards `if (!bo->image) return 0` (the fix) -> no crash -> mesa_loge FLUSHES.
#   2. Logs the gbm surface modifiers/format/dims + whether bo->image is NULL -> shows the
#      exact config mutter-50 asks for that venus-26.0.3 fails to back with an image.
import sys, io
f = "/build/mesa/src/egl/drivers/dri2/platform_drm.c"
src = io.open(f, encoding="utf-8").read()
if "LIMINA_IMP" in src:
    print("platform_drm already instrumented", file=sys.stderr); sys.exit(0)

edits = [
 ('#include "util/os_file.h"',
  '#include "util/os_file.h"\n#include "util/log.h"'),

 ("""   bo = gbm_dri_bo(dri2_surf->back->bo);
   buffers->image_mask = __DRI_IMAGE_BUFFER_BACK;
   buffers->back = bo->image;

   return 1;""",
  """   bo = gbm_dri_bo(dri2_surf->back->bo);
   {
      struct gbm_dri_surface *_s = dri2_surf->gbm_surf;
      mesa_loge("LIMINA_IMP: gbm getBuffers back->bo=%p bo->image=%p W=%u H=%u fmt=0x%x nmod=%u flags=0x%x mod0=0x%llx",
                (void *)dri2_surf->back->bo, (void *)bo->image, _s->base.v0.width, _s->base.v0.height,
                _s->base.v0.format, _s->base.v0.count, _s->base.v0.flags,
                _s->base.v0.count ? (unsigned long long)_s->base.v0.modifiers[0] : 0xffffffffffffffffULL);
   }
   if (!bo->image) {
      mesa_loge("LIMINA_IMP: gbm back bo->image NULL -> guard return 0 (was the crash)");
      return 0;
   }
   buffers->image_mask = __DRI_IMAGE_BUFFER_BACK;
   buffers->back = bo->image;

   return 1;"""),
]

for old, new in edits:
    if old not in src:
        print("ANCHOR NOT FOUND:\n" + old[:160], file=sys.stderr); sys.exit(2)
    src = src.replace(old, new, 1)
io.open(f, "w", encoding="utf-8").write(src)
print("instrumented platform_drm.c (guard back bo->image + log gbm modifiers)")
