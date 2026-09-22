// Where does xnu run a THREAD_TIME_CONSTRAINT_POLICY thread on an asymmetric Apple Silicon host?
//
// Each worker thread records, from its own loop, the CPU it is executing on about
// every 20 us while it runs, bucketed by whether its current priority was in the real-time band
// (>= 97) at the time. A monitor thread samples per-CPU processor state and tick counters, to look
// for a cluster going offline.
//
// Usage: placement <mode> [options]
//   mode  calib-all    one plain spinner per logical CPU (encoding check: every id must appear)
//         calib-bg     --threads spinners at QOS_CLASS_BACKGROUND (which ids are efficiency cores)
//         run          the scenario given by the options below
//   --rt N           real-time threads (default 0)
//   --plain N        ordinary threads doing the same work (default 0)
//   --hog N          ordinary saturating spinners with no measurement (host load)
//   --work burst|spin   burst: wake at a 16.667 ms deadline, spin --burst-us, sleep again;
//                       spin: compute flat out, parking 100 us every 250 ms (the heartbeat, so xnu's
//                       RT fail-safe does not demote the thread)
//   --burst-us U     burst length (default 300)
//   --rt-qos bg|bg-after|none   also put the RT threads at QOS_CLASS_BACKGROUND, before (bg) or
//                               after (bg-after) the time-constraint policy
//   --secs S         duration (default 10, capped at 30)
//   --ecores LIST    comma list of efficiency-core ids for the E/P summary
//   --label TEXT     tag printed on every line
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <mach/thread_info.h>
#include <mach/processor_info.h>
#include <pthread.h>
#include <pthread/qos.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysctl.h>
#include <unistd.h>

#define MAXCPU 64
#define MAXTHR 32
#define RT_PRI 97
#define MAXBUCKET 128

// The current CPU number. On macOS 26 (arm64) libsystem_pthread's pthread_cpu_number_np is
// `mrs x9, TPIDR_EL0; and x9, x9, #0xfff` (disassembled); TPIDRRO_EL0 holds the TSD pointer, not
// the CPU. Read the register directly (no call in the hot loop) and cross-check against the API.
static inline uint64_t cpureg(void) {
    uint64_t v;
    __asm__ volatile("mrs %0, tpidr_el0" : "=r"(v));
    return v;
}

static uint64_t cpu_mask = 0xfff;
static inline unsigned cpu_now(void) { return (unsigned)(cpureg() & cpu_mask); }

static mach_timebase_info_data_t tb;
static uint64_t ns2abs(uint64_t ns) { return ns * tb.denom / tb.numer; }
static uint64_t abs2ns(uint64_t a) { return a * tb.numer / tb.denom; }

enum kind { K_RT, K_PLAIN, K_HOG, K_BG };
enum work { W_BURST, W_SPIN };

typedef struct {
    int idx;
    enum kind kind;
    enum work work;
    int rt_qos; // 0 none, 1 bg before rt, 2 bg after rt
    int ret_policy, ret_qos;
    uint64_t end_abs;
    // measurement
    uint64_t hist_rt[MAXCPU], hist_ts[MAXCPU];
    uint64_t wake_cpu[MAXCPU];
    uint64_t iters_cpu[MAXCPU], abs_cpu[MAXCPU];
    int pri_min, pri_max;
    uint64_t pri_samples, pri_rt_samples;
    uint64_t raw_or, raw_and; // bits seen in TPIDR_EL0
    uint64_t late_ns[4096];
    unsigned nlate;
    uint64_t mismatch, checks; // TPIDR_EL0 & mask vs pthread_cpu_number_np
    uint32_t b_e_rt[MAXBUCKET], b_p_rt[MAXBUCKET], b_e_ts[MAXBUCKET], b_p_ts[MAXBUCKET];
} worker_t;

static worker_t workers[MAXTHR];
static int nworkers;
static atomic_int stop;
static int is_e[MAXCPU];
static uint64_t run_start_abs, bucket_abs, spin_after_abs;
static uint64_t burst_abs, period_abs, sample_abs, pri_abs, hb_every_abs, hb_park_abs;

static int cur_pri(void) {
    thread_extended_info_data_t info;
    mach_msg_type_number_t cnt = THREAD_EXTENDED_INFO_COUNT;
    mach_port_t self = mach_thread_self();
    kern_return_t kr = thread_info(self, THREAD_EXTENDED_INFO, (thread_info_t)&info, &cnt);
    mach_port_deallocate(mach_task_self(), self);
    return kr == KERN_SUCCESS ? info.pth_curpri : -1;
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

// Measured work: spin, recording the CPU roughly every sample_abs, until `until`.
static inline void spin_measure(worker_t *w, uint64_t until, int *pri) {
    uint64_t now = mach_absolute_time(), next = now + sample_abs, last = now, nextpri = now;
    uint64_t it = 0, since = 0;
    volatile uint64_t x = 1;
    while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
        for (int k = 0; k < 64; k++) x = x * 6364136223846793005ULL + 1442695040888963407ULL;
        it += 64;
        since += 64;
        now = mach_absolute_time();
        if (now < next) continue;
        uint64_t raw = cpureg();
        unsigned c = (unsigned)(raw & cpu_mask);
        w->raw_or |= raw;
        w->raw_and &= raw;
        if (now >= nextpri) {
            *pri = cur_pri();
            nextpri = now + pri_abs;
            w->pri_samples++;
            if (*pri >= RT_PRI) w->pri_rt_samples++;
            if (*pri < w->pri_min) w->pri_min = *pri;
            if (*pri > w->pri_max) w->pri_max = *pri;
        }
        size_t pc = 0;
        if (pthread_cpu_number_np(&pc) == 0) {
            w->checks++;
            if (pc != c) w->mismatch++;
        }
        if (c < MAXCPU) {
            uint64_t b = (now - run_start_abs) / bucket_abs;
            if (b >= MAXBUCKET) b = MAXBUCKET - 1;
            if (*pri >= RT_PRI) {
                w->hist_rt[c]++;
                if (is_e[c]) w->b_e_rt[b]++; else w->b_p_rt[b]++;
            } else {
                w->hist_ts[c]++;
                if (is_e[c]) w->b_e_ts[b]++; else w->b_p_ts[b]++;
            }
            w->iters_cpu[c] += since;
            w->abs_cpu[c] += now - last;
        }
        since = 0;
        last = now;
        next = now + sample_abs;
        if (now >= until) break;
    }
    (void)it;
}

static void *worker_main(void *arg) {
    worker_t *w = arg;
    w->pri_min = 1000;
    w->pri_max = -1;
    w->raw_and = ~0ULL;
    w->ret_policy = w->ret_qos = 0;
    if (w->kind == K_BG) w->ret_qos = pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0);
    if (w->kind == K_RT) {
        if (w->rt_qos == 1) w->ret_qos = pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0);
        w->ret_policy = set_rt();
        if (w->rt_qos == 2) w->ret_qos = pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0);
    }
    int pri = cur_pri();
    if (w->kind == K_HOG) {
        volatile uint64_t x = 1;
        while (!atomic_load_explicit(&stop, memory_order_relaxed) && mach_absolute_time() < w->end_abs)
            for (int k = 0; k < 4096; k++) x = x * 6364136223846793005ULL + 1;
        return NULL;
    }
    if (w->work == W_BURST) {
        uint64_t burst_end = spin_after_abs ? run_start_abs + spin_after_abs : w->end_abs;
        uint64_t d = mach_absolute_time() + period_abs;
        while (!atomic_load(&stop) && d < burst_end) {
            mach_wait_until(d);
            uint64_t t = mach_absolute_time();
            unsigned c = cpu_now();
            if (c < MAXCPU) w->wake_cpu[c]++;
            if (w->nlate < 4096) w->late_ns[w->nlate++] = t > d ? abs2ns(t - d) : 0;
            spin_measure(w, t + burst_abs, &pri);
            d += period_abs;
            uint64_t n = mach_absolute_time();
            if (d < n) d = n + period_abs;
        }
        if (!spin_after_abs) return NULL;
    }
    {
        uint64_t now = mach_absolute_time();
        while (!atomic_load(&stop) && now < w->end_abs) {
            uint64_t until = now + hb_every_abs;
            if (until > w->end_abs) until = w->end_abs;
            spin_measure(w, until, &pri);
            // Heartbeat: a real park clears xnu's RT computation accumulator.
            mach_wait_until(mach_absolute_time() + hb_park_abs);
            now = mach_absolute_time();
        }
    }
    return NULL;
}

// ---- monitor: per-CPU processor state + ticks ----
static int ncpu;
static uint64_t mon_offline[MAXCPU], mon_stalled[MAXCPU], mon_busy[MAXCPU], mon_total[MAXCPU];
static uint64_t mon_intervals, mon_late_max_ns;
static uint64_t rec_full, rec_partial, rec_and = ~0ULL, rec_or;
static uint64_t rec_seen[16];
static int nrec_seen;
static uint64_t rec_now(void) {
    uint64_t v = 0;
    size_t l = sizeof v;
    if (sysctlbyname("kern.sched_recommended_cores", &v, &l, NULL, 0)) return ~0ULL;
    if (l == 4) v &= 0xffffffffULL;
    return v;
}

static int read_ticks(uint64_t ticks[][CPU_STATE_MAX], int running[]) {
    natural_t n;
    processor_info_array_t a;
    mach_msg_type_number_t cnt;
    if (host_processor_info(mach_host_self(), PROCESSOR_CPU_LOAD_INFO, &n, &a, &cnt) != KERN_SUCCESS)
        return -1;
    for (natural_t i = 0; i < n && i < MAXCPU; i++)
        for (int s = 0; s < CPU_STATE_MAX; s++) ticks[i][s] = ((processor_cpu_load_info_t)a)[i].cpu_ticks[s];
    vm_deallocate(mach_task_self(), (vm_address_t)a, cnt * sizeof(integer_t));
    if (host_processor_info(mach_host_self(), PROCESSOR_BASIC_INFO, &n, &a, &cnt) != KERN_SUCCESS)
        return -1;
    for (natural_t i = 0; i < n && i < MAXCPU; i++) running[i] = ((processor_basic_info_t)a)[i].running;
    vm_deallocate(mach_task_self(), (vm_address_t)a, cnt * sizeof(integer_t));
    return (int)n;
}

static uint64_t run_end_abs;
static void *monitor_main(void *arg) {
    (void)arg;
    uint64_t prev[MAXCPU][CPU_STATE_MAX], cur[MAXCPU][CPU_STATE_MAX];
    int running[MAXCPU];
    read_ticks(prev, running);
    uint64_t interval = ns2abs(100000000), d = mach_absolute_time() + interval;
    while (!atomic_load(&stop) && d < run_end_abs) {
        mach_wait_until(d);
        uint64_t t = mach_absolute_time();
        if (abs2ns(t - d) > mon_late_max_ns) mon_late_max_ns = abs2ns(t - d);
        int n = read_ticks(cur, running);
        for (int i = 0; i < n && i < ncpu; i++) {
            uint64_t busy = 0, tot = 0;
            for (int s = 0; s < CPU_STATE_MAX; s++) {
                uint64_t dd = cur[i][s] - prev[i][s];
                tot += dd;
                if (s != CPU_STATE_IDLE) busy += dd;
            }
            if (!running[i]) mon_offline[i]++;
            if (tot == 0) mon_stalled[i]++;
            mon_busy[i] += busy;
            mon_total[i] += tot;
        }
        memcpy(prev, cur, sizeof prev);
        uint64_t r = rec_now(), full = (1ULL << ncpu) - 1;
        if ((r & full) == full) rec_full++; else rec_partial++;
        rec_and &= r;
        rec_or |= r;
        int seen = 0;
        for (int k = 0; k < nrec_seen; k++) if (rec_seen[k] == r) seen = 1;
        if (!seen && nrec_seen < 16) rec_seen[nrec_seen++] = r;
        mon_intervals++;
        d += interval;
    }
    return NULL;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

int main(int argc, char **argv) {
    mach_timebase_info(&tb);
    size_t len = sizeof ncpu;
    sysctlbyname("hw.ncpu", &ncpu, &len, NULL, 0);
    if (argc < 2) {
        fprintf(stderr, "usage: see header of placement.c\n");
        return 2;
    }
    const char *mode = argv[1], *label = "-";
    int bucket_ms = 1000, spin_after = 0;
    int nrt = 0, nplain = 0, nhog = 0, secs = 10, threads = 2, rt_qos = 0;
    enum work work = W_SPIN;
    unsigned burst_us = 300;
    for (int i = 2; i < argc; i++) {
        const char *a = argv[i], *v = i + 1 < argc ? argv[i + 1] : "";
        if (!strcmp(a, "--rt")) nrt = atoi(v), i++;
        else if (!strcmp(a, "--plain")) nplain = atoi(v), i++;
        else if (!strcmp(a, "--hog")) nhog = atoi(v), i++;
        else if (!strcmp(a, "--threads")) threads = atoi(v), i++;
        else if (!strcmp(a, "--secs")) secs = atoi(v), i++;
        else if (!strcmp(a, "--burst-us")) burst_us = (unsigned)atoi(v), i++;
        else if (!strcmp(a, "--label")) label = v, i++;
        else if (!strcmp(a, "--bucket-ms")) bucket_ms = atoi(v), i++;
        else if (!strcmp(a, "--spin-after")) spin_after = atoi(v), i++;
        else if (!strcmp(a, "--work")) work = !strcmp(v, "burst") ? W_BURST : W_SPIN, i++;
        else if (!strcmp(a, "--rt-qos")) rt_qos = !strcmp(v, "bg") ? 1 : !strcmp(v, "bg-after") ? 2 : 0, i++;
        else if (!strcmp(a, "--mask")) cpu_mask = strtoull(v, NULL, 0), i++;
        else if (!strcmp(a, "--ecores")) {
            char *s = strdup(v), *tok;
            while ((tok = strsep(&s, ","))) if (*tok) is_e[atoi(tok)] = 1;
            i++;
        } else {
            fprintf(stderr, "unknown arg %s\n", a);
            return 2;
        }
    }
    if (secs > 30) secs = 30;
    int pcores = 0;
    len = sizeof pcores;
    sysctlbyname("hw.perflevel0.logicalcpu", &pcores, &len, NULL, 0);
    // Safety: never let RT threads approach the core count. The watchdogd panic is RT spinners
    // owning every core the scheduler would run it on.
    if (nrt >= pcores || nrt > ncpu - 2 || nrt > 4) {
        fprintf(stderr, "refusing %d RT threads (P-cores %d, CPUs %d)\n", nrt, pcores, ncpu);
        return 2;
    }

    bucket_abs = ns2abs((uint64_t)bucket_ms * 1000000);
    spin_after_abs = ns2abs((uint64_t)spin_after * 1000000000ULL);
    burst_abs = ns2abs((uint64_t)burst_us * 1000);
    period_abs = ns2abs(16667000);
    sample_abs = ns2abs(20000);
    pri_abs = ns2abs(1000000);
    hb_every_abs = ns2abs(250000000);
    hb_park_abs = ns2abs(100000);

    if (!strcmp(mode, "calib-all")) nplain = ncpu, nrt = 0, nhog = 0, work = W_SPIN;
    else if (!strcmp(mode, "calib-bg")) nplain = 0, nrt = 0, nhog = 0, work = W_SPIN;
    else if (strcmp(mode, "run")) {
        fprintf(stderr, "unknown mode %s\n", mode);
        return 2;
    }

    uint64_t start = mach_absolute_time();
    run_start_abs = start;
    run_end_abs = start + ns2abs((uint64_t)secs * 1000000000ULL);
    pthread_t th[MAXTHR], mon;
    pthread_create(&mon, NULL, monitor_main, NULL);
    nworkers = 0;
    int nbg = !strcmp(mode, "calib-bg") ? threads : 0;
    struct { enum kind k; int n; } plan[] = {{K_RT, nrt}, {K_PLAIN, nplain}, {K_BG, nbg}, {K_HOG, nhog}};
    for (int p = 0; p < 4; p++)
        for (int j = 0; j < plan[p].n && nworkers < MAXTHR; j++) {
            worker_t *w = &workers[nworkers];
            memset(w, 0, sizeof *w);
            w->idx = nworkers;
            w->kind = plan[p].k;
            w->work = work;
            w->rt_qos = rt_qos;
            w->end_abs = run_end_abs;
            pthread_create(&th[nworkers], NULL, worker_main, w);
            nworkers++;
        }
    // Sleep past the end before joining: a thread blocked in pthread_join lends its priority to the
    // joined thread (measured: the first QOS_CLASS_BACKGROUND worker ran at 31 on P-cores while
    // main sat in pthread_join on it), which would contaminate the placement of that worker.
    mach_wait_until(run_end_abs + ns2abs(50000000));
    for (int i = 0; i < nworkers; i++) pthread_join(th[i], NULL);
    pthread_join(mon, NULL); // ends at run_end_abs by itself
    atomic_store(&stop, 1);

    static const char *kn[] = {"rt", "plain", "hog", "bg"};
    for (int i = 0; i < nworkers; i++) {
        worker_t *w = &workers[i];
        if (w->kind == K_HOG) continue;
        uint64_t e_rt = 0, p_rt = 0, e_ts = 0, p_ts = 0, e_w = 0, p_w = 0;
        for (int c = 0; c < MAXCPU; c++) {
            if (is_e[c]) e_rt += w->hist_rt[c], e_ts += w->hist_ts[c], e_w += w->wake_cpu[c];
            else p_rt += w->hist_rt[c], p_ts += w->hist_ts[c], p_w += w->wake_cpu[c];
        }
        printf("%s thr=%d kind=%s work=%s rtqos=%d ret_policy=%d ret_qos=%d pri=%d..%d rtpri_frac=%.3f "
               "E_rt=%llu P_rt=%llu E_ts=%llu P_ts=%llu wakeE=%llu wakeP=%llu",
               label, i, kn[w->kind], w->work == W_BURST ? "burst" : "spin", w->rt_qos, w->ret_policy,
               w->ret_qos, w->pri_min, w->pri_max,
               w->pri_samples ? (double)w->pri_rt_samples / w->pri_samples : 0.0, (unsigned long long)e_rt,
               (unsigned long long)p_rt, (unsigned long long)e_ts, (unsigned long long)p_ts,
               (unsigned long long)e_w, (unsigned long long)p_w);
        if (w->nlate) {
            qsort(w->late_ns, w->nlate, sizeof(uint64_t), cmp_u64);
            printf(" late_us_p50=%.1f p99=%.1f max=%.1f", w->late_ns[w->nlate / 2] / 1e3,
                   w->late_ns[w->nlate * 99 / 100] / 1e3, w->late_ns[w->nlate - 1] / 1e3);
        }
        printf(" raw_or=0x%llx raw_and=0x%llx cpuid_mismatch=%llu/%llu\n", (unsigned long long)w->raw_or,
               (unsigned long long)w->raw_and, (unsigned long long)w->mismatch, (unsigned long long)w->checks);
        printf("%s thr=%d series E_rt/P_rt/E_ts/P_ts per %dms:", label, i, bucket_ms);
        int nb = (int)((run_end_abs - run_start_abs) / bucket_abs) + 1;
        if (nb > MAXBUCKET) nb = MAXBUCKET;
        for (int b = 0; b < nb; b++)
            printf(" %u/%u/%u/%u", w->b_e_rt[b], w->b_p_rt[b], w->b_e_ts[b], w->b_p_ts[b]);
        printf("\n");
        printf("%s thr=%d hist cpu:samples(rt/ts):iters_per_us", label, i);
        for (int c = 0; c < MAXCPU; c++)
            if (w->hist_rt[c] || w->hist_ts[c])
                printf(" %d:%llu/%llu:%.0f", c, (unsigned long long)w->hist_rt[c],
                       (unsigned long long)w->hist_ts[c],
                       w->abs_cpu[c] ? w->iters_cpu[c] / (abs2ns(w->abs_cpu[c]) / 1e3) : 0.0);
        printf("\n");
    }
    printf("%s monitor intervals=%llu max_late_ms=%.1f cpu:busy%%/offline/stalled", label,
           (unsigned long long)mon_intervals, mon_late_max_ns / 1e6);
    for (int c = 0; c < ncpu; c++)
        printf(" %d:%.0f/%llu/%llu", c, mon_total[c] ? 100.0 * mon_busy[c] / mon_total[c] : -1.0,
               (unsigned long long)mon_offline[c], (unsigned long long)mon_stalled[c]);
    printf("\n");
    printf("%s recommended_cores full=%llu partial=%llu and=0x%llx or=0x%llx distinct:", label,
           (unsigned long long)rec_full, (unsigned long long)rec_partial, (unsigned long long)rec_and,
           (unsigned long long)rec_or);
    for (int k = 0; k < nrec_seen; k++) printf(" 0x%llx", (unsigned long long)rec_seen[k]);
    printf("\n");
    return 0;
}
