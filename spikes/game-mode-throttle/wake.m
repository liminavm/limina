// Which wakes does Game Mode throttle? A windowless, worker-shaped process that measures, once a
// second, every way one of our threads can be woken or run:
//
//   fifo     a thread blocked in read() on a FIFO the bait writes its mach_absolute_time into
//            every 5 ms: event-driven delivery latency (recv - sent), independent of the
//            sender's own cadence
//   plain    mach_wait_until to a 10 ms deadline (the control; what probe.m measured)
//   kqcrit   kevent EVFILT_TIMER, 10 ms one-shot, NOTE_CRITICAL (asks to be exempt from
//            coalescing)
//   dstrict  dispatch timer, 10 ms one-shot, leeway 0, DISPATCH_TIMER_STRICT, on a
//            user-interactive queue
//   busy     a thread that never sleeps: its CPU share of wall time, and the longest stretch
//            it went without running
//   au       an AUHAL default-output unit playing silence: render-callback interval
//
//   wake --secs N --fifo PATH [--label L] [--no-audio]
//
// Lateness is in microseconds, p50/p99/max (count). Every thread stops itself after --secs.

#import <AudioToolbox/AudioToolbox.h>
#import <Foundation/Foundation.h>
#include <dispatch/dispatch.h>
#include <errno.h>
#include <fcntl.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/stat.h>
#include <unistd.h>

#define MAXS 8192

static mach_timebase_info_data_t g_tb;
static uint64_t g_end_abs;
static const char *g_label = "wake";

static uint64_t ns_to_abs(uint64_t ns) { return ns * g_tb.denom / g_tb.numer; }
static uint64_t abs_to_ns(uint64_t a) { return a * g_tb.numer / g_tb.denom; }
static uint32_t abs_to_us(uint64_t a) { return (uint32_t)(abs_to_ns(a) / 1000); }

typedef struct {
    _Atomic uint32_t n;
    uint32_t us[MAXS];
    _Atomic thread_t port;
} ring_t;

static ring_t r_fifo, r_plain, r_kq, r_disp, r_au;
static _Atomic thread_t busy_port, main_port;
static _Atomic uint64_t busy_maxgap_abs;

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
    static uint32_t tmp[MAXS];
    uint32_t n = atomic_load(&r->n);
    if (n == 0) {
        snprintf(out, len, "-/-/-(0)");
        return;
    }
    memcpy(tmp, r->us, n * sizeof(uint32_t));
    atomic_store(&r->n, 0);
    qsort(tmp, n, sizeof(uint32_t), cmp_u32);
    snprintf(out, len, "%u/%u/%u(%u)", tmp[n / 2], tmp[(n * 99) / 100], tmp[n - 1], n);
}

static int curpri(thread_t port) {
    if (port == MACH_PORT_NULL) return -1;
    struct thread_extended_info info;
    mach_msg_type_number_t count = THREAD_EXTENDED_INFO_COUNT;
    if (thread_info(port, THREAD_EXTENDED_INFO, (thread_info_t)&info, &count) != KERN_SUCCESS)
        return -1;
    return info.pth_curpri;
}

static uint64_t thread_cpu_us(thread_t port) {
    struct thread_basic_info info;
    mach_msg_type_number_t count = THREAD_BASIC_INFO_COUNT;
    if (port == MACH_PORT_NULL ||
        thread_info(port, THREAD_BASIC_INFO, (thread_info_t)&info, &count) != KERN_SUCCESS)
        return 0;
    return (uint64_t)info.user_time.seconds * 1000000 + info.user_time.microseconds +
           (uint64_t)info.system_time.seconds * 1000000 + info.system_time.microseconds;
}

// fifo: the bait writes 8-byte mach_absolute_time stamps.
static int g_fifo_fd = -1;
static void *fifo_thread(void *arg) {
    (void)arg;
    atomic_store(&r_fifo.port, mach_thread_self());
    uint64_t buf[64];
    while (mach_absolute_time() < g_end_abs) {
        ssize_t n = read(g_fifo_fd, buf, sizeof buf);
        uint64_t now = mach_absolute_time();
        if (n <= 0) {
            if (n == 0) usleep(20 * 1000); // no writer yet (or it left): poll gently
            continue;
        }
        for (ssize_t i = 0; i < n / 8; i++) ring_put(&r_fifo, abs_to_us(now > buf[i] ? now - buf[i] : 0));
    }
    return NULL;
}

static void *plain_thread(void *arg) {
    (void)arg;
    atomic_store(&r_plain.port, mach_thread_self());
    while (mach_absolute_time() < g_end_abs) {
        uint64_t target = mach_absolute_time() + ns_to_abs(10000000);
        mach_wait_until(target);
        uint64_t now = mach_absolute_time();
        ring_put(&r_plain, abs_to_us(now > target ? now - target : 0));
    }
    return NULL;
}

static void *kq_thread(void *arg) {
    (void)arg;
    atomic_store(&r_kq.port, mach_thread_self());
    int kq = kqueue();
    while (mach_absolute_time() < g_end_abs) {
        struct kevent ev;
        uint64_t target = mach_absolute_time() + ns_to_abs(10000000);
        EV_SET(&ev, 1, EVFILT_TIMER, EV_ADD | EV_ONESHOT, NOTE_NSECONDS | NOTE_CRITICAL, 10000000, NULL);
        struct kevent out;
        if (kevent(kq, &ev, 1, &out, 1, NULL) < 1) continue;
        uint64_t now = mach_absolute_time();
        ring_put(&r_kq, abs_to_us(now > target ? now - target : 0));
    }
    close(kq);
    return NULL;
}

static dispatch_source_t g_disp;
static uint64_t g_disp_target;
static void disp_arm(void) {
    g_disp_target = mach_absolute_time() + ns_to_abs(10000000);
    dispatch_source_set_timer(g_disp, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_MSEC),
                              DISPATCH_TIME_FOREVER, 0);
}
static void disp_start(void) {
    dispatch_queue_attr_t attr =
        dispatch_queue_attr_make_with_qos_class(DISPATCH_QUEUE_SERIAL, QOS_CLASS_USER_INTERACTIVE, 0);
    dispatch_queue_t q = dispatch_queue_create("wake.dstrict", attr);
    g_disp = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, DISPATCH_TIMER_STRICT, q);
    dispatch_source_set_event_handler(g_disp, ^{
      uint64_t now = mach_absolute_time();
      atomic_store(&r_disp.port, mach_thread_self());
      ring_put(&r_disp, abs_to_us(now > g_disp_target ? now - g_disp_target : 0));
      if (now < g_end_abs) disp_arm();
    });
    disp_arm();
    dispatch_resume(g_disp);
}

static void *busy_thread(void *arg) {
    (void)arg;
    atomic_store(&busy_port, mach_thread_self());
    uint64_t last = mach_absolute_time();
    volatile uint64_t sink = 0;
    while (last < g_end_abs) {
        for (int i = 0; i < 2000; i++) sink += (uint64_t)i * i;
        uint64_t now = mach_absolute_time();
        uint64_t gap = now - last;
        uint64_t prev = atomic_load(&busy_maxgap_abs);
        if (gap > prev) atomic_store(&busy_maxgap_abs, gap);
        last = now;
    }
    return NULL;
}

// AUHAL: silence, recording the interval between render callbacks.
static uint64_t g_au_last;
static OSStatus au_render(void *ref, AudioUnitRenderActionFlags *flags, const AudioTimeStamp *ts,
                          UInt32 bus, UInt32 frames, AudioBufferList *io) {
    (void)ref; (void)ts; (void)bus; (void)frames;
    uint64_t now = mach_absolute_time();
    if (atomic_load(&r_au.port) == MACH_PORT_NULL) atomic_store(&r_au.port, mach_thread_self());
    if (g_au_last) ring_put(&r_au, abs_to_us(now - g_au_last));
    g_au_last = now;
    for (UInt32 i = 0; i < io->mNumberBuffers; i++) memset(io->mBuffers[i].mData, 0, io->mBuffers[i].mDataByteSize);
    *flags |= kAudioUnitRenderAction_OutputIsSilence;
    return noErr;
}

static AudioUnit au_start(void) {
    AudioComponentDescription d = {kAudioUnitType_Output, kAudioUnitSubType_DefaultOutput,
                                   kAudioUnitManufacturer_Apple, 0, 0};
    AudioComponent c = AudioComponentFindNext(NULL, &d);
    AudioUnit u = NULL;
    if (!c || AudioComponentInstanceNew(c, &u) != noErr) return NULL;
    AURenderCallbackStruct cb = {au_render, NULL};
    AudioUnitSetProperty(u, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &cb, sizeof cb);
    if (AudioUnitInitialize(u) != noErr || AudioOutputUnitStart(u) != noErr) {
        fprintf(stderr, "%s: AUHAL start failed\n", g_label);
        return NULL;
    }
    return u;
}

int main(int argc, char **argv) {
    mach_timebase_info(&g_tb);
    atomic_store(&main_port, mach_thread_self());
    double secs = 30;
    const char *fifo = NULL;
    int audio = 1;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--secs") && i + 1 < argc) secs = atof(argv[++i]);
        else if (!strcmp(argv[i], "--fifo") && i + 1 < argc) fifo = argv[++i];
        else if (!strcmp(argv[i], "--label") && i + 1 < argc) g_label = argv[++i];
        else if (!strcmp(argv[i], "--no-audio")) audio = 0;
        else {
            fprintf(stderr, "usage: wake --secs N --fifo PATH [--label L] [--no-audio]\n");
            return 2;
        }
    }
    if (secs <= 0 || secs > 120 || !fifo) {
        fprintf(stderr, "need --fifo and --secs in (0, 120]\n");
        return 2;
    }
    g_end_abs = mach_absolute_time() + ns_to_abs((uint64_t)(secs * 1e9));
    unlink(fifo);
    if (mkfifo(fifo, 0600) != 0) {
        perror("mkfifo");
        return 1;
    }
    // O_RDWR keeps the FIFO open for reading with no writer yet, so read() blocks instead of EOF.
    g_fifo_fd = open(fifo, O_RDWR);
    if (g_fifo_fd < 0) {
        perror("open fifo");
        return 1;
    }

    pthread_t th[4];
    pthread_create(&th[0], NULL, fifo_thread, NULL);
    pthread_create(&th[1], NULL, plain_thread, NULL);
    pthread_create(&th[2], NULL, kq_thread, NULL);
    pthread_create(&th[3], NULL, busy_thread, NULL);
    disp_start();
    AudioUnit au = audio ? au_start() : NULL;

    uint64_t t0 = mach_absolute_time(), last = t0;
    uint64_t busy_cpu_last = 0;
    for (;;) {
        sleep(1);
        uint64_t now = mach_absolute_time();
        char sf[48], sp[48], sk[48], sd[48], sa[48];
        ring_stats(&r_fifo, sf, sizeof sf);
        ring_stats(&r_plain, sp, sizeof sp);
        ring_stats(&r_kq, sk, sizeof sk);
        ring_stats(&r_disp, sd, sizeof sd);
        ring_stats(&r_au, sa, sizeof sa);
        uint64_t cpu = thread_cpu_us(atomic_load(&busy_port));
        double share = 100.0 * (double)(cpu - busy_cpu_last) / (double)(abs_to_ns(now - last) / 1000);
        busy_cpu_last = cpu;
        uint64_t gap = atomic_exchange(&busy_maxgap_abs, 0);
        printf("%s t=%.0f prio main=%d fifo=%d plain=%d kq=%d disp=%d busy=%d au=%d | fifo %s | plain %s | "
               "kqcrit %s | dstrict %s | busy %.0f%% maxgap %.1fms | au %s\n",
               g_label, abs_to_ns(now - t0) / 1e9, curpri(atomic_load(&main_port)),
               curpri(atomic_load(&r_fifo.port)), curpri(atomic_load(&r_plain.port)),
               curpri(atomic_load(&r_kq.port)), curpri(atomic_load(&r_disp.port)),
               curpri(atomic_load(&busy_port)), curpri(atomic_load(&r_au.port)), sf, sp, sk, sd, share,
               abs_to_ns(gap) / 1e6, sa);
        fflush(stdout);
        last = now;
        if (now >= g_end_abs) break;
    }
    if (au) {
        AudioOutputUnitStop(au);
        AudioComponentInstanceDispose(au);
    }
    dispatch_source_cancel(g_disp);
    // The fifo reader is blocked in read(); one stamp of our own releases it.
    uint64_t stamp = mach_absolute_time();
    (void)write(g_fifo_fd, &stamp, sizeof stamp);
    for (int i = 0; i < 4; i++) pthread_join(th[i], NULL);
    close(g_fifo_fd);
    unlink(fifo);
    return 0;
}
