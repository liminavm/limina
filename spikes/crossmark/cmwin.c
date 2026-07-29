// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

#ifdef HAVE_WAYLAND

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cmwin.h"
#include "xdg-shell-client-protocol.h"

static void
wm_base_ping(void *data, struct xdg_wm_base *wm_base, uint32_t serial)
{
   (void)data;
   xdg_wm_base_pong(wm_base, serial);
}

static const struct xdg_wm_base_listener wm_base_listener = { wm_base_ping };

static void
registry_global(void *data, struct wl_registry *reg, uint32_t name,
                const char *iface, uint32_t version)
{
   struct cm_win *w = data;
   (void)version;
   if (!strcmp(iface, wl_compositor_interface.name))
      w->compositor = wl_registry_bind(reg, name, &wl_compositor_interface, 4);
   else if (!strcmp(iface, xdg_wm_base_interface.name)) {
      w->wm_base = wl_registry_bind(reg, name, &xdg_wm_base_interface, 1);
      xdg_wm_base_add_listener(w->wm_base, &wm_base_listener, w);
   }
}

static void
registry_global_remove(void *data, struct wl_registry *reg, uint32_t name)
{
   (void)data;
   (void)reg;
   (void)name;
}

static const struct wl_registry_listener registry_listener = {
   registry_global, registry_global_remove
};

static void
xdg_surface_configure(void *data, struct xdg_surface *surface, uint32_t serial)
{
   struct cm_win *w = data;
   xdg_surface_ack_configure(surface, serial);
   w->configured = 1;
}

static const struct xdg_surface_listener xdg_surface_listener = {
   xdg_surface_configure
};

static void
toplevel_configure(void *data, struct xdg_toplevel *toplevel, int32_t width,
                   int32_t height, struct wl_array *states)
{
   struct cm_win *w = data;
   (void)toplevel;
   (void)states;
   if (width > 0 && height > 0) {
      w->width = width;
      w->height = height;
   }
}

static void
toplevel_close(void *data, struct xdg_toplevel *toplevel)
{
   struct cm_win *w = data;
   (void)toplevel;
   w->closed = 1;
}

static const struct xdg_toplevel_listener toplevel_listener = {
   toplevel_configure, toplevel_close
};

struct cm_win *
cm_win_create(int width, int height, int fullscreen, const char *title)
{
   struct cm_win *w = calloc(1, sizeof(*w));
   w->width = width;
   w->height = height;

   w->dpy = wl_display_connect(NULL);
   if (!w->dpy) {
      fprintf(stderr, "no Wayland display (run inside the seated session; "
                      "WAYLAND_DISPLAY + XDG_RUNTIME_DIR must be set)\n");
      free(w);
      return NULL;
   }
   struct wl_registry *reg = wl_display_get_registry(w->dpy);
   wl_registry_add_listener(reg, &registry_listener, w);
   wl_display_roundtrip(w->dpy);
   if (!w->compositor || !w->wm_base) {
      fprintf(stderr, "compositor lacks wl_compositor/xdg_wm_base\n");
      free(w);
      return NULL;
   }

   w->surface = wl_compositor_create_surface(w->compositor);
   w->xdg_surface = xdg_wm_base_get_xdg_surface(w->wm_base, w->surface);
   xdg_surface_add_listener(w->xdg_surface, &xdg_surface_listener, w);
   w->toplevel = xdg_surface_get_toplevel(w->xdg_surface);
   xdg_toplevel_add_listener(w->toplevel, &toplevel_listener, w);
   xdg_toplevel_set_title(w->toplevel, title);
   xdg_toplevel_set_app_id(w->toplevel, "crossmark");
   if (fullscreen)
      xdg_toplevel_set_fullscreen(w->toplevel, NULL);
   wl_surface_commit(w->surface);
   while (!w->configured && wl_display_dispatch(w->dpy) != -1)
      ;
   return w;
}

void
cm_win_pump(struct cm_win *w)
{
   wl_display_dispatch_pending(w->dpy);
   wl_display_flush(w->dpy);
}

void
cm_win_destroy(struct cm_win *w)
{
   if (w->toplevel)
      xdg_toplevel_destroy(w->toplevel);
   if (w->xdg_surface)
      xdg_surface_destroy(w->xdg_surface);
   if (w->surface)
      wl_surface_destroy(w->surface);
   if (w->dpy)
      wl_display_disconnect(w->dpy);
   free(w);
}

#endif /* HAVE_WAYLAND */
