// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * Run a program in a 4 KiB-page address space: posix_spawn with the private
 * _POSIX_SPAWN_FORCE_4K_PAGES flag (xnu bsd/sys/spawn.h:69), which makes
 * load_machfile create a PMAP_CREATE_FORCE_4K_PAGES pmap and a vm_map with
 * page shift 12 (bsd/kern/mach_loader.c:732-765). Same shape as xnu's own
 * tests/vm_spawn_tool.c, minus its DEVELOPMENT-only sysctl gate.
 *
 * Usage: fourk-spawn </path/to/program> [args...]
 */

#include <spawn.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef _POSIX_SPAWN_FORCE_4K_PAGES
#define _POSIX_SPAWN_FORCE_4K_PAGES 0x1000
#endif

extern char **environ;

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s </path/to/program> [args...]\n", argv[0]);
        return 2;
    }
    posix_spawnattr_t attr;
    posix_spawnattr_init(&attr);
    int rc = posix_spawnattr_setflags(&attr, _POSIX_SPAWN_FORCE_4K_PAGES);
    if (rc) { fprintf(stderr, "posix_spawnattr_setflags: %d\n", rc); return 1; }
    pid_t pid;
    rc = posix_spawn(&pid, argv[1], NULL, &attr, &argv[1], environ);
    if (rc) { fprintf(stderr, "posix_spawn: %d\n", rc); return 1; }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFSIGNALED(status)) {
        fprintf(stderr, "child killed by signal %d\n", WTERMSIG(status));
        return 128 + WTERMSIG(status);
    }
    return WEXITSTATUS(status);
}
