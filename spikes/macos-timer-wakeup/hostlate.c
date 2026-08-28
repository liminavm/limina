/* SPDX-License-Identifier: GPL-2.0-only
 *
 * One cell of wakeprobe, measured properly: how late is an ORDINARY host thread on a 16.667 ms
 * deadline, right now?
 *
 * wakeprobe sweeps six waits x four policies and spends ~80 s to produce one sample of each. That
 * is the right shape for "which lever exists" and the wrong shape for "does a VM's scheduling
 * policy move the host", where the quantity is a heavy tail and a single 200-sample run of it says
 * almost nothing — reps of it disagreed by 2 ms to 25 ms within one arm.
 *
 * So: one wait, one policy, many samples, and the counts that matter reported directly. A frame is
 * 16.667 ms; being 8 ms late is half a frame and 16 ms is a whole one.
 *
 *   clang -O2 -o hostlate hostlate.c && ./hostlate [samples] [deadline_us]
 */
#include <mach/mach_time.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static mach_timebase_info_data_t tb;
static uint64_t ns_to_abs(uint64_t ns) { return ns * tb.denom / tb.numer; }
static uint64_t abs_to_ns(uint64_t a) { return a * tb.numer / tb.denom; }

static int cmp_u64(const void *a, const void *b) {
	uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
	return (x > y) - (x < y);
}

int main(int argc, char **argv) {
	int n = argc > 1 ? atoi(argv[1]) : 600;
	uint64_t deadline_us = argc > 2 ? strtoull(argv[2], NULL, 10) : 16667;
	mach_timebase_info(&tb);

	uint64_t *late = malloc(sizeof(uint64_t) * n);
	for (int i = 0; i < n; i++) {
		uint64_t deadline = mach_absolute_time() + ns_to_abs(deadline_us * 1000);
		mach_wait_until(deadline);
		uint64_t now = mach_absolute_time();
		late[i] = now > deadline ? abs_to_ns(now - deadline) / 1000 : 0;
	}
	qsort(late, n, sizeof(uint64_t), cmp_u64);

	int over8 = 0, over16 = 0;
	double sum = 0;
	for (int i = 0; i < n; i++) {
		sum += late[i];
		if (late[i] > 8000) over8++;
		if (late[i] > 16000) over16++;
	}
	printf("n=%d p50=%llu p90=%llu p99=%llu max=%llu mean=%.0f over8ms=%d over16ms=%d\n",
	       n, (unsigned long long)late[n / 2], (unsigned long long)late[(int)(n * .9)],
	       (unsigned long long)late[(int)(n * .99)], (unsigned long long)late[n - 1],
	       sum / n, over8, over16);
	return 0;
}
