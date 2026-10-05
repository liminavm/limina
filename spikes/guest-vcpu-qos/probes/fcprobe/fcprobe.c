// Standalone Wayland frame pacing probe, independent of Firefox.
//
// Opens an xdg_toplevel with wl_shm buffers and redraws it on every frame
// callback, optionally spending --work ms before committing (to mimic a
// renderer). Logs the interval between frame callbacks and, via
// wp_presentation, between presented frames plus commit->present latency and
// discarded frames.
//
//   fcprobe [--seconds 10] [--work 4] [--size 1920x1080] [--fullscreen] [--raw]

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>

#include "presentation-time-client-protocol.h"
#include "xdg-shell-client-protocol.h"

#define MAX_SAMPLES 100000

struct buffer {
  struct wl_buffer* wl;
  uint32_t* data;
  bool busy;
};

static struct wl_compositor* compositor;
static struct wl_shm* shm;
static struct xdg_wm_base* wm_base;
static struct wp_presentation* presentation;
static clockid_t pres_clock = CLOCK_MONOTONIC;
static struct wl_surface* surface;
static struct xdg_surface* xsurface;
static struct xdg_toplevel* toplevel;
static struct buffer buffers[3];
static int width = 1920, height = 1080;
static bool configured, running = true;
static double work_ms;
static uint64_t frame_no;

static double cb_ms[MAX_SAMPLES];
static int n_cb;
static double pres_ms[MAX_SAMPLES];
static double latency_ms[MAX_SAMPLES];
static uint32_t pres_flags[MAX_SAMPLES];
static int n_pres, n_discarded;
static uint64_t refresh_ns;

static double now_ms(clockid_t clk) {
  struct timespec t;
  clock_gettime(clk, &t);
  return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static void buffer_release(void* data, struct wl_buffer* b) {
  ((struct buffer*)data)->busy = false;
}
static const struct wl_buffer_listener buffer_listener = {buffer_release};

static void create_buffers(void) {
  int stride = width * 4, size = stride * height;
  for (int i = 0; i < 3; i++) {
    int fd = memfd_create("fcprobe", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, size) < 0) {
      perror("memfd");
      exit(1);
    }
    buffers[i].data = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    struct wl_shm_pool* pool = wl_shm_create_pool(shm, fd, size);
    buffers[i].wl = wl_shm_pool_create_buffer(pool, 0, width, height, stride, WL_SHM_FORMAT_XRGB8888);
    wl_buffer_add_listener(buffers[i].wl, &buffer_listener, &buffers[i]);
    wl_shm_pool_destroy(pool);
    close(fd);
  }
}

struct feedback_data {
  double commit_ms;
};

static void fb_sync_output(void* d, struct wp_presentation_feedback* f, struct wl_output* o) {}
static void fb_presented(void* d, struct wp_presentation_feedback* f, uint32_t tv_sec_hi,
                         uint32_t tv_sec_lo, uint32_t tv_nsec, uint32_t refresh, uint32_t seq_hi,
                         uint32_t seq_lo, uint32_t flags) {
  struct feedback_data* fd = d;
  double t = (((uint64_t)tv_sec_hi << 32) | tv_sec_lo) * 1e3 + tv_nsec / 1e6;
  if (n_pres < MAX_SAMPLES) {
    pres_ms[n_pres] = t;
    latency_ms[n_pres] = t - fd->commit_ms;
    pres_flags[n_pres] = flags;
    n_pres++;
  }
  refresh_ns = refresh;
  free(fd);
  wp_presentation_feedback_destroy(f);
}
static void fb_discarded(void* d, struct wp_presentation_feedback* f) {
  n_discarded++;
  free(d);
  wp_presentation_feedback_destroy(f);
}
static const struct wp_presentation_feedback_listener fb_listener = {fb_sync_output, fb_presented,
                                                                       fb_discarded};

static void redraw(void);

static void frame_done(void* data, struct wl_callback* cb, uint32_t time) {
  wl_callback_destroy(cb);
  if (n_cb < MAX_SAMPLES) cb_ms[n_cb++] = now_ms(CLOCK_MONOTONIC);
  redraw();
}
static const struct wl_callback_listener frame_listener = {frame_done};

static void redraw(void) {
  struct buffer* b = NULL;
  for (int i = 0; i < 3; i++) {
    if (!buffers[i].busy) {
      b = &buffers[i];
      break;
    }
  }
  if (!b) {
    fprintf(stderr, "no free buffer\n");
    exit(1);
  }
  if (work_ms > 0) {
    double end = now_ms(CLOCK_MONOTONIC) + work_ms;
    while (now_ms(CLOCK_MONOTONIC) < end) {
    }
  }
  uint32_t color = 0xff000000 | ((frame_no * 3) & 0xff) << 16 | ((frame_no * 7) & 0xff) << 8;
  for (int i = 0; i < width * height; i += 64) b->data[i] = color;
  frame_no++;

  wl_surface_attach(surface, b->wl, 0, 0);
  wl_surface_damage_buffer(surface, 0, 0, width, height);
  struct wl_callback* cb = wl_surface_frame(surface);
  wl_callback_add_listener(cb, &frame_listener, NULL);
  if (presentation) {
    struct feedback_data* fd = malloc(sizeof(*fd));
    fd->commit_ms = now_ms(pres_clock);
    struct wp_presentation_feedback* f = wp_presentation_feedback(presentation, surface);
    wp_presentation_feedback_add_listener(f, &fb_listener, fd);
  }
  b->busy = true;
  wl_surface_commit(surface);
}

static void pres_clock_id(void* d, struct wp_presentation* p, uint32_t clk) { pres_clock = clk; }
static const struct wp_presentation_listener pres_listener = {pres_clock_id};

static void wm_ping(void* d, struct xdg_wm_base* b, uint32_t serial) { xdg_wm_base_pong(b, serial); }
static const struct xdg_wm_base_listener wm_listener = {wm_ping};

static void xs_configure(void* d, struct xdg_surface* s, uint32_t serial) {
  xdg_surface_ack_configure(s, serial);
  if (!configured) {
    configured = true;
    redraw();
  }
}
static const struct xdg_surface_listener xs_listener = {xs_configure};

static void tl_configure(void* d, struct xdg_toplevel* t, int32_t w, int32_t h, struct wl_array* s) {}
static void tl_close(void* d, struct xdg_toplevel* t) { running = false; }
static void tl_bounds(void* d, struct xdg_toplevel* t, int32_t w, int32_t h) {}
static void tl_caps(void* d, struct xdg_toplevel* t, struct wl_array* c) {}
static const struct xdg_toplevel_listener tl_listener = {tl_configure, tl_close, tl_bounds, tl_caps};

static void reg_global(void* d, struct wl_registry* r, uint32_t name, const char* iface, uint32_t v) {
  if (!strcmp(iface, wl_compositor_interface.name))
    compositor = wl_registry_bind(r, name, &wl_compositor_interface, 4);
  else if (!strcmp(iface, wl_shm_interface.name))
    shm = wl_registry_bind(r, name, &wl_shm_interface, 1);
  else if (!strcmp(iface, xdg_wm_base_interface.name))
    wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
  else if (!strcmp(iface, wp_presentation_interface.name))
    presentation = wl_registry_bind(r, name, &wp_presentation_interface, 1);
}
static void reg_remove(void* d, struct wl_registry* r, uint32_t name) {}
static const struct wl_registry_listener reg_listener = {reg_global, reg_remove};

static int cmp_double(const void* a, const void* b) {
  double x = *(const double*)a, y = *(const double*)b;
  return (x > y) - (x < y);
}

static void report(const char* name, double* t, int n, bool raw) {
  if (n < 3) {
    printf("%s: too few samples (%d)\n", name, n);
    return;
  }
  int m = n - 1;
  double* g = malloc(sizeof(double) * m);
  int hist[64] = {0}, over25 = 0;
  for (int i = 0; i < m; i++) {
    g[i] = t[i + 1] - t[i];
    int bucket = (int)g[i];
    hist[bucket < 63 ? bucket : 63]++;
    if (g[i] > 25) over25++;
  }
  if (raw) {
    for (int i = 0; i < m; i++) printf("%s_interval %.3f\n", name, g[i]);
  }
  qsort(g, m, sizeof(double), cmp_double);
  printf("%s: n=%d rate=%.1f/s p50=%.2f p95=%.2f p99=%.2f max=%.2f over25ms=%d\n", name, m,
         m / ((t[n - 1] - t[0]) / 1e3), g[m / 2], g[(int)(m * 0.95)], g[(int)(m * 0.99)], g[m - 1],
         over25);
  printf("%s histogram (ms: count):", name);
  for (int i = 0; i < 64; i++)
    if (hist[i]) printf(" %d:%d", i, hist[i]);
  printf("\n");
  free(g);
}

int main(int argc, char** argv) {
  double seconds = 10;
  bool fullscreen = false, raw = false;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "--seconds") && i + 1 < argc)
      seconds = atof(argv[++i]);
    else if (!strcmp(argv[i], "--work") && i + 1 < argc)
      work_ms = atof(argv[++i]);
    else if (!strcmp(argv[i], "--size") && i + 1 < argc)
      sscanf(argv[++i], "%dx%d", &width, &height);
    else if (!strcmp(argv[i], "--fullscreen"))
      fullscreen = true;
    else if (!strcmp(argv[i], "--raw"))
      raw = true;
  }
  struct wl_display* dpy = wl_display_connect(NULL);
  if (!dpy) {
    fprintf(stderr, "cannot connect to wayland display\n");
    return 1;
  }
  struct wl_registry* reg = wl_display_get_registry(dpy);
  wl_registry_add_listener(reg, &reg_listener, NULL);
  wl_display_roundtrip(dpy);
  if (!compositor || !shm || !wm_base) {
    fprintf(stderr, "missing globals\n");
    return 1;
  }
  if (presentation) wp_presentation_add_listener(presentation, &pres_listener, NULL);
  xdg_wm_base_add_listener(wm_base, &wm_listener, NULL);
  wl_display_roundtrip(dpy);

  create_buffers();
  surface = wl_compositor_create_surface(compositor);
  xsurface = xdg_wm_base_get_xdg_surface(wm_base, surface);
  xdg_surface_add_listener(xsurface, &xs_listener, NULL);
  toplevel = xdg_surface_get_toplevel(xsurface);
  xdg_toplevel_add_listener(toplevel, &tl_listener, NULL);
  xdg_toplevel_set_title(toplevel, "fcprobe");
  if (fullscreen) xdg_toplevel_set_fullscreen(toplevel, NULL);
  wl_surface_commit(surface);

  double start = now_ms(CLOCK_MONOTONIC);
  while (running && wl_display_dispatch(dpy) != -1) {
    if (now_ms(CLOCK_MONOTONIC) - start > seconds * 1e3) break;
  }

  printf("size=%dx%d work=%.1fms presentation=%s refresh=%.3fms discarded=%d\n", width, height,
         work_ms, presentation ? "yes" : "no", refresh_ns / 1e6, n_discarded);
  report("frame_callback", cb_ms, n_cb, raw);
  report("presented", pres_ms, n_pres, raw);
  if (n_pres > 2) {
    qsort(latency_ms, n_pres, sizeof(double), cmp_double);
    printf("commit_to_present: p50=%.2f p95=%.2f p99=%.2f max=%.2f\n", latency_ms[n_pres / 2],
           latency_ms[(int)(n_pres * 0.95)], latency_ms[(int)(n_pres * 0.99)],
           latency_ms[n_pres - 1]);
    int vsync = 0, hw_clock = 0, hw_completion = 0, zero_copy = 0;
    for (int i = 0; i < n_pres; i++) {
      vsync += !!(pres_flags[i] & WP_PRESENTATION_FEEDBACK_KIND_VSYNC);
      hw_clock += !!(pres_flags[i] & WP_PRESENTATION_FEEDBACK_KIND_HW_CLOCK);
      hw_completion += !!(pres_flags[i] & WP_PRESENTATION_FEEDBACK_KIND_HW_COMPLETION);
      zero_copy += !!(pres_flags[i] & WP_PRESENTATION_FEEDBACK_KIND_ZERO_COPY);
    }
    printf("presentation flags over %d frames: vsync=%d hw_clock=%d hw_completion=%d zero_copy=%d\n",
           n_pres, vsync, hw_clock, hw_completion, zero_copy);
  }
  wl_display_disconnect(dpy);
  return 0;
}
