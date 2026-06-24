#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Instrument + guard dri2_allocate_textures (src/gallium/frontends/dri/dri2.c), the
# actual SIGSEGV site: `texture = images.<front|back>->texture; drawable->w = texture->width0`
# derefs a NULL texture. images.* comes straight from the compositor's loader getBuffers
# callback (mutter), NOT zink resource_from_handle (proven: that probe never fires). So
# this logs the image_mask + which image's ->texture is NULL, and GUARDS the deref (skip
# the block) so we can see whether gnome-shell survives degraded -- doubling as the
# graceful-degrade fix candidate. Logs via syslog (journald; survives the crash).
import sys, io
f = "/build/mesa/src/gallium/frontends/dri/dri2.c"
src = io.open(f, encoding="utf-8").read()
if "LIMINA_IMP" in src:
    print("dri2 already instrumented", file=sys.stderr); sys.exit(0)

edits = [
 # macro, before the function
 ("""dri2_allocate_textures(struct dri_context *ctx,
                       struct dri_drawable *drawable,""",
  """#include "util/log.h"
#define LIMINA_LOG(...) mesa_loge(__VA_ARGS__)
dri2_allocate_textures(struct dri_context *ctx,
                       struct dri_drawable *drawable,"""),

 # FRONT block: log + guard
 ("""      struct pipe_resource *texture = images.front->texture;

      drawable->w = texture->width0;
      drawable->h = texture->height0;

      pipe_resource_reference(buf, texture);
      dri_image_fence_sync(ctx, images.front);
   }""",
  """      struct pipe_resource *texture = images.front->texture;
      LIMINA_LOG("LIMINA_IMP: dri2_alloc FRONT mask=0x%x front=%p front->texture=%p\\n", (unsigned)images.image_mask, (void*)images.front, (void*)texture);
      if (texture) {
      drawable->w = texture->width0;
      drawable->h = texture->height0;

      pipe_resource_reference(buf, texture);
      dri_image_fence_sync(ctx, images.front);
      } else { LIMINA_LOG("LIMINA_IMP: FRONT texture NULL -> GUARD skip\\n"); }
   }"""),

 # BACK block (followed by the SHARED if): log + guard
 ("""      struct pipe_resource *texture = images.back->texture;

      drawable->w = texture->width0;
      drawable->h = texture->height0;

      pipe_resource_reference(buf, texture);
      dri_image_fence_sync(ctx, images.back);
   }

   if (images.image_mask & __DRI_IMAGE_BUFFER_SHARED) {""",
  """      struct pipe_resource *texture = images.back->texture;
      LIMINA_LOG("LIMINA_IMP: dri2_alloc BACK mask=0x%x back=%p back->texture=%p\\n", (unsigned)images.image_mask, (void*)images.back, (void*)texture);
      if (texture) {
      drawable->w = texture->width0;
      drawable->h = texture->height0;

      pipe_resource_reference(buf, texture);
      dri_image_fence_sync(ctx, images.back);
      } else { LIMINA_LOG("LIMINA_IMP: BACK texture NULL -> GUARD skip\\n"); }
   }

   if (images.image_mask & __DRI_IMAGE_BUFFER_SHARED) {"""),
]

for old, new in edits:
    if old not in src:
        print("ANCHOR NOT FOUND:\n" + old[:160], file=sys.stderr); sys.exit(2)
    src = src.replace(old, new, 1)
io.open(f, "w", encoding="utf-8").write(src)
print("instrumented dri2.c (log + guard FRONT/BACK)")
