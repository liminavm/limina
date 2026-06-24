#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# CONFIRM the root cause: log zink's dmabuf caps at gbm DEVICE CREATION (early in mutter
# startup -> the mesa_loge flushes long before any crash, unlike render-time probes).
# has_dmabuf_export = (pscreen->caps.dmabuf & DRM_PRIME_CAP_EXPORT). If it's false on F44
# (venus 26.0.3) and true on dev-enh (venus 25.3.6), the gbm create_dumb fallback -> null
# bo->image -> dri2 null deref chain is confirmed, and the fix target is venus's dmabuf
# EXPORT capability. Run inside the container against /build/mesa.
import sys, io
f = "/build/mesa/src/gbm/backends/dri/gbm_dri.c"
src = io.open(f, encoding="utf-8").read()
if "LIMINA_IMP" in src:
    print("gbm_dri already instrumented", file=sys.stderr); sys.exit(0)

edits = [
 ('#include "mesa_interface.h"',
  '#include "mesa_interface.h"\n#include "util/log.h"'),

 ("""   if (pscreen->caps.dmabuf & DRM_PRIME_CAP_EXPORT)
      dri->has_dmabuf_export = true;
#endif""",
  """   if (pscreen->caps.dmabuf & DRM_PRIME_CAP_EXPORT)
      dri->has_dmabuf_export = true;
   mesa_loge("LIMINA_IMP: gbm_create_device caps.dmabuf=0x%x import=%d export=%d (export=0 -> scanout bo falls to create_dumb -> null bo->image -> crash)",
             (unsigned)pscreen->caps.dmabuf, (int)dri->has_dmabuf_import, (int)dri->has_dmabuf_export);
#endif"""),
]
for old, new in edits:
    if old not in src:
        print("ANCHOR NOT FOUND:\n" + old[:160], file=sys.stderr); sys.exit(2)
    src = src.replace(old, new, 1)
io.open(f, "w", encoding="utf-8").write(src)
print("instrumented gbm_dri.c (log dmabuf caps at device creation)")
