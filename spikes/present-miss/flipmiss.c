// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* flipmiss — reproduce gnome-shell-rs's "presentation misses" without the
 * compositor (present-miss spike).
 *
 * Mimics a damage-driven compositor's frame clock against virtio-gpu's
 * emulated vblank (kernel 7.1 drm_vblank_helper hrtimer):
 *
 *   target  = the vblank the frame is built for: last flip-event timestamp
 *             rounded up onto the mode's refresh grid
 *   queued  = we sleep until target − headroom, then drmModePageFlip
 *   actual  = the DRM_EVENT_FLIP_COMPLETE timestamp
 *   missed  = round((actual − target) / refresh)   — same math as the
 *             compositor's frame_log
 *
 * Modes:
 *   flipmiss [card] busy N HEADROOM_MS   — flip every cycle (60 fps), N flips
 *   flipmiss [card] idle N HEADROOM_MS GAP_S — one flip every GAP_S seconds
 *                                          (the isolated-flip regime)
 *
 * Output CSV per flip: i, target_us, queued_us, actual_us, headroom_us,
 * missed. Summary on stderr.
 *
 * Needs DRM master: stop the display manager first (systemctl stop gdm).
 * Build: gcc -O2 -o flipmiss flipmiss.c -ldrm -I/usr/include/libdrm
 */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

struct dumb_fb {
	uint32_t handle, pitch, fb_id;
	uint64_t size;
};

static int64_t now_us(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (int64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void sleep_until_us(int64_t t)
{
	struct timespec ts = { .tv_sec = t / 1000000,
			       .tv_nsec = (t % 1000000) * 1000 };
	while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &ts, NULL) == EINTR)
		;
}

static int create_dumb_fb(int fd, drmModeModeInfo *mode, uint32_t fill,
			  struct dumb_fb *out)
{
	struct drm_mode_create_dumb creq = { .width = mode->hdisplay,
					     .height = mode->vdisplay,
					     .bpp = 32 };
	if (drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &creq))
		return -1;
	out->handle = creq.handle;
	out->pitch = creq.pitch;
	out->size = creq.size;
	if (drmModeAddFB(fd, mode->hdisplay, mode->vdisplay, 24, 32, creq.pitch,
			 creq.handle, &out->fb_id))
		return -1;
	struct drm_mode_map_dumb mreq = { .handle = creq.handle };
	if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mreq))
		return -1;
	uint32_t *map = mmap(0, creq.size, PROT_READ | PROT_WRITE, MAP_SHARED,
			     fd, mreq.offset);
	if (map == MAP_FAILED)
		return -1;
	for (uint64_t i = 0; i < creq.size / 4; i++)
		map[i] = fill;
	munmap(map, creq.size);
	return 0;
}

static int64_t g_actual_us; /* set by the flip handler */

static void flip_handler(int fd, unsigned int seq, unsigned int tv_sec,
			 unsigned int tv_usec, void *data)
{
	(void)fd;
	(void)seq;
	(void)data;
	g_actual_us = (int64_t)tv_sec * 1000000 + tv_usec;
}

int main(int argc, char **argv)
{
	const char *card = "/dev/dri/card0";
	int argi = 1;

	if (argi < argc && strncmp(argv[argi], "/dev/", 5) == 0)
		card = argv[argi++];
	const char *rmode = argi < argc ? argv[argi++] : "busy";
	int n = argi < argc ? atoi(argv[argi++]) : 600;
	double headroom_ms = argi < argc ? atof(argv[argi++]) : 5.0;
	double gap_s = argi < argc ? atof(argv[argi++]) : 1.0;
	int idle = strcmp(rmode, "idle") == 0;

	int fd = open(card, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		perror(card);
		return 1;
	}
	if (drmSetMaster(fd)) {
		fprintf(stderr, "drmSetMaster: %s (stop gdm first)\n",
			strerror(errno));
		return 1;
	}

	drmModeRes *res = drmModeGetResources(fd);
	drmModeConnector *conn = NULL;
	for (int i = 0; res && i < res->count_connectors; i++) {
		conn = drmModeGetConnector(fd, res->connectors[i]);
		if (conn && conn->connection == DRM_MODE_CONNECTED &&
		    conn->count_modes)
			break;
		drmModeFreeConnector(conn);
		conn = NULL;
	}
	if (!conn) {
		fprintf(stderr, "no connected connector\n");
		return 1;
	}
	drmModeModeInfo *mode = &conn->modes[0]; /* preferred */
	uint32_t crtc_id = res->crtcs[0];

	/* The grid period exactly as the kernel computes framedur_ns. */
	double framedur_us = (double)mode->htotal * mode->vtotal * 1000.0 /
			     mode->clock;
	fprintf(stderr,
		"# mode %ux%u clock=%u htotal=%u vtotal=%u framedur=%.3fus (%.4f Hz)\n",
		mode->hdisplay, mode->vdisplay, mode->clock, mode->htotal,
		mode->vtotal, framedur_us, 1e6 / framedur_us);

	struct dumb_fb fb[2];
	if (create_dumb_fb(fd, mode, 0xFF204060, &fb[0]) ||
	    create_dumb_fb(fd, mode, 0xFF604020, &fb[1])) {
		fprintf(stderr, "dumb fb: %s\n", strerror(errno));
		return 1;
	}
	if (drmModeSetCrtc(fd, crtc_id, fb[0].fb_id, 0, 0, &conn->connector_id,
			   1, mode)) {
		fprintf(stderr, "SetCrtc: %s\n", strerror(errno));
		return 1;
	}

	drmEventContext ev = { .version = 2, .page_flip_handler = flip_handler };
	int64_t headroom_us = (int64_t)(headroom_ms * 1000);
	int64_t last_actual = now_us();
	int misses = 0, late_queued = 0;

	printf("i,target_us,queued_us,actual_us,headroom_us,missed\n");
	for (int i = 0; i < n; i++) {
		/* Frame clock: next grid point after "now + headroom" on the
		 * grid anchored at the last presentation (what a compositor's
		 * next_presentation_time extrapolates). */
		int64_t t = now_us();
		int64_t k = (int64_t)ceil((double)(t + headroom_us - last_actual) /
					  framedur_us);
		if (k < 1)
			k = 1;
		int64_t target = last_actual + (int64_t)(k * framedur_us);
		sleep_until_us(target - headroom_us);

		int64_t queued = now_us();
		if (drmModePageFlip(fd, crtc_id, fb[(i + 1) & 1].fb_id,
				    DRM_MODE_PAGE_FLIP_EVENT, NULL)) {
			fprintf(stderr, "PageFlip: %s\n", strerror(errno));
			return 1;
		}
		drmHandleEvent(fd, &ev); /* blocks until the flip event */

		int64_t missed = llround((double)(g_actual_us - target) /
					 framedur_us);
		printf("%d,%" PRId64 ",%" PRId64 ",%" PRId64 ",%" PRId64
		       ",%" PRId64 "\n",
		       i, target, queued, g_actual_us, target - queued, missed);
		if (missed > 0)
			misses++;
		if (target - queued < 0)
			late_queued++;
		last_actual = g_actual_us;

		if (idle) {
			fflush(stdout);
			usleep((useconds_t)(gap_s * 1e6));
		}
	}
	fprintf(stderr, "# %d flips, %d missed (%.1f%%), %d queued late\n", n,
		misses, 100.0 * misses / n, late_queued);
	return 0;
}
