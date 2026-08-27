/* SPDX-License-Identifier: GPL-2.0-only
 *
 * How late does macOS wake a thread that asked for a ~16.7 ms deadline?
 *
 * That is the question underneath limina's WFI park: an idle guest vCPU traps on WFI,
 * we compute the guest's virtual-timer deadline, and a host thread sleeps until it.
 * If the host serves that sleep late, the guest misses its frame flip.
 *
 * This probe measures the lateness (observed wake - requested deadline) for each
 * host wait primitive we could use, under each thread scheduling policy we could set.
 *
 *   clang -O2 -o wakeprobe wakeprobe.c && ./wakeprobe [iterations] [deadline_us]
 */
#include <dispatch/dispatch.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/qos.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static mach_timebase_info_data_t tb;
static uint64_t ns_to_abs(uint64_t ns) { return ns * tb.denom / tb.numer; }
static uint64_t abs_to_ns(uint64_t a) { return a * tb.numer / tb.denom; }

static int cmp_u64(const void *a, const void *b) {
	uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
	return (x > y) - (x < y);
}

/* --- the waits. each sleeps until `deadline` (mach absolute) and returns. --- */

static void wait_nanosleep(uint64_t deadline) {
	uint64_t now = mach_absolute_time();
	if (now >= deadline) return;
	uint64_t ns = abs_to_ns(deadline - now);
	struct timespec ts = { .tv_sec = ns / 1000000000ull, .tv_nsec = ns % 1000000000ull };
	nanosleep(&ts, NULL);
}

/* The shape crossbeam's select! + after() ends up in: a condvar with a relative timeout,
 * on a condvar nobody will ever signal. */
static pthread_mutex_t cv_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static void wait_condvar(uint64_t deadline) {
	uint64_t now = mach_absolute_time();
	if (now >= deadline) return;
	uint64_t ns = abs_to_ns(deadline - now);
	struct timespec rel = { .tv_sec = ns / 1000000000ull, .tv_nsec = ns % 1000000000ull };
	pthread_mutex_lock(&cv_mu);
	pthread_cond_timedwait_relative_np(&cv, &cv_mu, &rel);
	pthread_mutex_unlock(&cv_mu);
}

static void wait_mach(uint64_t deadline) { mach_wait_until(deadline); }

static int kq = -1;
static void kq_wait(uint64_t deadline, uint32_t extra_fflags) {
	struct kevent64_s ev;
	EV_SET64(&ev, 1, EVFILT_TIMER, EV_ADD | EV_ONESHOT,
	         NOTE_MACHTIME | NOTE_ABSOLUTE | extra_fflags, (int64_t)deadline, 0, 0, 0);
	kevent64(kq, &ev, 1, NULL, 0, 0, NULL);
	struct kevent64_s out;
	kevent64(kq, NULL, 0, &out, 1, 0, NULL);
}
static void wait_kqueue(uint64_t d) { kq_wait(d, 0); }
static void wait_kqueue_critical(uint64_t d) { kq_wait(d, NOTE_CRITICAL); }

/* Spin the last `spin_ns` before the deadline, sleep the rest. */
static uint64_t spin_ns = 500000; /* 0.5 ms */
static void wait_hybrid(uint64_t deadline) {
	uint64_t spin_abs = ns_to_abs(spin_ns);
	if (deadline > spin_abs) mach_wait_until(deadline - spin_abs);
	while (mach_absolute_time() < deadline) { /* spin */ }
}

/* --- thread policies --- */

static void policy_default(void) {}

static void policy_qos_ui(void) {
	pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
}

static void policy_latency_tier0(void) {
	struct thread_latency_qos_policy p = { .thread_latency_qos_tier = LATENCY_QOS_TIER_0 };
	thread_policy_set(mach_thread_self(), THREAD_LATENCY_QOS_POLICY,
	                  (thread_policy_t)&p, THREAD_LATENCY_QOS_POLICY_COUNT);
}

/* The CoreAudio band: a 60 Hz period, a slice of work, a tight constraint. */
static void policy_time_constraint(void) {
	struct thread_time_constraint_policy p = {
		.period = (uint32_t)ns_to_abs(16666667ull),
		.computation = (uint32_t)ns_to_abs(2000000ull),
		.constraint = (uint32_t)ns_to_abs(4000000ull),
		.preemptible = 1,
	};
	kern_return_t kr = thread_policy_set(mach_thread_self(), THREAD_TIME_CONSTRAINT_POLICY,
	                                     (thread_policy_t)&p, THREAD_TIME_CONSTRAINT_POLICY_COUNT);
	if (kr != KERN_SUCCESS) fprintf(stderr, "  (time-constraint policy refused: %d)\n", kr);
}

static void report_latency_tier(const char *label) {
	struct thread_latency_qos_policy p = { 0 };
	mach_msg_type_number_t cnt = THREAD_LATENCY_QOS_POLICY_COUNT;
	boolean_t def = FALSE;
	kern_return_t kr = thread_policy_get(mach_thread_self(), THREAD_LATENCY_QOS_POLICY,
	                                     (thread_policy_t)&p, &cnt, &def);
	if (kr == KERN_SUCCESS)
		printf("  [%s] latency QoS tier = 0x%x%s\n", label,
		       (unsigned)p.thread_latency_qos_tier, def ? " (default)" : "");
}

struct wait_kind { const char *name; void (*fn)(uint64_t); };
struct pol_kind  { const char *name; void (*fn)(void); };

int main(int argc, char **argv) {
	int iters = argc > 1 ? atoi(argv[1]) : 300;
	uint64_t deadline_us = argc > 2 ? strtoull(argv[2], NULL, 10) : 16667;

	mach_timebase_info(&tb);
	kq = kqueue();

	struct wait_kind waits[] = {
		{ "nanosleep",          wait_nanosleep },
		{ "cond_timedwait",     wait_condvar },
		{ "mach_wait_until",    wait_mach },
		{ "kqueue timer",       wait_kqueue },
		{ "kqueue NOTE_CRITICAL", wait_kqueue_critical },
		{ "mach + 0.5ms spin",  wait_hybrid },
	};
	struct pol_kind pols[] = {
		{ "default",            policy_default },
		{ "QOS_USER_INTERACTIVE", policy_qos_ui },
		{ "LATENCY_QOS_TIER_0", policy_latency_tier0 },
		{ "TIME_CONSTRAINT",    policy_time_constraint },
	};

	printf("timebase %u/%u (%.4f ns/tick), deadline %llu us, %d iterations per cell\n",
	       tb.numer, tb.denom, (double)tb.numer / tb.denom,
	       (unsigned long long)deadline_us, iters);
	printf("lateness = observed wake - requested deadline, in microseconds\n\n");

	uint64_t *lat = malloc(sizeof(uint64_t) * iters);

	for (size_t p = 0; p < sizeof(pols) / sizeof(pols[0]); p++) {
		fflush(stdout);
		/* Each policy gets a fresh process-wide thread state; policies are additive
		 * on a thread, so run each in its own child to keep the cells independent. */
		pid_t pid = fork();
		if (pid > 0) { int st; waitpid(pid, &st, 0); continue; }

		kq = kqueue();
		pols[p].fn();
		printf("=== policy: %s\n", pols[p].name);
		report_latency_tier(pols[p].name);
		printf("  %-22s %8s %8s %8s %8s %8s\n", "wait", "p50", "p90", "p99", "max", "mean");

		for (size_t w = 0; w < sizeof(waits) / sizeof(waits[0]); w++) {
			double sum = 0;
			for (int i = 0; i < iters; i++) {
				uint64_t deadline = mach_absolute_time() + ns_to_abs(deadline_us * 1000);
				waits[w].fn(deadline);
				uint64_t now = mach_absolute_time();
				uint64_t late = now > deadline ? abs_to_ns(now - deadline) / 1000 : 0;
				lat[i] = late;
				sum += (double)late;
			}
			qsort(lat, iters, sizeof(uint64_t), cmp_u64);
			printf("  %-22s %8llu %8llu %8llu %8llu %8.1f\n", waits[w].name,
			       (unsigned long long)lat[iters / 2],
			       (unsigned long long)lat[(int)(iters * 0.90)],
			       (unsigned long long)lat[(int)(iters * 0.99)],
			       (unsigned long long)lat[iters - 1], sum / iters);
		}
		printf("\n");
		fflush(stdout);
		_exit(0);
	}
	return 0;
}
