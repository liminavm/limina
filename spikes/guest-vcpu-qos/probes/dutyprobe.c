// vCPU speed at a given duty cycle: on cpu 2, sleep gap_us, then time a fixed
// chunk of work repeatedly for busy_us. Prints "elapsed_us:chunk_ns" at
// doubling elapsed times, one line per period.
#define _GNU_SOURCE
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static volatile uint64_t sink;

static uint64_t now_ns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec;
}

int main(int argc, char** argv) {
  long gap_us = argc > 1 ? atol(argv[1]) : 5000;
  int n = argc > 2 ? atoi(argv[2]) : 60;
  long busy_us = argc > 3 ? atol(argv[3]) : 1000;
  cpu_set_t s;
  CPU_ZERO(&s);
  CPU_SET(2, &s);
  sched_setaffinity(0, sizeof(s), &s);
  struct timespec gap = {gap_us / 1000000, (gap_us % 1000000) * 1000};
  for (int i = 0; i < n; i++) {
    nanosleep(&gap, NULL);
    uint64_t start = now_ns(), next = 0;
    for (;;) {
      uint64_t t0 = now_ns(), x = 0;
      for (int m = 0; m < 200; m++) x += m * t0;
      sink = x;
      uint64_t t1 = now_ns();
      uint64_t el = (t1 - start) / 1000;
      if (el >= next) {
        printf("%llu:%llu ", (unsigned long long)el, (unsigned long long)(t1 - t0));
        next = next ? next * 2 : 1;
      }
      if (el > busy_us) break;
    }
    printf("\n");
  }
  return 0;
}
