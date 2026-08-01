// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Does a cnd_timedwait TIMEOUT reach the branch vkr_context_wait_ring_seqno thinks it does?
//
// The venus ring wait (virglrenderer src/venus/vkr_context.c) logs a "STUCK >500ms" diagnostic
// when its cnd_timedwait times out, and treats any OTHER non-success return as a hard failure —
// which poisons the context (ring FATAL). That split is only correct if a timeout returns
// thrd_timeout. Mesa's c11 shim returns thrd_busy. This probe pins which it is, and prints the
// branch the real code would take.
//
// Build + run:  cc -o probe probe.c && ./probe
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <time.h>

// The enum from src/mesa/compat/c11/threads.h, verbatim.
enum { thrd_success = 0, thrd_timeout, thrd_error, thrd_busy, thrd_nomem };

// cnd_timedwait from src/mesa/compat/c11/threads_posix.h, verbatim.
static int cnd_timedwait_shim(pthread_cond_t *cond, pthread_mutex_t *mtx,
                              const struct timespec *abs_time) {
    int rt = pthread_cond_timedwait(cond, mtx, abs_time);
    if (rt == ETIMEDOUT)
        return thrd_busy;
    return (rt == 0) ? thrd_success : thrd_error;
}

static const char *name(int v) {
    switch (v) {
    case thrd_success: return "thrd_success";
    case thrd_timeout: return "thrd_timeout";
    case thrd_error:   return "thrd_error";
    case thrd_busy:    return "thrd_busy";
    default:           return "?";
    }
}

int main(void) {
    pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t c = PTHREAD_COND_INITIALIZER;

    struct timespec ts;
    timespec_get(&ts, TIME_UTC);
    ts.tv_nsec += 100 * 1000 * 1000;   // 100 ms; nobody will ever signal it
    if (ts.tv_nsec >= 1000000000) { ts.tv_sec += 1; ts.tv_nsec -= 1000000000; }

    pthread_mutex_lock(&m);
    const int ret = cnd_timedwait_shim(&c, &m, &ts);
    pthread_mutex_unlock(&m);

    printf("cnd_timedwait on timeout returned %d (%s)\n", ret, name(ret));
    printf("  thrd_timeout = %d, thrd_busy = %d\n", thrd_timeout, thrd_busy);

    // The OLD branch structure of vkr_context_wait_ring_seqno (the bug).
    int old_ok = 1, old_stuck = 0;
    if (ret == thrd_timeout) {
        old_stuck = 1;
    } else if (ret != thrd_success) {
        old_ok = 0;
    }
    printf("\nold classification (tests thrd_timeout only):\n");
    printf("  logs STUCK: %s\n", old_stuck ? "yes" : "NO (dead branch)");
    printf("  ok=%d  =>  %s\n", old_ok,
           old_ok ? "keeps waiting"
                  : "vkr_context_set_fatal(ctx) — CONTEXT POISONED BY A SLOW WAIT");

    // The SHIPPED structure (virglrenderer patch 0058).
    int new_ok = 1, new_stuck = 0;
    if (ret == thrd_busy || ret == thrd_timeout) {
        new_stuck = 1;
    } else if (ret != thrd_success) {
        new_ok = 0;
    }
    printf("shipped classification (tests thrd_busy || thrd_timeout):\n");
    printf("  logs STUCK: %s\n", new_stuck ? "yes" : "no");
    printf("  ok=%d  =>  %s\n", new_ok,
           new_ok ? "keeps waiting (intended)" : "context poisoned");

    // The regression this guards: a timeout must be reported and survived, never
    // mistaken for a failed wait.
    if (!new_stuck || !new_ok) {
        printf("\nFAIL: the shipped classification mishandles a timeout on this platform.\n");
        return 1;
    }
    if (old_ok) {
        printf("\nNOTE: this platform's shim returns thrd_timeout, so the old code was benign"
               " here.\n      The fix is still required for shims that return thrd_busy.\n");
        return 0;
    }
    printf("\nOK: a timeout is reported and survived (the old code would have poisoned"
           " the context).\n");
    return 0;
}
