// Can limina-netd read a host socket's buffer state from the events it already gets, instead of a
// getsockopt per wakeup? Compares, on a loopback TCP pair:
//   send side: TCP_CONNECTION_INFO tcpi_snd_sbbytes, SO_NWRITE, and EVFILT_WRITE's data;
//   recv side: SO_NREAD and EVFILT_READ's data.
// The peer reads nothing at first, then drains in steps. See RESULTS.md.

#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdio.h>
#include <string.h>
#include <sys/event.h>
#include <sys/socket.h>
#include <unistd.h>

static int pair(int *rx) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t al = sizeof a;
    bind(l, (void *)&a, sizeof a);
    listen(l, 1);
    getsockname(l, (void *)&a, &al);
    int tx = socket(AF_INET, SOCK_STREAM, 0);
    connect(tx, (void *)&a, sizeof a);
    *rx = accept(l, 0, 0);
    close(l);
    return tx;
}

// A fresh kqueue per question, so the answer is this fd's and this filter's; -1 = not ready.
static long ev_data(int fd, short filter) {
    int kq = kqueue();
    struct kevent ev, out;
    EV_SET(&ev, fd, filter, EV_ADD, 0, 0, 0);
    struct timespec z = {0, 0};
    long d = kevent(kq, &ev, 1, &out, 1, &z) == 1 ? (long)out.data : -1;
    close(kq);
    return d;
}

static void show(const char *when, int tx, int rx, long sent, long drained) {
    struct tcp_connection_info ci;
    socklen_t cl = sizeof ci;
    int nwrite = 0, nread = 0, sndbuf = 0, rcvbuf = 0;
    socklen_t il = sizeof(int);
    getsockopt(tx, IPPROTO_TCP, TCP_CONNECTION_INFO, &ci, &cl);
    getsockopt(tx, SOL_SOCKET, SO_NWRITE, &nwrite, &il);
    getsockopt(rx, SOL_SOCKET, SO_NREAD, &nread, &il);
    getsockopt(tx, SOL_SOCKET, SO_SNDBUF, &sndbuf, &il);
    getsockopt(rx, SOL_SOCKET, SO_RCVBUF, &rcvbuf, &il);
    printf("%-22s sent %8ld drained %8ld | tx: sbbytes %8u nwrite %8d sndbuf %8d kq-write %8ld "
           "(sndbuf-sbbytes %8ld) | rx: nread %8d rcvbuf %8d kq-read %8ld\n",
           when, sent, drained, ci.tcpi_snd_sbbytes, nwrite, sndbuf, ev_data(tx, EVFILT_WRITE),
           (long)sndbuf - ci.tcpi_snd_sbbytes, nread, rcvbuf, ev_data(rx, EVFILT_READ));
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    int rx, tx = pair(&rx);
    fcntl(tx, F_SETFL, O_NONBLOCK);
    fcntl(rx, F_SETFL, O_NONBLOCK);
    static char b[256 << 10];
    long sent = 0, drained = 0;
    show("start", tx, rx, sent, drained);
    for (int i = 0; i < 64; i++) {
        ssize_t n = write(tx, b, sizeof b);
        if (n <= 0) break;
        sent += n;
        usleep(20000);
        show("after write", tx, rx, sent, drained);
    }
    for (int i = 0; i < 6; i++) {
        ssize_t n = read(rx, b, 64 << 10);
        if (n <= 0) break;
        drained += n;
        usleep(20000);
        show("after peer read", tx, rx, sent, drained);
    }
    return 0;
}
