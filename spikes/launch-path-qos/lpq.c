// Thread policy x launch path: how late does a mostly idle thread wake, and how fast does it run?
//
// One measured thread per arm, in a fresh process (thread policies are additive on a thread). Each
// period it waits for an absolute deadline 16.667 ms after the last one, records how late it woke
// and on which CPU, then does --busy-us of fixed work in chunks, timing every chunk and recording
// the CPU it ran on. That separates the two things a guest cannot tell apart: placement (which core)
// and clock (how fast that core ran the chunk).
//
//   lpq arm --policy P --busy-us B [--periods N] [--ecores 0,1] [--label L]
//   lpq driver --policies a,b,... --busy b1,b2,... [--periods N] [--ecores 0,1] [--label L] [--reverse]
//       runs every policy x busy cell as a child `lpq arm`, in order (or reversed), each bounded.
//
// Policies: default | utility | ui (QOS_CLASS_USER_INTERACTIVE) | lat0 (THREAD_LATENCY_QOS_POLICY
// tier 0) | critical (kqueue EVFILT_TIMER NOTE_CRITICAL wait) | rt (libkrun's band:
// THREAD_TIME_CONSTRAINT_POLICY 16.667/1/2 ms) | wg (joins an AudioWorkIntervalCreate workgroup and
// brackets every period with interval start/finish) | wgrt (rt, then wg).
//
// Every arm prints one `ident` line (who launched us, as the kernel sees it) and one `result` line.
#include <AudioToolbox/AudioWorkInterval.h>
#include <errno.h>
#include <libproc.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <mach/task_policy.h>
#include <os/workgroup.h>
#include <pthread.h>
#include <pthread/qos.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/resource.h>
#include <sys/sysctl.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

#define PRIO_DARWIN_ROLE 6
// Private libproc flavor (xnu bsd/sys/proc_info_private.h): the process's coalition ids.
#define PROC_PIDCOALITIONINFO 20
struct proc_pidcoalitioninfo {
    uint64_t coalition_id[2]; // [0] resource, [1] jetsam
    uint64_t reserved1, reserved2, reserved3;
};

#define MAXCPU 64
#define CHUNK_ITERS 512
#define MAXCHUNKS (4u << 20)
#define MAXPER 4096

static mach_timebase_info_data_t tb;
static uint64_t ns2abs(uint64_t ns) { return ns * tb.denom / tb.numer; }
static double abs2us(uint64_t a) { return (double)a * tb.numer / tb.denom / 1e3; }
static inline unsigned cpu_now(void) {
    uint64_t v;
    __asm__ volatile("mrs %0, tpidr_el0" : "=r"(v));
    return (unsigned)(v & 0xfff);
}

static int is_e[MAXCPU];
static const char *policy = "default", *label = "-";
static unsigned busy_us = 300, periods = 240;

// results (written by the measured thread, read by main after join)
static uint64_t late[MAXPER];
static unsigned nlate, wake_e, wake_p, rtpri_periods;
static uint32_t *chunks; // ticks, bit 31 = ran on an E-core
static unsigned nchunks;
static uint64_t cpu_chunks[MAXCPU], cpu_ticks[MAXCPU];
static int rc_policy, rc_join = -1, wg_start_err, wg_finish_err, kq_err;
static int pri_after = -1, base_after = -1;
static unsigned qos_after;

static void thread_pri(int *cur, int *base) {
    thread_extended_info_data_t info;
    mach_msg_type_number_t cnt = THREAD_EXTENDED_INFO_COUNT;
    mach_port_t self = mach_thread_self();
    kern_return_t kr = thread_info(self, THREAD_EXTENDED_INFO, (thread_info_t)&info, &cnt);
    mach_port_deallocate(mach_task_self(), self);
    *cur = kr == KERN_SUCCESS ? info.pth_curpri : -1;
    if (base) *base = kr == KERN_SUCCESS ? info.pth_priority : -1;
}

static int set_rt(void) {
    thread_time_constraint_policy_data_t p = {
        .period = (uint32_t)ns2abs(16667000),
        .computation = (uint32_t)ns2abs(1000000),
        .constraint = (uint32_t)ns2abs(2000000),
        .preemptible = 1,
    };
    mach_port_t self = mach_thread_self();
    kern_return_t kr = thread_policy_set(self, THREAD_TIME_CONSTRAINT_POLICY, (thread_policy_t)&p,
                                         THREAD_TIME_CONSTRAINT_POLICY_COUNT);
    mach_port_deallocate(mach_task_self(), self);
    return kr;
}

static int set_lat0(void) {
    thread_latency_qos_policy_data_t p = {.thread_latency_qos_tier = LATENCY_QOS_TIER_0};
    mach_port_t self = mach_thread_self();
    kern_return_t kr = thread_policy_set(self, THREAD_LATENCY_QOS_POLICY, (thread_policy_t)&p,
                                         THREAD_LATENCY_QOS_POLICY_COUNT);
    mach_port_deallocate(mach_task_self(), self);
    return kr;
}

static void *measured(void *arg) {
    (void)arg;
    int use_wg = !strcmp(policy, "wg") || !strcmp(policy, "wgrt");
    int use_kq = !strcmp(policy, "critical");
    if (!strcmp(policy, "utility")) rc_policy = pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0);
    else if (!strcmp(policy, "ui")) rc_policy = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
    else if (!strcmp(policy, "lat0")) rc_policy = set_lat0();
    else if (!strcmp(policy, "rt") || !strcmp(policy, "wgrt")) rc_policy = set_rt();

    os_workgroup_interval_t wg = NULL;
    os_workgroup_join_token_s tok;
    if (use_wg) {
        wg = AudioWorkIntervalCreate("lpq", OS_CLOCK_MACH_ABSOLUTE_TIME, NULL);
        rc_join = wg ? os_workgroup_join(wg, &tok) : -2;
    }
    int kq = -1;
    if (use_kq) kq = kqueue();
    thread_pri(&pri_after, &base_after);
    qos_after = qos_class_self();

    uint64_t period = ns2abs(16667000), busy = ns2abs((uint64_t)busy_us * 1000);
    uint64_t d = mach_absolute_time() + period;
    volatile uint64_t x = 1;
    for (unsigned i = 0; i < periods; i++) {
        if (use_kq) {
            struct kevent ev;
            EV_SET(&ev, 1, EVFILT_TIMER, EV_ADD | EV_ONESHOT, NOTE_ABSOLUTE | NOTE_MACHTIME | NOTE_CRITICAL, d,
                   NULL);
            struct kevent out;
            if (kevent(kq, &ev, 1, &out, 1, NULL) != 1) kq_err++;
        } else {
            mach_wait_until(d);
        }
        uint64_t t = mach_absolute_time();
        unsigned c = cpu_now();
        if (c < MAXCPU && is_e[c]) wake_e++; else wake_p++;
        if (nlate < MAXPER) late[nlate++] = t > d ? t - d : 0;
        if (wg && rc_join == 0 && os_workgroup_interval_start(wg, d, d + period, NULL)) wg_start_err++;
        uint64_t end = t + busy;
        uint64_t now = t;
        while (now < end) {
            uint64_t t0 = mach_absolute_time();
            for (int k = 0; k < CHUNK_ITERS; k++) x = x * 6364136223846793005ULL + 1442695040888963407ULL;
            now = mach_absolute_time();
            unsigned cc = cpu_now();
            uint64_t dt = now - t0;
            if (cc < MAXCPU) cpu_chunks[cc]++, cpu_ticks[cc] += dt;
            if (nchunks < MAXCHUNKS)
                chunks[nchunks++] = (uint32_t)(dt > 0x7fffffff ? 0x7fffffff : dt) | (cc < MAXCPU && is_e[cc] ? 0x80000000u : 0);
        }
        int cur;
        thread_pri(&cur, NULL);
        if (cur >= 97) rtpri_periods++;
        if (wg && rc_join == 0 && os_workgroup_interval_finish(wg, NULL)) wg_finish_err++;
        d += period;
        uint64_t n = mach_absolute_time();
        if (d < n) d = n + period; // overran: do not count a backlog as lateness
    }
    if (wg && rc_join == 0) os_workgroup_leave(wg, &tok);
    if (kq >= 0) close(kq);
    return NULL;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}
static int cmp_u32(const void *a, const void *b) {
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;
    return (x > y) - (x < y);
}

static void parse_ecores(const char *v) {
    char *s = strdup(v), *tok;
    while ((tok = strsep(&s, ","))) if (*tok) is_e[atoi(tok)] = 1;
}

static void ident(void) {
    struct proc_pidcoalitioninfo ci;
    memset(&ci, 0, sizeof ci);
    int n = proc_pidinfo(getpid(), PROC_PIDCOALITIONINFO, 0, &ci, sizeof ci);
    errno = 0;
    int role = getpriority(PRIO_DARWIN_ROLE, 0);
    int role_err = errno;
    task_category_policy_data_t cat = {0};
    mach_msg_type_number_t cnt = TASK_CATEGORY_POLICY_COUNT;
    boolean_t def = 0;
    kern_return_t kr = task_policy_get(mach_task_self(), TASK_CATEGORY_POLICY, (task_policy_t)&cat, &cnt, &def);
    int cur, base;
    thread_pri(&cur, &base);
    printf("%s ident pid=%d ppid=%d coalition=%llu/%llu(%d) darwin_role=%d%s task_role=%d%s main_pri=%d/%d main_qos=0x%x\n",
           label, getpid(), getppid(), (unsigned long long)ci.coalition_id[0],
           (unsigned long long)ci.coalition_id[1], n, role, role_err ? "(err)" : "", kr == KERN_SUCCESS ? cat.role : -99,
           def ? "(default)" : "", cur, base, qos_class_self());
}

static int run_arm(void) {
    chunks = malloc(sizeof(uint32_t) * MAXCHUNKS);
    ident();
    pthread_t th;
    pthread_create(&th, NULL, measured, NULL);
    pthread_join(th, NULL);

    qsort(late, nlate, sizeof(uint64_t), cmp_u64);
    unsigned over2 = 0, over8 = 0;
    for (unsigned i = 0; i < nlate; i++) {
        if (late[i] > ns2abs(2000000)) over2++;
        if (late[i] > ns2abs(8000000)) over8++;
    }
    // chunk medians: overall, E only, P only
    uint32_t *e = malloc(sizeof(uint32_t) * nchunks), *p = malloc(sizeof(uint32_t) * nchunks), *all = malloc(sizeof(uint32_t) * nchunks);
    unsigned ne = 0, np = 0;
    for (unsigned i = 0; i < nchunks; i++) {
        uint32_t v = chunks[i] & 0x7fffffff;
        all[i] = v;
        if (chunks[i] & 0x80000000u) e[ne++] = v; else p[np++] = v;
    }
    qsort(all, nchunks, sizeof(uint32_t), cmp_u32);
    qsort(e, ne, sizeof(uint32_t), cmp_u32);
    qsort(p, np, sizeof(uint32_t), cmp_u32);
    double tick_ns = (double)tb.numer / tb.denom;
    printf("%s result policy=%s busy_us=%u periods=%u rc_policy=%d rc_join=%d wg_err=%d/%d kq_err=%d pri=%d/%d qos=0x%x "
           "rtpri_periods=%u late_us p50=%.0f p90=%.0f p99=%.0f max=%.0f over2ms=%u over8ms=%u wakeE=%u wakeP=%u "
           "chunks=%u onE=%.3f chunk_ns med=%.0f p90=%.0f medE=%.0f medP=%.0f\n",
           label, policy, busy_us, periods, rc_policy, rc_join, wg_start_err, wg_finish_err, kq_err, pri_after,
           base_after, qos_after, rtpri_periods, abs2us(late[nlate / 2]), abs2us(late[nlate * 9 / 10]),
           abs2us(late[nlate * 99 / 100]), abs2us(late[nlate - 1]), over2, over8, wake_e, wake_p, nchunks,
           nchunks ? (double)ne / nchunks : 0, nchunks ? all[nchunks / 2] * tick_ns : 0,
           nchunks ? all[nchunks * 9 / 10] * tick_ns : 0, ne ? e[ne / 2] * tick_ns : 0, np ? p[np / 2] * tick_ns : 0);
    printf("%s percpu policy=%s busy_us=%u cpu:chunks:mean_ns", label, policy, busy_us);
    for (int c = 0; c < MAXCPU; c++)
        if (cpu_chunks[c]) printf(" %d:%llu:%.0f", c, (unsigned long long)cpu_chunks[c], cpu_ticks[c] * tick_ns / cpu_chunks[c]);
    printf("\n");
    fflush(stdout);
    return 0;
}

static int run_driver(int argc, char **argv, const char *policies, const char *busies, const char *ecores, int reverse) {
    (void)argc;
    char *pl[16], *bl[16];
    int npol = 0, nbusy = 0;
    char *s = strdup(policies), *tok;
    while ((tok = strsep(&s, ",")) && npol < 16) if (*tok) pl[npol++] = tok;
    s = strdup(busies);
    while ((tok = strsep(&s, ",")) && nbusy < 16) if (*tok) bl[nbusy++] = tok;
    ident();
    char per[16];
    snprintf(per, sizeof per, "%u", periods);
    int total = npol * nbusy;
    for (int k = 0; k < total; k++) {
        int idx = reverse ? total - 1 - k : k;
        char *pol = pl[idx / nbusy], *b = bl[idx % nbusy];
        char *av[] = {argv[0], "arm", "--policy", pol, "--busy-us", b, "--periods", per, "--ecores", (char *)ecores,
                      "--label", (char *)label, NULL};
        fflush(stdout);
        pid_t pid;
        if (posix_spawn(&pid, argv[0], NULL, NULL, av, environ)) {
            printf("%s spawn-failed policy=%s busy=%s errno=%d\n", label, pol, b, errno);
            continue;
        }
        // Bound every arm: its periods plus 5 s, then kill it.
        uint64_t deadline = mach_absolute_time() + ns2abs((uint64_t)periods * 16667000ULL + 5000000000ULL);
        int st = 0;
        for (;;) {
            pid_t r = waitpid(pid, &st, WNOHANG);
            if (r == pid) break;
            if (mach_absolute_time() > deadline) {
                kill(pid, SIGKILL);
                waitpid(pid, &st, 0);
                printf("%s ABORT policy=%s busy=%s overran its bound\n", label, pol, b);
                break;
            }
            usleep(50000);
        }
        usleep(500000); // let the host settle between arms
    }
    printf("%s DONE\n", label);
    fflush(stdout);
    return 0;
}

int main(int argc, char **argv) {
    mach_timebase_info(&tb);
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc < 2) {
        fprintf(stderr, "usage: see the header of lpq.c\n");
        return 2;
    }
    const char *mode = argv[1], *policies = "default", *busies = "300", *ecores = "0,1";
    int reverse = 0;
    for (int i = 2; i < argc; i++) {
        const char *a = argv[i], *v = i + 1 < argc ? argv[i + 1] : "";
        if (!strcmp(a, "--policy")) policy = v, i++;
        else if (!strcmp(a, "--policies")) policies = v, i++;
        else if (!strcmp(a, "--busy-us")) busy_us = (unsigned)atoi(v), i++;
        else if (!strcmp(a, "--busy")) busies = v, i++;
        else if (!strcmp(a, "--periods")) periods = (unsigned)atoi(v), i++;
        else if (!strcmp(a, "--ecores")) ecores = v, i++;
        else if (!strcmp(a, "--label")) label = v, i++;
        else if (!strcmp(a, "--reverse")) reverse = 1;
        else {
            fprintf(stderr, "unknown arg %s\n", a);
            return 2;
        }
    }
    if (periods > MAXPER) periods = MAXPER;
    parse_ecores(ecores);
    if (!strcmp(mode, "arm")) return run_arm();
    if (!strcmp(mode, "driver")) return run_driver(argc, argv, policies, busies, ecores, reverse);
    fprintf(stderr, "unknown mode %s\n", mode);
    return 2;
}
