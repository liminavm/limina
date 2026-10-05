// Waker-side cost of a futex wake: thread A (cpu a) times FUTEX_WAKE of thread
// B (cpu b) blocked in FUTEX_WAIT, after an idle gap so cpu b is idle. Also
// prints a getppid() timed just before it, as the cost of a null syscall.
#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static _Atomic uint32_t word;
static _Atomic int done;
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

static void* waiter(void* arg) {
  (void)arg;
  pin(cpu_b);
  while (!atomic_load(&done)) {
    while (atomic_load(&word) == 0 && !atomic_load(&done)) {
      syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    }
    atomic_store(&word, 0);
  }
  return NULL;
}

int main(int argc, char** argv) {
  int a = argc > 1 ? atoi(argv[1]) : 0;
  cpu_b = argc > 2 ? atoi(argv[2]) : 1;
  long gap_us = argc > 3 ? atol(argv[3]) : 5000;
  int n = argc > 4 ? atoi(argv[4]) : 500;
  pthread_t t;
  pthread_create(&t, NULL, waiter, NULL);
  pin(a);
  struct timespec gap = {gap_us / 1000000, (gap_us % 1000000) * 1000};
  for (int i = 0; i < n; i++) {
    nanosleep(&gap, NULL);
    while (atomic_load(&word) != 0) {
      sched_yield();
    }
    atomic_store(&word, 1);
    uint64_t n0 = now_ns();
    syscall(SYS_getppid);
    uint64_t t0 = now_ns();
    long woken = syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    uint64_t t1 = now_ns();
    if (woken == 1) {
      printf("%llu %llu\n", (unsigned long long)(t1 - t0), (unsigned long long)(t0 - n0));
    }
  }
  atomic_store(&done, 1);
  atomic_store(&word, 1);
  syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
  pthread_join(t, NULL);
  return 0;
}
