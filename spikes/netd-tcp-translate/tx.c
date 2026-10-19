// Guest -> host for limina-netd: writing the guest's segments into a host socket, and what it
// costs to know how much room the socket has (the window netd can offer the guest). See
// RESULTS.md.
//
//   tx <none|sockopt|kevent> <secs> [sockbuf-bytes|0]
//
// A child process is the far end: it reads and discards. This process writes 64 KiB "guest
// segments" from an 8 MiB ring (the frame netd will really write from):
//
//   none:    write until EAGAIN, then wait for writability; no window bookkeeping.
//   sockopt: before each write, TCP_CONNECTION_INFO's tcpi_snd_sbbytes against SO_SNDBUF.
//   kevent:  the room EVFILT_WRITE reports with each event, counted down by what is written.
//
// Only this process's CPU (getrusage) is counted.

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
#include <time.h>
#include <unistd.h>

#define FRAME (64 * 1024)
#define GRING (8 << 20)

static uint64_t now_ns(void) { return clock_gettime_nsec_np(CLOCK_UPTIME_RAW); }

static void sockbuf(int fd, int opt, int bytes) {
    if (bytes > 0 && setsockopt(fd, SOL_SOCKET, opt, &bytes, sizeof bytes)) perror("setsockopt");
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: tx <none|sockopt|kevent> <secs> [sockbuf]\n");
        return 2;
    }
    const char *mode = argv[1];
    int so = !strcmp(mode, "sockopt"), ke = !strcmp(mode, "kevent");
    double secs = atof(argv[2]);
    int buf = argc > 3 ? atoi(argv[3]) : 0;

    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    if (bind(l, (void *)&a, sizeof a) || listen(l, 1) || getsockname(l, (void *)&a, &al)) return 1;
    pid_t child = fork();
    if (!child) {
        int s = socket(AF_INET, SOCK_STREAM, 0);
        sockbuf(s, SO_RCVBUF, buf);
        if (connect(s, (void *)&a, sizeof a)) _exit(1);
        static char b[1 << 20];
        for (;;)
            if (read(s, b, sizeof b) <= 0) _exit(0);
    }
    int fd = accept(l, 0, 0);
    sockbuf(fd, SO_SNDBUF, buf);
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    fcntl(fd, F_SETFL, O_NONBLOCK);
    int kq = kqueue();
    struct kevent ev, out;
    // TX_LEVEL=1 registers the write filter level-triggered instead of with EV_CLEAR.
    EV_SET(&ev, fd, EVFILT_WRITE, getenv("TX_LEVEL") ? EV_ADD : EV_ADD | EV_CLEAR, 0, 0, 0);
    kevent(kq, &ev, 1, 0, 0, 0);

    char *gring = malloc(GRING);
    memset(gring, 0x5a, GRING);
    uint64_t pos = 0, written = 0, n_write = 0, n_wait = 0, n_info = 0, n_timeout = 0, n_short = 0;
    int64_t room = 0;
    int sndbuf = 0;
    socklen_t sl = sizeof sndbuf;

    struct rusage r0, r1;
    getrusage(RUSAGE_SELF, &r0);
    uint64_t t0 = now_ns(), end = t0 + (uint64_t)(secs * 1e9);
    struct timespec ms = {0, 1000000};
    while (now_ns() < end) {
        size_t len = FRAME;
        if (so) {
            struct tcp_connection_info ci;
            socklen_t cl = sizeof ci;
            getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &sndbuf, &sl);
            getsockopt(fd, IPPROTO_TCP, TCP_CONNECTION_INFO, &ci, &cl);
            n_info++;
            room = (int64_t)sndbuf - ci.tcpi_snd_sbbytes;
        }
        if ((so || ke) && room < FRAME) {
            if (kevent(kq, 0, 0, &out, 1, &ms) == 1 && ke) room = out.data;
            n_wait++;
            continue;
        }
        ssize_t n = write(fd, gring + pos % GRING, len);
        n_write++;
        if (n <= 0) {
            int r = kevent(kq, 0, 0, &out, 1, &ms);
            if (r == 1 && ke) room = out.data;
            if (r == 0) n_timeout++;
            n_wait++;
            continue;
        }
        if ((size_t)n < len) n_short++;
        pos += n, written += n;
        room -= n;
    }
    uint64_t t1 = now_ns();
    getrusage(RUSAGE_SELF, &r1);
    kill(child, SIGKILL);
    double wall = (t1 - t0) / 1e9;
    double cpu = (r1.ru_utime.tv_sec - r0.ru_utime.tv_sec) + (r1.ru_utime.tv_usec - r0.ru_utime.tv_usec) / 1e6 +
                 (r1.ru_stime.tv_sec - r0.ru_stime.tv_sec) + (r1.ru_stime.tv_usec - r0.ru_stime.tv_usec) / 1e6;
    double gbit = written * 8 / wall / 1e9;
    getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &sndbuf, &sl);
    printf("%-7s sockbuf %5d KiB: %6.2f Gbit/s, cpu %5.1f%%, %5.1f%%/Gbit/s; %llu writes, %llu waits, "
           "%llu info calls, %llu short writes, %llu wait timeouts\n",
           mode, sndbuf >> 10, gbit, 100 * cpu / wall, 100 * cpu / wall / (gbit > 0 ? gbit : 1),
           (unsigned long long)n_write, (unsigned long long)n_wait, (unsigned long long)n_info,
           (unsigned long long)n_short, (unsigned long long)n_timeout);
    return 0;
}
