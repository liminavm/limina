// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Does a UNIX socket passed with SCM_RIGHTS survive while it waits in the receiver's queue?
//
//   inflight <close|keep> <delay-ms> [stream|dgram]
//
// Makes a link socketpair and a connection pair (near, far). Sends `far` down the link, then
// either closes the sender's copy of `far` (close) or holds it (keep), writes one byte on `near`,
// waits <delay-ms>, and only then receives `far` and reads from it. `near` stays open throughout.
// Prints what the read returned: "1 [a]" is the byte, "0 []" is an EOF that should not be there.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static void send_fd(int link, int fd) {
    char b = 0;
    struct iovec iov = {&b, 1};
    union { struct cmsghdr h; char buf[CMSG_SPACE(sizeof(int))]; } u;
    struct msghdr m = {0};
    m.msg_iov = &iov; m.msg_iovlen = 1;
    m.msg_control = u.buf; m.msg_controllen = sizeof u.buf;
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof fd);
    if (sendmsg(link, &m, 0) != 1) { perror("sendmsg"); exit(1); }
}

static int recv_fd(int link) {
    char b;
    struct iovec iov = {&b, 1};
    union { struct cmsghdr h; char buf[CMSG_SPACE(sizeof(int))]; } u;
    struct msghdr m = {0};
    m.msg_iov = &iov; m.msg_iovlen = 1;
    m.msg_control = u.buf; m.msg_controllen = sizeof u.buf;
    if (recvmsg(link, &m, 0) != 1) { perror("recvmsg"); exit(1); }
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    int fd = -1;
    if (c) memcpy(&fd, CMSG_DATA(c), sizeof fd);
    return fd;
}

int main(int argc, char **argv) {
    if (argc < 3) { fprintf(stderr, "usage: inflight <close|keep> <delay-ms> [stream|dgram]\n"); return 2; }
    int keep = !strcmp(argv[1], "keep");
    int delay_ms = atoi(argv[2]);
    int type = argc > 3 && !strcmp(argv[3], "dgram") ? SOCK_DGRAM : SOCK_STREAM;
    int link[2], pair[2];
    socketpair(AF_UNIX, type, 0, link);
    socketpair(AF_UNIX, SOCK_STREAM, 0, pair);
    send_fd(link[0], pair[1]);
    if (!keep) close(pair[1]);
    write(pair[0], "a", 1);
    usleep(delay_ms * 1000);
    int fd = recv_fd(link[1]);
    char buf[8] = {0};
    ssize_t n = read(fd, buf, sizeof buf - 1);
    printf("%s %d ms %s: read %zd [%s]\n", argv[1], delay_ms, type == SOCK_DGRAM ? "dgram" : "stream", n, buf);
    return 0;
}
