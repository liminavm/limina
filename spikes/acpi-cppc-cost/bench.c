// bench <secs> — one worker pinned to each online CPU runs a cache-resident integer loop for
// <secs> seconds; prints per-CPU and total iterations (millions). Throughput, not latency.
#define _GNU_SOURCE
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }

int main(int argc, char **argv)
{
	double secs = argc > 1 ? atof(argv[1]) : 10;
	int n = sysconf(_SC_NPROCESSORS_ONLN);
	volatile uint64_t *res = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	for (int c = 0; c < n; c++) {
		if (fork() == 0) {
			cpu_set_t s; CPU_ZERO(&s); CPU_SET(c, &s); sched_setaffinity(0, sizeof s, &s);
			uint64_t x = 88172645463325252ull + c, it = 0;
			double end = now() + secs;
			for (;;) {
				for (int i = 0; i < 100000; i++) { x ^= x << 13; x ^= x >> 7; x ^= x << 17; }
				it++;
				if (now() >= end) break;
			}
			res[c] = it; res[64 + c] = x;
			_exit(0);
		}
	}
	while (wait(NULL) > 0) ;
	uint64_t tot = 0;
	for (int c = 0; c < n; c++) { printf("cpu%d %.3f ", c, res[c] / 10.0); tot += res[c]; }
	printf("total_Miter %.3f\n", tot / 10.0);
	return 0;
}
