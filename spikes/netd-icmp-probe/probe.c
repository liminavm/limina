// netd-icmp-probe: how macOS's unprivileged ICMP sockets (SOCK_DGRAM, IPPROTO_ICMP) behave, for
// limina-netd's echo NAT. Two sockets each send an echo request with their own identifier to the
// same destination, then both read for a second: does a reply carry the IP header, is the
// identifier preserved, does the kernel fill in the checksum, does each socket see only its own?
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/ip.h>
#include <netinet/ip_icmp.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static unsigned short cksum(const void *p, int n) {
    const unsigned short *w = p;
    unsigned long s = 0;
    for (; n > 1; n -= 2) s += *w++;
    if (n) s += *(const unsigned char *)w;
    s = (s >> 16) + (s & 0xffff);
    s += s >> 16;
    return (unsigned short)~s;
}

static int sock(void) {
    int s = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    if (s < 0) { perror("socket"); exit(1); }
    return s;
}

static void send_echo(int s, const char *dst, unsigned short id, int fill_cksum) {
    unsigned char pkt[16] = {0};
    struct icmp *ic = (struct icmp *)pkt;
    ic->icmp_type = ICMP_ECHO;
    ic->icmp_id = htons(id);
    ic->icmp_seq = htons(1);
    memcpy(pkt + 8, "limina!!", 8);
    if (fill_cksum) ic->icmp_cksum = cksum(pkt, sizeof pkt);
    struct sockaddr_in to = {.sin_len = sizeof to, .sin_family = AF_INET};
    inet_pton(AF_INET, dst, &to.sin_addr);
    ssize_t n = sendto(s, pkt, sizeof pkt, 0, (struct sockaddr *)&to, sizeof to);
    printf("  send id=%u cksum=%s -> %zd%s%s\n", id, fill_cksum ? "filled" : "zero", n,
           n < 0 ? " " : "", n < 0 ? strerror(errno) : "");
}

static void drain(int s, const char *name) {
    struct pollfd p = {.fd = s, .events = POLLIN};
    while (poll(&p, 1, 1000) > 0) {
        unsigned char b[256];
        struct sockaddr_in from;
        socklen_t fl = sizeof from;
        ssize_t n = recvfrom(s, b, sizeof b, 0, (struct sockaddr *)&from, &fl);
        if (n <= 0) break;
        int has_ip = (b[0] >> 4) == 4;
        int off = has_ip ? (b[0] & 0xf) * 4 : 0;
        struct icmp *ic = (struct icmp *)(b + off);
        char a[32];
        inet_ntop(AF_INET, &from.sin_addr, a, sizeof a);
        printf("  %s got %zd bytes from %s: ip-header=%s type=%d id=%u seq=%u payload=%.8s\n",
               name, n, a, has_ip ? "yes" : "no", ic->icmp_type, ntohs(ic->icmp_id),
               ntohs(ic->icmp_seq), (char *)(b + off + 8));
    }
}

int main(int argc, char **argv) {
    const char *dsts[] = {"127.0.0.1", argc > 1 ? argv[1] : "1.1.1.1"};
    for (int d = 0; d < 2; d++) {
        for (int fill = 1; fill >= 0; fill--) {
            printf("%s, checksum %s:\n", dsts[d], fill ? "filled" : "left zero");
            int a = sock(), b = sock();
            send_echo(a, dsts[d], 0x1111, fill);
            send_echo(b, dsts[d], 0x2222, fill);
            drain(a, "A");
            drain(b, "B");
            close(a);
            close(b);
        }
    }
    return 0;
}
