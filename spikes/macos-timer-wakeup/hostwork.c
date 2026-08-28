/* SPDX-License-Identifier: GPL-2.0-only
 *
 * How much work can the HOST still get done while a VM holds real-time reservations?
 *
 * The companion to wakeprobe: that one measures latency, this one measures throughput, and a
 * reservation starves the two differently. A banded vCPU cannot be preempted, so ordinary host
 * work does not lose its *punctuality* to it — it loses its *share of the machine*, which no
 * deadline measurement reports.
 *
 * Fixed work, measured in wall-clock: each thread runs the same iteration count, so a slower
 * machine takes longer rather than doing less. Ordinary priority and ordinary QoS on purpose —
 * this is the user's editor, not something that knows how to ask for anything.
 *
 *   clang -O2 -o hostwork hostwork.c && ./hostwork [threads] [megaiters]
 */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static uint64_t iters;

/* An integer mix the optimiser cannot fold away and that stays in registers, so this measures
 * CPU share and not the memory system. */
static void *work(void *arg) {
	uint64_t x = (uint64_t)(uintptr_t)arg | 1;
	for (uint64_t i = 0; i < iters; i++) {
		x ^= x << 13;
		x ^= x >> 7;
		x ^= x << 17;
		x += 0x9E3779B97F4A7C15ULL;
	}
	return (void *)(uintptr_t)x;
}

static double now_s(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
	int nthreads = argc > 1 ? atoi(argv[1]) : 8;
	uint64_t mega = argc > 2 ? strtoull(argv[2], NULL, 10) : 400;
	iters = mega * 1000000ULL;

	pthread_t *t = malloc(sizeof(pthread_t) * nthreads);
	double t0 = now_s();
	for (int i = 0; i < nthreads; i++)
		pthread_create(&t[i], NULL, work, (void *)(uintptr_t)(i + 1));
	for (int i = 0; i < nthreads; i++)
		pthread_join(t[i], NULL);
	double el = now_s() - t0;

	/* Aggregate rate is the quantity that degrades under contention; per-thread wall time is what
	 * the person waiting on one job actually feels. */
	printf("threads=%d work=%lluM elapsed=%.3fs rate=%.1f Miter/s\n",
	       nthreads, (unsigned long long)mega, el,
	       (double)nthreads * mega / el);
	return 0;
}
