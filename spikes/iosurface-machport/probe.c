// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Prove: a child can hand a NON-global IOSurface to its parent via a Mach port
// (bootstrap rendezvous + mach_msg port descriptor), the parent reconstructs it and
// reads the child's pixel, and IOSurfaceLookup(id) FAILS cross-process (hole closed).
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <spawn.h>
#include <sys/wait.h>
#include <mach/mach.h>
#include <servers/bootstrap.h>
#include <IOSurface/IOSurface.h>
#include <CoreFoundation/CoreFoundation.h>

extern char **environ;

typedef struct { mach_msg_header_t hdr; mach_msg_body_t body; mach_msg_port_descriptor_t port; mach_msg_trailer_t trailer; } msg_t;

static CFNumberRef num(int v){ return CFNumberCreate(NULL, kCFNumberIntType, &v); }

static IOSurfaceRef make_surface(int global) {
    int w=64,h=8,bpr=w*4;
    const void *keys[] = { kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfacePixelFormat, kIOSurfaceIsGlobal };
    const void *vals[] = { num(w), num(h), num(4), num(bpr), num('BGRA'), global?kCFBooleanTrue:kCFBooleanFalse };
    CFDictionaryRef d = CFDictionaryCreate(NULL, keys, vals, global?6:5, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    IOSurfaceRef s = IOSurfaceCreate(d);
    CFRelease(d);
    return s;
}

int main(int argc, char **argv) {
    if (argc >= 3 && strcmp(argv[1], "child") == 0) {
        // CHILD: look up the parent's port, create a non-global surface, send its mach port.
        mach_port_t parent = MACH_PORT_NULL;
        kern_return_t kr = bootstrap_look_up(bootstrap_port, argv[2], &parent);
        if (kr) { printf("child: look_up failed %d\n", kr); return 1; }
        IOSurfaceRef s = make_surface(0 /*non-global*/);
        printf("child: surface id=%u (non-global)\n", IOSurfaceGetID(s));
        IOSurfaceLock(s, 0, NULL);
        unsigned char *base = IOSurfaceGetBaseAddress(s);
        base[0]=0x11; base[1]=0x22; base[2]=0x33; base[3]=0x44; // a known pixel
        IOSurfaceUnlock(s, 0, NULL);
        mach_port_t sp = IOSurfaceCreateMachPort(s);
        msg_t m; memset(&m, 0, sizeof m);
        m.hdr.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0) | MACH_MSGH_BITS_COMPLEX;
        m.hdr.msgh_size = sizeof(mach_msg_header_t)+sizeof(mach_msg_body_t)+sizeof(mach_msg_port_descriptor_t);
        m.hdr.msgh_remote_port = parent;
        m.hdr.msgh_local_port = MACH_PORT_NULL;
        m.body.msgh_descriptor_count = 1;
        m.port.name = sp; m.port.disposition = MACH_MSG_TYPE_COPY_SEND; m.port.type = MACH_MSG_PORT_DESCRIPTOR;
        kr = mach_msg(&m.hdr, MACH_SEND_MSG, m.hdr.msgh_size, 0, MACH_PORT_NULL, MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL);
        printf("child: mach_msg send: %d (%s)\n", kr, mach_error_string(kr));
        return kr ? 1 : 0;
    }

    // PARENT: receive port, register, spawn child, receive the surface port, verify.
    mach_port_t recv = MACH_PORT_NULL;
    mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &recv);
    mach_port_insert_right(mach_task_self(), recv, recv, MACH_MSG_TYPE_MAKE_SEND);
    char name[80]; snprintf(name, sizeof name, "eti.noronha.limina.spike.%d", getpid());
    kern_return_t kr = bootstrap_register(bootstrap_port, name, recv);
    if (kr) { printf("parent: register failed %d\n", kr); return 1; }

    char *cargv[] = { argv[0], "child", name, NULL };
    pid_t pid; if (posix_spawn(&pid, argv[0], NULL, NULL, cargv, environ)) { perror("spawn"); return 1; }

    msg_t m; memset(&m, 0, sizeof m);
    kr = mach_msg(&m.hdr, MACH_RCV_MSG, 0, sizeof m, recv, 5000, MACH_PORT_NULL);
    printf("parent: mach_msg recv: %d (%s) desc=%d\n", kr, mach_error_string(kr), m.body.msgh_descriptor_count);
    if (kr) return 1;
    mach_port_t surf_port = m.port.name;
    IOSurfaceRef s = IOSurfaceLookupFromMachPort(surf_port);
    printf("parent: IOSurfaceLookupFromMachPort -> %p\n", (void*)s);
    if (s) {
        IOSurfaceLock(s, kIOSurfaceLockReadOnly, NULL);
        unsigned char *base = IOSurfaceGetBaseAddress(s);
        printf("parent: pixel = %02x %02x %02x %02x (expect 11 22 33 44) id=%u\n", base[0],base[1],base[2],base[3], IOSurfaceGetID(s));
        IOSurfaceUnlock(s, kIOSurfaceLockReadOnly, NULL);
        // HOLE CHECK: can a global-id lookup find this non-global surface?
        uint32_t id = IOSurfaceGetID(s);
        IOSurfaceRef byid = IOSurfaceLookup(id);
        printf("parent: IOSurfaceLookup(%u) -> %p  (NULL = hole CLOSED)\n", id, (void*)byid);
    }
    int st; waitpid(pid, &st, 0);
    return 0;
}
