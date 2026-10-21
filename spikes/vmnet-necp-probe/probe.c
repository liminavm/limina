// vmnet-necp-probe: can this binary, as signed, open a TCP connection to a vmnet guest's address?
// Local Network privacy (NECP) drops ad-hoc binaries' connects with EHOSTUNREACH; this asks the
// same of other signatures. Prints one line: connected (with the first bytes the peer sent) or the
// error.
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s IP PORT\n", argv[0]);
        return 2;
    }
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_len = sizeof a, .sin_family = AF_INET,
                            .sin_port = htons((unsigned short)atoi(argv[2]))};
    inet_pton(AF_INET, argv[1], &a.sin_addr);
    fcntl(s, F_SETFL, O_NONBLOCK);
    int rc = connect(s, (struct sockaddr *)&a, sizeof a);
    if (rc < 0 && errno == EINPROGRESS) {
        struct pollfd p = {.fd = s, .events = POLLOUT};
        if (poll(&p, 1, 10000) != 1) {
            printf("%s: timed out\n", argv[0]);
            return 1;
        }
        int err = 0;
        socklen_t l = sizeof err;
        getsockopt(s, SOL_SOCKET, SO_ERROR, &err, &l);
        errno = err;
        rc = err ? -1 : 0;
    }
    if (rc < 0) {
        printf("%s: connect: errno %d (%s)\n", argv[0], errno, strerror(errno));
        return 1;
    }
    fcntl(s, F_SETFL, 0);
    char b[64] = {0};
    struct timeval tv = {.tv_sec = 5};
    setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    ssize_t n = read(s, b, sizeof b - 1);
    if (n > 0) b[strcspn(b, "\r\n")] = 0;
    printf("%s: connected; peer said \"%s\"\n", argv[0], n > 0 ? b : "");
    return 0;
}
