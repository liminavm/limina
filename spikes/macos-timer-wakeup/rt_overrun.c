/* SPDX-License-Identifier: GPL-2.0-only
 *
 * What happens to a real-time thread that overruns its computation slice?
 *
 * xnu's fail-safe (osfmk/kern/priority.c, thread_quantum_expire) demotes a TH_MODE_REALTIME
 * thread to plain timeshare once its accumulated computation since it last *blocked* exceeds
 * max_unsafe_rt_computation, and holds it there until safe_release. On a release kernel that
 * reads as 100 quanta x 10 ms = 1 s of running to trip it, and 2 x that = 2 s demoted.
 *
 * A vCPU thread runs guest code without blocking for as long as the guest wants, so this is the
 * question that decides whether limina can put vCPU threads in the band. Read the source, then
 * watch it happen: band the thread, measure its wake latency, burn CPU without blocking for
 * longer than the limit, and measure again while it recovers.
 *
 * With a heartbeat interval, the burn loop blocks briefly every so often. thread_unblock()
 * zeroes computation_metered, so a thread that really enters TH_WAIT often enough can never
 * reach the limit — which is the cheapest way to keep a vCPU thread in the band safely.
 *
 *   clang -O2 -o rt_overrun rt_overrun.c && ./rt_overrun [burn_ms] [heartbeat_ms]
 */
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static mach_timebase_info_data_t tb;
static uint64_t ns_to_abs(uint64_t ns) { return ns * tb.denom / tb.numer; }
static uint64_t abs_to_ns(uint64_t a) { return a * tb.numer / tb.denom; }

static void band(void) {
	struct thread_time_constraint_policy p = {
		.period = (uint32_t)ns_to_abs(16666667ull),
		.computation = (uint32_t)ns_to_abs(1000000ull),
		.constraint = (uint32_t)ns_to_abs(2000000ull),
		.preemptible = 1,
	};
	kern_return_t kr = thread_policy_set(mach_thread_self(), THREAD_TIME_CONSTRAINT_POLICY,
	                                     (thread_policy_t)&p, THREAD_TIME_CONSTRAINT_POLICY_COUNT);
	printf("thread_policy_set -> %d (%s)\n", kr, kr == KERN_SUCCESS ? "in the band" : "REFUSED");
}

/* One 16.667 ms deadline; returns lateness in microseconds. */
static uint64_t one_wait(void) {
	uint64_t deadline = mach_absolute_time() + ns_to_abs(16666667ull);
	mach_wait_until(deadline);
	uint64_t now = mach_absolute_time();
	return now > deadline ? abs_to_ns(now - deadline) / 1000 : 0;
}

/* Report the mean lateness of `n` waits, and the wall-clock at which the window ended. */
static void window(const char *tag, int n, uint64_t t0) {
	uint64_t sum = 0, max = 0;
	for (int i = 0; i < n; i++) {
		uint64_t l = one_wait();
		sum += l;
		if (l > max) max = l;
	}
	double at = (double)abs_to_ns(mach_absolute_time() - t0) / 1e9;
	printf("  [t+%5.2fs] %-22s mean %7llu us   max %7llu us\n", at, tag,
	       (unsigned long long)(sum / n), (unsigned long long)max);
	fflush(stdout);
}

int main(int argc, char **argv) {
	uint64_t burn_ms = argc > 1 ? strtoull(argv[1], NULL, 10) : 1500;
	/* 0 = never block during the burn (the failing shape). */
	uint64_t hb_ms = argc > 2 ? strtoull(argv[2], NULL, 10) : 0;
	mach_timebase_info(&tb);
	band();

	uint64_t t0 = mach_absolute_time();
	printf("before the overrun:\n");
	for (int i = 0; i < 3; i++) window("banded", 30, t0);

	if (hb_ms)
		printf("burning %llu ms, blocking briefly every %llu ms...\n",
		       (unsigned long long)burn_ms, (unsigned long long)hb_ms);
	else
		printf("burning %llu ms of uninterrupted computation...\n", (unsigned long long)burn_ms);

	uint64_t until = mach_absolute_time() + ns_to_abs(burn_ms * 1000000ull);
	uint64_t next_hb = hb_ms ? mach_absolute_time() + ns_to_abs(hb_ms * 1000000ull) : 0;
	uint64_t beats = 0;
	volatile uint64_t sink = 0;
	while (mach_absolute_time() < until) {
		sink += 1;
		if (next_hb && mach_absolute_time() >= next_hb) {
			/* Far enough out that the thread genuinely parks: a deadline already in the
			 * past returns without ever entering TH_WAIT, and then nothing is reset. */
			mach_wait_until(mach_absolute_time() + ns_to_abs(100000ull)); /* 100 us */
			next_hb = mach_absolute_time() + ns_to_abs(hb_ms * 1000000ull);
			beats++;
		}
	}
	(void)sink;
	if (hb_ms) printf("  (%llu heartbeat blocks)\n", (unsigned long long)beats);

	printf("after the overrun (each row is 30 deadlines, ~0.5 s):\n");
	for (int i = 0; i < 12; i++) window("post-overrun", 30, t0);
	return 0;
}
