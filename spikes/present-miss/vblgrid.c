// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* vblgrid — observe the guest's emulated vblank grid (present-miss spike).
 *
 * Kernel 7.1 virtio-gpu delivers flip events from a drm_vblank_helper hrtimer
 * (framedur_ns period, started RELATIVE to vblank-enable, re-armed with
 * hrtimer_forward_now — so late fires shift the phase forward permanently, and
 * a >vblankoffdelay idle gap restarts it at an arbitrary new phase).
 *
 * This probe rides drmWaitVBlank to sample that grid from any client (no DRM
 * master needed): each wait returns the timestamp the timer stamped. Output is
 * CSV: seq, vblank tv (us), CLOCK_MONOTONIC at return (us).
 *
 * Modes:
 *   vblgrid [card] cont N        — N back-to-back waits (grid interval + phase)
 *   vblgrid [card] gap  N SLEEP  — N waits, sleeping SLEEP seconds between
 *                                  each (cross the 5 s vblankoffdelay to watch
 *                                  the phase re-anchor)
 *
 * Build: gcc -O2 -o vblgrid vblgrid.c -ldrm -I/usr/include/libdrm
 */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>

static int64_t now_us(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (int64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

int main(int argc, char **argv)
{
	const char *card = "/dev/dri/card0";
	int argi = 1;

	if (argi < argc && strncmp(argv[argi], "/dev/", 5) == 0)
		card = argv[argi++];
	const char *mode = argi < argc ? argv[argi++] : "cont";
	int n = argi < argc ? atoi(argv[argi++]) : 600;
	double gap_s = argi < argc ? atof(argv[argi++]) : 6.0;

	int fd = open(card, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		perror(card);
		return 1;
	}

	printf("# mode=%s n=%d gap=%.1fs card=%s\n", mode, n, gap_s, card);
	printf("seq,vbl_us,ret_us\n");
	for (int i = 0; i < n; i++) {
		drmVBlank vbl;
		memset(&vbl, 0, sizeof(vbl));
		vbl.request.type = DRM_VBLANK_RELATIVE;
		vbl.request.sequence = 1;
		int ret = drmWaitVBlank(fd, &vbl);
		if (ret) {
			fprintf(stderr, "drmWaitVBlank: %s\n", strerror(errno));
			return 1;
		}
		int64_t tv = (int64_t)vbl.reply.tval_sec * 1000000 + vbl.reply.tval_usec;
		printf("%u,%" PRId64 ",%" PRId64 "\n", vbl.reply.sequence, tv, now_us());
		if (strcmp(mode, "gap") == 0) {
			fflush(stdout);
			usleep((useconds_t)(gap_s * 1e6));
		}
	}
	return 0;
}
