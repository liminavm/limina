// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* cmwin — minimal Wayland/xdg-shell window shared by both crossmark backends'
 * -present mode. Compiled only where wayland-client exists (HAVE_WAYLAND). */

#ifndef CMWIN_H
#define CMWIN_H

#ifdef HAVE_WAYLAND

#include <wayland-client.h>

struct cm_win {
   struct wl_display *dpy;
   struct wl_compositor *compositor;
   struct xdg_wm_base *wm_base;
   struct wl_surface *surface;
   struct xdg_surface *xdg_surface;
   struct xdg_toplevel *toplevel;
   int width, height; /* current size (from configure; default if 0x0) */
   int configured;
   int closed;
};

/* Connect, create a mapped xdg toplevel, and block until the first configure.
 * width/height are the requested size (compositor may override, esp. -F). */
struct cm_win *cm_win_create(int width, int height, int fullscreen,
                             const char *title);
/* Dispatch pending events without blocking (call once per frame). */
void cm_win_pump(struct cm_win *w);
void cm_win_destroy(struct cm_win *w);

#endif /* HAVE_WAYLAND */

#endif
