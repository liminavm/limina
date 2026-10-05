// Cross-CPU thread wakeup latency: thread A (cpu a) writes to a pipe after an
// idle gap, thread B (cpu b) blocked in read() records the delay until it runs.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static int fds[2];
static int cpu_b;

static uint64_t now_ns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec;
}

static void pin(int cpu) {
  cpu_set_t s;
  CPU_ZERO(&s);
  CPU_SET(cpu, &s);
  pthread_setaffinity_np(pthread_self(), sizeof(s), &s);
}

static void* reader(void* arg) {
  (void)arg;
  pin(cpu_b);
  uint64_t sent;
  while (read(fds[0], &sent, sizeof(sent)) == sizeof(sent)) {
    if (sent == 0) break;
    printf("%llu\n", (unsigned long long)(now_ns() - sent));
  }
  return NULL;
}

int main(int argc, char** argv) {
  int a = argc > 1 ? atoi(argv[1]) : 0;
  cpu_b = argc > 2 ? atoi(argv[2]) : 1;
  int n = argc > 3 ? atoi(argv[3]) : 500;
  int gap_us = argc > 4 ? atoi(argv[4]) : 5000;
  if (pipe(fds)) return 1;
  pthread_t t;
  pthread_create(&t, NULL, reader, NULL);
  pin(a);
  for (int i = 0; i < n; i++) {
    usleep(gap_us);
    uint64_t s = now_ns();
    if (write(fds[1], &s, sizeof(s)) != sizeof(s)) return 1;
  }
  uint64_t z = 0;
  if (write(fds[1], &z, sizeof(z)) != sizeof(z)) return 1;
  pthread_join(t, NULL);
  return 0;
}
