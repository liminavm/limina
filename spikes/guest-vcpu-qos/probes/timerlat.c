// Timer wakeup overshoot: clock_nanosleep(TIMER_ABSTIME) every period, record lateness.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

int main(int argc, char** argv) {
  long period_us = argc > 1 ? atol(argv[1]) : 16667;
  int n = argc > 2 ? atoi(argv[2]) : 600;
  struct timespec next, now;
  clock_gettime(CLOCK_MONOTONIC, &next);
  for (int i = 0; i < n; i++) {
    next.tv_nsec += period_us * 1000;
    while (next.tv_nsec >= 1000000000) {
      next.tv_nsec -= 1000000000;
      next.tv_sec++;
    }
    clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &next, NULL);
    clock_gettime(CLOCK_MONOTONIC, &now);
    printf("%ld\n", (now.tv_sec - next.tv_sec) * 1000000000L + (now.tv_nsec - next.tv_nsec));
  }
  return 0;
}
