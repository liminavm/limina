// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* procwake — sample a process's wakeup counters via proc_pid_rusage (no root for
 * same-uid targets). Distinguishes the TWO macOS wakeup metrics:
 *   ri_pkg_idle_wkups  — wakeups that took the package out of idle (top's IDLEW)
 *   ri_interrupt_wkups — ALL interrupt wakeups (much larger under load)
 * Activity Monitor's "Idle Wake Ups" column is one of these, windowed; this probe
 * prints both cumulatives and their per-interval deltas so we can match AM's display.
 *
 * Usage: procwake <pid> [interval_s] [count]
 * Build: cc -O2 -o procwake procwake.c
 */
#include <libproc.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <pid> [interval_s] [count]\n", argv[0]);
        return 1;
    }
    pid_t pid = (pid_t)atoi(argv[1]);
    int interval = argc > 2 ? atoi(argv[2]) : 5;
    int count = argc > 3 ? atoi(argv[3]) : 5;

    struct rusage_info_v4 prev = {0}, cur = {0};
    int have_prev = 0;
    printf("%-8s %14s %14s %12s %12s\n", "sample", "pkg_idle(cum)", "intr(cum)",
           "pkg_idle/s", "intr/s");
    for (int i = 0; i < count; i++) {
        if (proc_pid_rusage(pid, RUSAGE_INFO_V4, (rusage_info_t *)&cur) != 0) {
            perror("proc_pid_rusage");
            return 1;
        }
        if (have_prev) {
            printf("%-8d %14llu %14llu %12.1f %12.1f\n", i, cur.ri_pkg_idle_wkups,
                   cur.ri_interrupt_wkups,
                   (double)(cur.ri_pkg_idle_wkups - prev.ri_pkg_idle_wkups) / interval,
                   (double)(cur.ri_interrupt_wkups - prev.ri_interrupt_wkups) / interval);
        } else {
            printf("%-8d %14llu %14llu %12s %12s\n", i, cur.ri_pkg_idle_wkups,
                   cur.ri_interrupt_wkups, "-", "-");
        }
        prev = cur;
        have_prev = 1;
        if (i + 1 < count)
            sleep(interval);
    }
    return 0;
}
