// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Can a supervisor hand file descriptors to a gui-domain launchd job over Mach?
//
// The worker cannot inherit anything once launchd starts it, so the supervisor has to meet the job
// by bootstrap name and pass every fd across as a fileport. Two directions for the name:
//
//   A  the supervisor bootstrap_register()s a receive right; the job bootstrap_look_up()s it,
//      sends a request carrying a reply port, and the supervisor answers with the fileport.
//   B  the job's plist declares MachServices; the job bootstrap_check_in()s the receive right,
//      and the supervisor bootstrap_look_up()s it right after `launchctl bootstrap` and sends.
//
// Either way the job turns the fileport back into an fd and writes one line into it, which the
// supervisor reads from its end of the socketpair.
//
//   rendezvous sup <A|B> <workdir>     supervisor side: writes the plist, bootstraps the job,
//                                      passes the fd, prints the line (or the failure)
//   rendezvous job <A|B> <name>        job side, started by launchd
#include <errno.h>
#include <fcntl.h>
#include <mach/mach.h>
#include <poll.h>
#include <servers/bootstrap.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fileport.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

typedef struct {
    mach_msg_header_t header;
    mach_msg_body_t body;
    mach_msg_port_descriptor_t port;
} port_msg_t;

typedef struct {
    port_msg_t msg;
    mach_msg_trailer_t trailer;
} port_msg_rcv_t;

static int send_port(mach_port_t dest, mach_port_t payload, mach_msg_type_name_t disp, mach_port_t reply) {
    port_msg_t m;
    memset(&m, 0, sizeof m);
    m.header.msgh_bits = MACH_MSGH_BITS_COMPLEX |
        MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, reply ? MACH_MSG_TYPE_MAKE_SEND_ONCE : 0);
    m.header.msgh_size = sizeof m;
    m.header.msgh_remote_port = dest;
    m.header.msgh_local_port = reply;
    m.header.msgh_id = 0x4c494d;
    m.body.msgh_descriptor_count = 1;
    m.port.name = payload;
    m.port.disposition = disp;
    m.port.type = MACH_MSG_PORT_DESCRIPTOR;
    return mach_msg(&m.header, MACH_SEND_MSG | MACH_SEND_TIMEOUT, sizeof m, 0, MACH_PORT_NULL, 5000,
                    MACH_PORT_NULL);
}

static int recv_port(mach_port_t on, port_msg_rcv_t *r, int timeout_ms) {
    memset(r, 0, sizeof *r);
    return mach_msg(&r->msg.header, MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0, sizeof *r, on, timeout_ms,
                    MACH_PORT_NULL);
}

static mach_port_t new_recv(void) {
    mach_port_t p = MACH_PORT_NULL;
    mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &p);
    mach_port_insert_right(mach_task_self(), p, p, MACH_MSG_TYPE_MAKE_SEND);
    return p;
}

static int job(char arm, const char *name) {
    printf("job: pid %d ppid %d arm %c name %s\n", getpid(), getppid(), arm, name);
    mach_port_t fileport = MACH_PORT_NULL;
    port_msg_rcv_t r;
    if (arm == 'A') {
        mach_port_t sup = MACH_PORT_NULL;
        kern_return_t kr = bootstrap_look_up(bootstrap_port, name, &sup);
        printf("job: bootstrap_look_up -> %d (%s)\n", kr, bootstrap_strerror(kr));
        if (kr) return 1;
        mach_port_t reply = new_recv();
        // The request carries a send right to our reply port as its payload.
        kr = send_port(sup, reply, MACH_MSG_TYPE_MAKE_SEND, MACH_PORT_NULL);
        printf("job: request -> %d\n", kr);
        if (kr) return 1;
        kr = recv_port(reply, &r, 10000);
        printf("job: fileport reply -> %d\n", kr);
        if (kr) return 1;
        fileport = r.msg.port.name;
    } else {
        mach_port_t svc = MACH_PORT_NULL;
        kern_return_t kr = bootstrap_check_in(bootstrap_port, name, &svc);
        printf("job: bootstrap_check_in -> %d (%s)\n", kr, bootstrap_strerror(kr));
        if (kr) return 1;
        kr = recv_port(svc, &r, 10000);
        printf("job: fileport message -> %d\n", kr);
        if (kr) return 1;
        fileport = r.msg.port.name;
    }
    int fd = fileport_makefd(fileport);
    printf("job: fileport_makefd -> %d (%s)\n", fd, fd < 0 ? strerror(errno) : "ok");
    if (fd < 0) return 1;
    dprintf(fd, "hello from job pid %d ppid %d via arm %c\n", getpid(), getppid(), arm);
    close(fd);
    fflush(stdout);
    return 0;
}

static int run(const char *cmd) {
    int rc = system(cmd);
    return WIFEXITED(rc) ? WEXITSTATUS(rc) : -1;
}

static int sup(char arm, const char *dir) {
    char exe[1024], name[128], label[128], plist[1200], cmd[4096];
    uint32_t sz = sizeof exe;
    extern int _NSGetExecutablePath(char *, uint32_t *);
    _NSGetExecutablePath(exe, &sz);
    snprintf(label, sizeof label, "dev.limina.spike.rdv.%c.%d", arm, getpid());
    snprintf(name, sizeof name, "%s.port", label);
    snprintf(plist, sizeof plist, "%s/%s.plist", dir, label);
    printf("sup: pid %d ppid %d arm %c name %s\n", getpid(), getppid(), arm, name);

    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) { perror("socketpair"); return 1; }
    fcntl(sv[0], F_SETFD, FD_CLOEXEC);
    fcntl(sv[1], F_SETFD, FD_CLOEXEC);  // only the fileport may carry it; launchctl must not inherit it
    mach_port_t fileport = MACH_PORT_NULL;
    if (fileport_makeport(sv[1], &fileport)) { perror("fileport_makeport"); return 1; }
    close(sv[1]);

    mach_port_t mine = MACH_PORT_NULL;
    if (arm == 'A') {
        mine = new_recv();
        kern_return_t kr = bootstrap_register(bootstrap_port, name, mine);
        printf("sup: bootstrap_register -> %d (%s)\n", kr, bootstrap_strerror(kr));
        if (kr) return 1;
    }

    char services[400] = "";
    if (arm == 'B')
        snprintf(services, sizeof services,
                 "  <key>MachServices</key><dict><key>%s</key><true/></dict>\n", name);
    FILE *f = fopen(plist, "w");
    fprintf(f,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" "
            "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n"
            "<plist version=\"1.0\"><dict>\n"
            "  <key>Label</key><string>%s</string>\n"
            "  <key>ProgramArguments</key><array><string>%s</string><string>job</string>"
            "<string>%c</string><string>%s</string></array>\n"
            "  <key>ProcessType</key><string>Interactive</string>\n"
            "  <key>RunAtLoad</key><true/>\n"
            "  <key>KeepAlive</key><false/>\n"
            "  <key>StandardOutPath</key><string>%s/%s.out</string>\n"
            "  <key>StandardErrorPath</key><string>%s/%s.out</string>\n"
            "%s"
            "</dict></plist>\n",
            label, exe, arm, name, dir, label, dir, label, services);
    fclose(f);
    snprintf(cmd, sizeof cmd, "launchctl bootstrap gui/%d '%s'", getuid(), plist);
    printf("sup: %s -> %d\n", cmd, run(cmd));

    kern_return_t kr;
    if (arm == 'A') {
        port_msg_rcv_t r;
        kr = recv_port(mine, &r, 10000);
        printf("sup: request from job -> %d\n", kr);
        if (!kr) {
            kr = send_port(r.msg.port.name, fileport, MACH_MSG_TYPE_MOVE_SEND, MACH_PORT_NULL);
            printf("sup: fileport reply -> %d\n", kr);
        }
    } else {
        mach_port_t svc = MACH_PORT_NULL;
        kr = bootstrap_look_up(bootstrap_port, name, &svc);
        printf("sup: bootstrap_look_up -> %d (%s)\n", kr, bootstrap_strerror(kr));
        if (!kr) {
            kr = send_port(svc, fileport, MACH_MSG_TYPE_MOVE_SEND, MACH_PORT_NULL);
            printf("sup: fileport send -> %d\n", kr);
        }
    }

    char buf[256] = {0};
    struct pollfd p = {.fd = sv[0], .events = POLLIN};
    int ok = 0;
    if (poll(&p, 1, 10000) == 1) {
        ssize_t n = read(sv[0], buf, sizeof buf - 1);
        if (n > 0) { printf("sup: RESULT OK: %s", buf); ok = 1; }
    }
    if (!ok) printf("sup: RESULT FAIL: nothing read from the job\n");
    sleep(1);
    snprintf(cmd, sizeof cmd, "launchctl bootout gui/%d/%s 2>/dev/null", getuid(), label);
    run(cmd);
    snprintf(cmd, sizeof cmd, "cat '%s/%s.out'", dir, label);
    printf("--- job output\n");
    fflush(stdout);
    run(cmd);
    return ok ? 0 : 1;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc == 4 && !strcmp(argv[1], "sup")) return sup(argv[2][0], argv[3]);
    if (argc == 4 && !strcmp(argv[1], "job")) return job(argv[2][0], argv[3]);
    fprintf(stderr, "usage: rendezvous sup <A|B> <workdir> | rendezvous job <A|B> <name>\n");
    return 2;
}
