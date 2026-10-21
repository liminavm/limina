// Host -> guest for limina-netd: what it costs to move a host TCP stream into a guest under each
// way of holding the data the guest has not ACKed yet. See RESULTS.md.
//
//   rx <peek|retain|forget> <window-bytes> <secs> [sockbuf-bytes|0] [ack-lag-us] [drop]
//
// A child process is the far end: it writes as fast as it can into a loopback TCP connection.
// This process is netd: it reads that connection and "delivers" to a guest emulated as a lossless
// consumer, by memcpy into an 8 MiB ring in 64 KiB frames (the frame copy netd will really make).
// The guest ACKs what it was delivered `ack-lag` microseconds later, and netd keeps at most
// `window` bytes in flight between delivery and ACK.
//
//   peek:   passt's way. Nothing is buffered: each round peeks the receive queue from its head
//           (macOS has no SO_PEEK_OFF), delivers the part past what is in flight, and reads out
//           (a second copy) what the guest has ACKed.
//   retain: read once into a per-connection buffer, deliver from it, free it on the ACK.
//   forget: read straight into the guest's frame and keep nothing.
//
// `drop` loses one frame after 1 GiB: the guest's ACKs stop at it until netd resends from the
// last ACK. peek and retain can resend; forget cannot, and the stream stalls.
//
// Only this process's CPU (getrusage) is counted; the far end is a separate process.

#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#define FRAME (64 * 1024)
#define GRING (8 << 20)
#define ACKQ (1 << 16)

static uint64_t now_ns(void) { return clock_gettime_nsec_np(CLOCK_UPTIME_RAW); }

static int listener(int *port) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    if (bind(l, (void *)&a, sizeof a) || listen(l, 1) || getsockname(l, (void *)&a, &al)) {
        perror("listener");
        exit(1);
    }
    *port = ntohs(a.sin_port);
    return l;
}

static void sockbuf(int fd, int opt, int bytes) {
    if (bytes > 0 && setsockopt(fd, SOL_SOCKET, opt, &bytes, sizeof bytes)) perror("setsockopt");
}

// The far end: writes forever.
static void source(int port, int buf) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    sockbuf(s, SO_SNDBUF, buf);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port),
                            .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    if (connect(s, (void *)&a, sizeof a)) { perror("connect"); _exit(1); }
    static char b[1 << 20];
    memset(b, 0xa5, sizeof b);
    for (;;)
        if (write(s, b, sizeof b) < 0) _exit(0);
}

static char *gring;
static uint64_t gpos;
static void deliver(const char *src, size_t len) {
    while (len) {
        size_t off = gpos % GRING, n = len < GRING - off ? len : GRING - off;
        memcpy(gring + off, src, n);
        gpos += n, src += n, len -= n;
    }
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: rx <peek|retain|forget> <window> <secs> [sockbuf] [ack-lag-us] [drop]\n");
        return 2;
    }
    const char *mode = argv[1];
    int peek = !strcmp(mode, "peek"), retain = !strcmp(mode, "retain"), forget = !strcmp(mode, "forget");
    if (!peek && !retain && !forget) return 2;
    uint64_t W = strtoull(argv[2], 0, 0);
    double secs = atof(argv[3]);
    int buf = argc > 4 ? atoi(argv[4]) : 0;
    uint64_t lag = (argc > 5 ? strtoull(argv[5], 0, 0) : 100) * 1000;
    int drop = argc > 6 && atoi(argv[6]);

    int port, l = listener(&port);
    pid_t child = fork();
    if (!child) source(port, buf);
    int fd = accept(l, 0, 0);
    sockbuf(fd, SO_RCVBUF, buf);
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    fcntl(fd, F_SETFL, O_NONBLOCK);
    int kq = kqueue();
    struct kevent ev;
    EV_SET(&ev, fd, EVFILT_READ, EV_ADD, 0, 0, 0);
    kevent(kq, &ev, 1, 0, 0, 0);

    gring = malloc(GRING);
    char *hold = malloc(W + FRAME);      // retain: the stream at offset o is at hold[o % W]
    char *pbuf = malloc(W + FRAME);      // peek: the receive queue from its head
    char *scratch = malloc(W + FRAME);   // peek: where ACKed bytes are read out to
    memset(gring, 0, GRING);
    memset(hold, 0, W + FRAME);
    memset(pbuf, 0, W + FRAME);

    struct { uint64_t t, at; } *q = calloc(ACKQ, sizeof *q);
    uint64_t qh = 0, qt = 0;
    uint64_t delivered = 0, acked = 0, consumed = 0;  // stream offsets
    uint64_t lost_at = UINT64_MAX, last_progress = 0, lowat = 0;
    uint64_t n_read = 0, n_peek = 0, n_wait = 0, n_resend = 0, resent = 0;
    int stalled = 0;

    struct rusage r0, r1;
    getrusage(RUSAGE_SELF, &r0);
    uint64_t t0 = now_ns(), end = t0 + (uint64_t)(secs * 1e9);
    last_progress = t0;

    for (;;) {
        uint64_t now = now_ns();
        if (now >= end) break;

        // The guest ACKs what it got `lag` ago, never past a lost frame.
        while (qh != qt && q[qh % ACKQ].t + lag <= now) {
            uint64_t at = q[qh % ACKQ].at < lost_at ? q[qh % ACKQ].at : lost_at;
            if (at > acked) acked = at, last_progress = now;
            qh++;
        }
        // peek frees the socket's copy only now; retain and forget dropped theirs already.
        while (peek && consumed < acked) {
            ssize_t n = recv(fd, scratch, acked - consumed, 0);
            if (n <= 0) break;
            consumed += n, n_read++;
        }
        // A loss shows as ACKs that stop moving with data in flight: resend from the last ACK.
        if (lost_at != UINT64_MAX && delivered > acked && now - last_progress > 1000000) {
            if (forget) { stalled = 1; break; }
            uint64_t len = delivered - acked;
            if (peek) {
                ssize_t n = recv(fd, pbuf, len, MSG_PEEK);
                n_peek++;
                if (n > 0) deliver(pbuf, n);
            } else {
                for (uint64_t o = acked; o < delivered;) {
                    uint64_t off = o % W, n = delivered - o < W - off ? delivered - o : W - off;
                    deliver(hold + off, n);
                    o += n;
                }
            }
            n_resend++, resent += len;
            lost_at = UINT64_MAX;
            q[qt % ACKQ].t = now, q[qt % ACKQ].at = delivered, qt++;
            last_progress = now;
            continue;
        }

        uint64_t inflight = delivered - acked, room = W - inflight;
        ssize_t got = 0;
        if (room > 0) {
            uint64_t want = room < FRAME ? room : FRAME;
            if (peek) {
                uint64_t ahead = delivered - consumed;
                ssize_t n = recv(fd, pbuf, ahead + want, MSG_PEEK);
                n_peek++;
                if (n > (ssize_t)ahead) {
                    got = n - ahead;
                    deliver(pbuf + ahead, got);
                }
            } else if (retain) {
                uint64_t off = delivered % W;
                if (want > W - off) want = W - off;
                ssize_t n = recv(fd, hold + off, want, 0);
                n_read++;
                if (n > 0) got = n, deliver(hold + off, n);
            } else {
                uint64_t off = gpos % GRING;
                if (want > GRING - off) want = GRING - off;
                ssize_t n = recv(fd, gring + off, want, 0);
                n_read++;
                if (n > 0) got = n, gpos += n;
            }
        }
        if (got > 0) {
            if (drop && lost_at == UINT64_MAX && !resent && delivered >= (1ull << 30))
                lost_at = delivered;  // this frame never reaches the guest
            delivered += got;
            q[qt % ACKQ].t = now, q[qt % ACKQ].at = delivered, qt++;
            continue;
        }

        // Nothing to do: wait for data, or for the next ACK to come due.
        uint64_t wake = qh != qt ? q[qh % ACKQ].t + lag : now + 1000000;
        if (lost_at != UINT64_MAX && last_progress + 1000000 < wake) wake = last_progress + 1000001;
        uint64_t dt = wake > now ? wake - now : 0;
        struct timespec ts = {dt / 1000000000, dt % 1000000000};
        if (peek && room > 0 && lowat != delivered - consumed + 1) {
            // The queue still holds what is in flight, so readability has to mean "more than that".
            lowat = delivered - consumed + 1;
            EV_SET(&ev, fd, EVFILT_READ, EV_ADD, NOTE_LOWAT, lowat, 0);
            kevent(kq, &ev, 1, 0, 0, 0);
        }
        struct kevent out;
        if (room > 0)
            kevent(kq, 0, 0, &out, 1, &ts);
        else
            nanosleep(&ts, 0);
        n_wait++;
    }

    uint64_t t1 = now_ns();
    getrusage(RUSAGE_SELF, &r1);
    kill(child, SIGKILL);
    double wall = (t1 - t0) / 1e9;
    double cpu = (r1.ru_utime.tv_sec - r0.ru_utime.tv_sec) + (r1.ru_utime.tv_usec - r0.ru_utime.tv_usec) / 1e6 +
                 (r1.ru_stime.tv_sec - r0.ru_stime.tv_sec) + (r1.ru_stime.tv_usec - r0.ru_stime.tv_usec) / 1e6;
    double gbit = acked * 8 / wall / 1e9;
    int rb = 0;
    socklen_t rl = sizeof rb;
    getsockopt(fd, SOL_SOCKET, SO_RCVBUF, &rb, &rl);
    printf("%-6s window %7llu KiB sockbuf %5d KiB lag %3llu us%s: %6.2f Gbit/s, cpu %5.1f%%, %5.1f%%/Gbit/s; "
           "%llu reads, %llu peeks, %llu waits, %llu resends%s\n",
           mode, (unsigned long long)(W >> 10), rb >> 10, (unsigned long long)(lag / 1000), drop ? " drop" : "",
           gbit, 100 * cpu / wall, 100 * cpu / wall / (gbit > 0 ? gbit : 1), (unsigned long long)n_read,
           (unsigned long long)n_peek, (unsigned long long)n_wait, (unsigned long long)n_resend,
           stalled ? ", STALLED (nothing to resend)" : "");
    return 0;
}
