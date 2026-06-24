#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Instrument zink's dma-buf scanout import path to diagnose the F44 (mutter-50)
# gnome-shell SIGSEGV: zink_resource_from_handle returns NULL (null scanout texture
# -> dri2_allocate_textures deref). The failure is SILENT in the journal, so the
# prime suspect is negotiate_image_config() returning success=false (the one
# create_image() fail-path with no mesa_loge). This adds LIMINA_IMP: fprintf logs at:
#   1. zink_resource_from_handle ENTRY  (modifier/format/dims the compositor hands us)
#   2. the resource_create()==NULL early return
#   3. create_image() right after negotiate_image_config (success + negotiated config)
# Run INSIDE the container against /build/mesa (after patches/mesa 0001-0006; none of
# them touch zink_resource.c, so the anchors below are stable). Idempotent-ish: aborts
# if a marker is already present.
import sys, io

f = "/build/mesa/src/gallium/drivers/zink/zink_resource.c"
src = io.open(f, encoding="utf-8").read()
if "LIMINA_IMP" in src:
    print("already instrumented", file=sys.stderr); sys.exit(0)

edits = [
 # 0. crash-durable log macro (stderr is a buffered pipe under gdm -> lost on SIGSEGV).
 #    fopen(append)+fclose per call flushes to disk immediately. Anchor: the result enum,
 #    which precedes both create_image() and zink_resource_from_handle().
 ("""enum resource_object_create_result {""",
  """/* Use mesa_loge: the ONLY log channel proven to escape the sandboxed gdm gnome-shell
 * (it's how zink's custom_border_color warning reaches the journal). stderr is a buffered
 * pipe lost on SIGSEGV; a file hits PrivateTmp; syslog() is namespace-blackholed for uid 60581. */
#define LIMINA_LOG(...) mesa_loge(__VA_ARGS__)

enum resource_object_create_result {"""),

 # 1. entry log (anchor: the unique modifier guard at top of zink_resource_from_handle)
 ("""   struct zink_screen *screen = zink_screen(pscreen);

   if (whandle->modifier != DRM_FORMAT_MOD_INVALID &&
       !screen->info.have_EXT_image_drm_format_modifier)
      return NULL;""",
  """   struct zink_screen *screen = zink_screen(pscreen);

   LIMINA_LOG("LIMINA_IMP: from_handle ENTRY modifier=0x%llx whandle_fmt=0x%llx templ_fmt=%d target=%d w=%u h=%u have_mod_ext=%d can_invalid_linear=%d\\n",
           (unsigned long long)whandle->modifier, (unsigned long long)whandle->format, (int)templ->format, (int)templ->target,
           (unsigned)templ->width0, (unsigned)templ->height0, screen->info.have_EXT_image_drm_format_modifier ? 1 : 0,
           screen->driver_workarounds.can_do_invalid_linear_modifier ? 1 : 0);

   if (whandle->modifier != DRM_FORMAT_MOD_INVALID &&
       !screen->info.have_EXT_image_drm_format_modifier) {
      LIMINA_LOG("LIMINA_IMP: REJECT no have_EXT_image_drm_format_modifier (modifier=0x%llx)\\n", (unsigned long long)whandle->modifier);
      return NULL;
   }"""),

 # 2. resource_create() == NULL early return
 ("""   struct pipe_resource *pres = resource_create(pscreen, &templ2, whandle, usage, &modifier, modifier_count, NULL, NULL);
   if (!pres)
      return NULL;""",
  """   struct pipe_resource *pres = resource_create(pscreen, &templ2, whandle, usage, &modifier, modifier_count, NULL, NULL);
   if (!pres) {
      LIMINA_LOG("LIMINA_IMP: resource_create returned NULL (modifier=0x%llx fmt=%d)\\n", (unsigned long long)modifier, (int)templ2.format);
      return NULL;
   }"""),

 # 3. create_image negotiate result (anchor on the unique post-negotiate srgb-dmabuf block)
 ("""   if (!success)
      return roc_fail_and_free_object;

   if (ici.tiling == VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT && srgb &&""",
  """   if ((templ->bind & ZINK_BIND_DMABUF) || alloc_info->whandle)
      LIMINA_LOG("LIMINA_IMP: create_image bind=0x%x negotiate success=%d mod=0x%llx ici.format=%d ici.tiling=%d num_planes=%u export_types=0x%x whandle_mod=0x%llx winsys_modifier=%d\\n",
              (unsigned)templ->bind, (int)success, (unsigned long long)mod, (int)ici.format, (int)ici.tiling, (unsigned)num_planes,
              (unsigned)alloc_info->export_types, alloc_info->whandle ? (unsigned long long)alloc_info->whandle->modifier : 0xffffffffffffffffULL, (int)winsys_modifier);
   if (!success)
      return roc_fail_and_free_object;

   if (ici.tiling == VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT && srgb &&"""),
]

for old, new in edits:
    if old not in src:
        print("ANCHOR NOT FOUND:\n" + old[:200], file=sys.stderr); sys.exit(2)
    src = src.replace(old, new, 1)

io.open(f, "w", encoding="utf-8").write(src)
print("instrumented zink_resource.c with 3 LIMINA_IMP probes")
