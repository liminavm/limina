#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# ROOT-CAUSE probe + candidate fix for the F44/mutter-50 gnome-shell SIGSEGV.
# Crash (proven by coredump): dri2_allocate_textures derefs images.front==NULL because
# device_image_get_buffers (EGLDevice platform) advertises __DRI_IMAGE_BUFFER_FRONT and
# assigns buffers->front = dri2_surf->front UNCONDITIONALLY -- even when device_alloc_image
# (dri_create_image) returned NULL. surfaceless_image_get_buffers guards this ("if (!front)
# return 0"); platform_device does NOT. This:
#   1. Adds the missing guard (the fix) -> no more crash, and since the process survives,
#      mesa_loge now FLUSHES so we can finally read logs.
#   2. Logs device_alloc_image dims/visual + OK/NULL -> reveals WHY dri_create_image fails
#      (0x0 surface? bad format? zink/venus reject?), i.e. the deeper root cause.
import sys, io
f = "/build/mesa/src/egl/drivers/dri2/platform_device.c"
src = io.open(f, encoding="utf-8").read()
if "LIMINA_IMP" in src:
    print("platform_device already instrumented", file=sys.stderr); sys.exit(0)

edits = [
 # ensure mesa_loge is available
 ('#include "util/u_debug.h"',
  '#include "util/u_debug.h"\n#include "util/log.h"'),

 # log device_alloc_image dims + result
 ("""   return dri_create_image(
      dri2_dpy->dri_screen_render_gpu, dri2_surf->base.Width,
      dri2_surf->base.Height, dri2_surf->visual, NULL, 0, 0, NULL);
}""",
  """   struct dri_image *_img = dri_create_image(
      dri2_dpy->dri_screen_render_gpu, dri2_surf->base.Width,
      dri2_surf->base.Height, dri2_surf->visual, NULL, 0, 0, NULL);
   mesa_loge("LIMINA_IMP: device_alloc_image W=%d H=%d visual=%d -> %s",
             dri2_surf->base.Width, dri2_surf->base.Height, (int)dri2_surf->visual, _img ? "OK" : "NULL");
   return _img;
}"""),

 # guard the FRONT path (the fix) + log when alloc fails
 ("""      if (!dri2_surf->front)
         dri2_surf->front = device_alloc_image(dri2_dpy, dri2_surf);

      buffers->image_mask |= __DRI_IMAGE_BUFFER_FRONT;
      buffers->front = dri2_surf->front;
   }""",
  """      if (!dri2_surf->front)
         dri2_surf->front = device_alloc_image(dri2_dpy, dri2_surf);

      if (!dri2_surf->front) {
         mesa_loge("LIMINA_IMP: device_image_get_buffers FRONT alloc NULL -> guard return 0 (was the crash)");
         return 0;
      }

      buffers->image_mask |= __DRI_IMAGE_BUFFER_FRONT;
      buffers->front = dri2_surf->front;
   }"""),
]

for old, new in edits:
    if old not in src:
        print("ANCHOR NOT FOUND:\n" + old[:160], file=sys.stderr); sys.exit(2)
    src = src.replace(old, new, 1)
io.open(f, "w", encoding="utf-8").write(src)
print("instrumented platform_device.c (guard FRONT + log device_alloc_image)")
