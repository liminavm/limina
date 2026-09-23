// Game Mode throttle probe: a process tree shaped like limina's supervisor + worker, reporting
// the scheduler priority of its own threads once a second.
//
//   probe parent --secs N [--activity A] [--child-activity B] [--label L]
//       A Regular-policy AppKit app with a visible window (the supervisor's shape) that
//       posix_spawns `probe child` (the worker's shape) and reports its main thread too.
//   probe child --secs N [--activity A] [--label L]
//       Windowless: four default threads that sleep 10 ms at a time (the vCPUs' shape) and one
//       THREAD_TIME_CONSTRAINT_POLICY thread with the band's defaults (60 Hz, 1 ms, 2 ms).
//
// Activities: none | user (NSActivityUserInitiated) | latency (UserInitiated|LatencyCritical).
// Guards (--guard, both roles): none | role (reset our own darwin role when it reads DARWIN_BG)
// | bg (clear an external DARWIN_BG) | both. A guard polls every 50 ms from its own thread.
// Every thread stops itself after --secs, so a starved main thread cannot extend a run.
//
// Output, one line a second per process:
//   <label> <role> t=S prio main=P vcpu=P,P,P,P rt=P | rt late us p50/p99/max | vcpu late us ...
//       | role=R bg=B guard=resets/errors
// A priority is `pth_curpri` from THREAD_EXTENDED_INFO; 4 is MAXPRI_THROTTLE. role is
// getpriority(PRIO_DARWIN_ROLE) (6 = DARWIN_BG), bg is getpriority(PRIO_DARWIN_PROCESS).

#import <AppKit/AppKit.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <pthread.h>
#include <errno.h>
#include <spawn.h>
#include <sys/resource.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

#define NVCPU 4
#define MAXS 4096

// Private in the SDK; values from xnu bsd/sys/resource_private.h.
#define PRIO_DARWIN_ROLE 6
#define PRIO_DARWIN_ROLE_UI_NON_FOCAL 0x4
#define PRIO_DARWIN_ROLE_DARWIN_BG 0x6
#define PRIO_DARWIN_ROLE_USER_INIT 0x7

static double g_secs = 20;
static const char *g_guard = "none";
static int g_guard_role_to = PRIO_DARWIN_ROLE_USER_INIT;
static _Atomic int g_resets, g_errors, g_last_errno;
static const char *g_label = "arm";
static mach_timebase_info_data_t g_tb;
static uint64_t g_end_abs;

static uint64_t ns_to_abs(uint64_t ns) { return ns * g_tb.denom / g_tb.numer; }
static uint64_t abs_to_ns(uint64_t a) { return a * g_tb.numer / g_tb.denom; }

// Lateness samples, reset by the reporter each second. Single writer per ring; the reporter
// reads a racy snapshot, which is fine for a once-a-second percentile.
typedef struct {
    _Atomic uint32_t n;
    uint32_t us[MAXS];
    _Atomic thread_t port;
} ring_t;

static ring_t g_rt, g_vcpu[NVCPU];
static _Atomic thread_t g_main_port;

static void ring_put(ring_t *r, uint32_t us) {
    uint32_t i = atomic_load(&r->n);
    if (i < MAXS) {
        r->us[i] = us;
        atomic_store(&r->n, i + 1);
    }
}

static int cmp_u32(const void *a, const void *b) {
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;
    return x < y ? -1 : x > y;
}

static void ring_stats(ring_t *r, char *out, size_t len) {
    uint32_t n = atomic_load(&r->n);
    static uint32_t tmp[MAXS];
    if (n == 0) {
        snprintf(out, len, "-/-/- (0)");
        return;
    }
    memcpy(tmp, r->us, n * sizeof(uint32_t));
    atomic_store(&r->n, 0);
    qsort(tmp, n, sizeof(uint32_t), cmp_u32);
    snprintf(out, len, "%u/%u/%u (%u)", tmp[n / 2], tmp[(n * 99) / 100], tmp[n - 1], n);
}

static int curpri(thread_t port) {
    if (port == MACH_PORT_NULL) return -1;
    struct thread_extended_info info;
    mach_msg_type_number_t count = THREAD_EXTENDED_INFO_COUNT;
    if (thread_info(port, THREAD_EXTENDED_INFO, (thread_info_t)&info, &count) != KERN_SUCCESS)
        return -1;
    return info.pth_curpri;
}

static void *vcpu_thread(void *arg) {
    ring_t *r = arg;
    atomic_store(&r->port, mach_thread_self());
    uint64_t period = ns_to_abs(10 * 1000 * 1000);
    uint64_t next = mach_absolute_time() + period;
    while (mach_absolute_time() < g_end_abs) {
        mach_wait_until(next);
        uint64_t now = mach_absolute_time();
        ring_put(r, (uint32_t)(abs_to_ns(now > next ? now - next : 0) / 1000));
        next += period;
        if (next < now) next = now + period;
    }
    return NULL;
}

static void *rt_thread(void *arg) {
    (void)arg;
    atomic_store(&g_rt.port, mach_thread_self());
    struct thread_time_constraint_policy pol = {
        .period = (uint32_t)ns_to_abs(16666667),
        .computation = (uint32_t)ns_to_abs(1000000),
        .constraint = (uint32_t)ns_to_abs(2000000),
        .preemptible = 1,
    };
    kern_return_t kr = thread_policy_set(mach_thread_self(), THREAD_TIME_CONSTRAINT_POLICY,
                                         (thread_policy_t)&pol, THREAD_TIME_CONSTRAINT_POLICY_COUNT);
    if (kr != KERN_SUCCESS) fprintf(stderr, "%s: time-constraint policy refused: %d\n", g_label, kr);
    uint64_t period = pol.period;
    uint64_t next = mach_absolute_time() + period;
    while (mach_absolute_time() < g_end_abs) {
        mach_wait_until(next);
        uint64_t now = mach_absolute_time();
        ring_put(&g_rt, (uint32_t)(abs_to_ns(now > next ? now - next : 0) / 1000));
        next += period;
        if (next < now) next = now + period;
    }
    return NULL;
}

static id begin_activity(const char *which) {
    NSActivityOptions opts;
    if (!which || !strcmp(which, "none")) return nil;
    if (!strcmp(which, "user")) opts = NSActivityUserInitiated;
    else if (!strcmp(which, "latency")) opts = NSActivityUserInitiated | NSActivityLatencyCritical;
    else {
        fprintf(stderr, "unknown activity %s\n", which);
        exit(2);
    }
    return [[NSProcessInfo processInfo] beginActivityWithOptions:opts reason:@"game-mode-throttle probe"];
}

static int read_prio(int which, int *err) {
    errno = 0;
    int v = getpriority(which, 0);
    *err = (v == -1 && errno) ? errno : 0;
    return v;
}

static void *guard_thread(void *arg) {
    (void)arg;
    int want_role = !strcmp(g_guard, "role") || !strcmp(g_guard, "both");
    int want_bg = !strcmp(g_guard, "bg") || !strcmp(g_guard, "both");
    while (mach_absolute_time() < g_end_abs) {
        int err;
        if (want_role && read_prio(PRIO_DARWIN_ROLE, &err) == PRIO_DARWIN_ROLE_DARWIN_BG && !err) {
            if (setpriority(PRIO_DARWIN_ROLE, 0, g_guard_role_to) == 0) atomic_fetch_add(&g_resets, 1);
            else {
                atomic_fetch_add(&g_errors, 1);
                atomic_store(&g_last_errno, errno);
            }
        }
        if (want_bg && read_prio(PRIO_DARWIN_PROCESS, &err) > 0 && !err) {
            if (setpriority(PRIO_DARWIN_PROCESS, 0, 0) == 0) atomic_fetch_add(&g_resets, 1);
            else {
                atomic_fetch_add(&g_errors, 1);
                atomic_store(&g_last_errno, errno);
            }
        }
        usleep(50 * 1000);
    }
    return NULL;
}

static void start_guard(void) {
    if (!strcmp(g_guard, "none")) return;
    pthread_t th;
    pthread_create(&th, NULL, guard_thread, NULL);
    pthread_detach(th);
}

static void darwin_state(char *out, size_t len) {
    int re, be;
    int role = read_prio(PRIO_DARWIN_ROLE, &re);
    int bg = read_prio(PRIO_DARWIN_PROCESS, &be);
    char rs[24], bs[24];
    if (re) snprintf(rs, sizeof rs, "err%d", re); else snprintf(rs, sizeof rs, "%d", role);
    if (be) snprintf(bs, sizeof bs, "err%d", be); else snprintf(bs, sizeof bs, "%d", bg);
    snprintf(out, len, "role=%s bg=%s guard=%d/%d(errno %d)", rs, bs, atomic_load(&g_resets),
             atomic_load(&g_errors), atomic_load(&g_last_errno));
}

static void report(const char *role, double t, int with_workers) {
    char ds[96];
    darwin_state(ds, sizeof ds);
    if (!with_workers) {
        printf("%s %s t=%.0f prio main=%d | %s\n", g_label, role, t, curpri(atomic_load(&g_main_port)), ds);
        fflush(stdout);
        return;
    }
    char rts[64], vs[NVCPU][64];
    ring_stats(&g_rt, rts, sizeof rts);
    for (int i = 0; i < NVCPU; i++) ring_stats(&g_vcpu[i], vs[i], sizeof vs[i]);
    printf("%s %s t=%.0f prio main=%d vcpu=%d,%d,%d,%d rt=%d | rt late us %s | vcpu0 late us %s | %s\n",
           g_label, role, t, curpri(atomic_load(&g_main_port)), curpri(atomic_load(&g_vcpu[0].port)),
           curpri(atomic_load(&g_vcpu[1].port)), curpri(atomic_load(&g_vcpu[2].port)),
           curpri(atomic_load(&g_vcpu[3].port)), curpri(atomic_load(&g_rt.port)), rts, vs[0], ds);
    fflush(stdout);
}

static int run_child(const char *activity) {
    id act = begin_activity(activity);
    (void)act;
    start_guard();
    pthread_t th[NVCPU + 1];
    for (int i = 0; i < NVCPU; i++) pthread_create(&th[i], NULL, vcpu_thread, &g_vcpu[i]);
    pthread_create(&th[NVCPU], NULL, rt_thread, NULL);
    uint64_t t0 = mach_absolute_time();
    for (;;) {
        sleep(1);
        uint64_t now = mach_absolute_time();
        report("child", abs_to_ns(now - t0) / 1e9, 1);
        if (now >= g_end_abs) break;
    }
    for (int i = 0; i <= NVCPU; i++) pthread_join(th[i], NULL);
    return 0;
}

static int run_parent(const char *activity, const char *child_activity, const char *self) {
    @autoreleasepool {
        id act = begin_activity(activity);
        (void)act;
        start_guard();
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        NSWindow *w = [[NSWindow alloc] initWithContentRect:NSMakeRect(80, 80, 360, 200)
                                                  styleMask:NSWindowStyleMaskTitled
                                                    backing:NSBackingStoreBuffered
                                                      defer:NO];
        w.title = [NSString stringWithFormat:@"game-mode probe %s", g_label];
        [w makeKeyAndOrderFront:nil];
        [app finishLaunching];

        char secs[32];
        snprintf(secs, sizeof secs, "%g", g_secs);
        char *argv[] = {(char *)self, "child", "--secs", secs, "--activity", (char *)child_activity,
                        "--label", (char *)g_label, "--guard", (char *)g_guard, NULL};
        pid_t pid;
        int rc = posix_spawn(&pid, self, NULL, NULL, argv, environ);
        if (rc != 0) {
            fprintf(stderr, "posix_spawn: %s\n", strerror(rc));
            return 1;
        }
        printf("%s parent pid=%d child pid=%d activity=%s child-activity=%s\n", g_label, getpid(), pid,
               activity, child_activity);
        fflush(stdout);

        uint64_t t0 = mach_absolute_time();
        uint64_t next = t0 + ns_to_abs(1000000000ull);
        while (mach_absolute_time() < g_end_abs + ns_to_abs(1500000000ull)) {
            @autoreleasepool {
                NSEvent *ev = [app nextEventMatchingMask:NSEventMaskAny
                                               untilDate:[NSDate dateWithTimeIntervalSinceNow:0.1]
                                                  inMode:NSDefaultRunLoopMode
                                                 dequeue:YES];
                if (ev) [app sendEvent:ev];
            }
            uint64_t now = mach_absolute_time();
            if (now >= next) {
                report("parent", abs_to_ns(now - t0) / 1e9, 0);
                next += ns_to_abs(1000000000ull);
            }
        }
        int st;
        waitpid(pid, &st, 0);
        return 0;
    }
}

int main(int argc, char **argv) {
    mach_timebase_info(&g_tb);
    atomic_store(&g_main_port, mach_thread_self());
    if (argc < 2) {
        fprintf(stderr, "usage: probe parent|child --secs N [--activity A] [--child-activity B] [--label L]\n");
        return 2;
    }
    const char *mode = argv[1], *activity = "none", *child_activity = "none";
    for (int i = 2; i + 1 < argc; i += 2) {
        if (!strcmp(argv[i], "--secs")) g_secs = atof(argv[i + 1]);
        else if (!strcmp(argv[i], "--activity")) activity = argv[i + 1];
        else if (!strcmp(argv[i], "--child-activity")) child_activity = argv[i + 1];
        else if (!strcmp(argv[i], "--label")) g_label = argv[i + 1];
        else if (!strcmp(argv[i], "--guard")) g_guard = argv[i + 1];
        else {
            fprintf(stderr, "unknown flag %s\n", argv[i]);
            return 2;
        }
    }
    if (g_secs <= 0 || g_secs > 120) {
        fprintf(stderr, "--secs must be in (0, 120]\n");
        return 2;
    }
    g_end_abs = mach_absolute_time() + ns_to_abs((uint64_t)(g_secs * 1e9));
    if (!strcmp(mode, "child")) return run_child(activity);
    if (!strcmp(mode, "parent")) return run_parent(activity, child_activity, argv[0]);
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
}
