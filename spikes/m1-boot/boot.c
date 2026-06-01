// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// limina M1 boot spike: boot Fedora-Workstation-43.raw via EFI with a CAPTURED serial console.
//
// Links the Homebrew libkrun bottle (1.17.4) as a reality-check before the from-source
// build. Exercises the smallest M1 path: HVF + EDK2 EFI firmware + raw disk + serial
// console + the hypervisor entitlement.
//
// Console trick (no libkrun patch needed): on the firmware/EFI path libkrun creates an
// *implicit* legacy serial whose output is hardcoded to None (builder.rs:731 — the
// `io::stdout()` line is commented out), so EDK2/GRUB output is dropped. We instead
// krun_disable_implicit_console() and add our OWN serial first via
// krun_add_serial_console_default(in,out): it becomes ttyS0 (the PL011 EDK2 uses as
// ConOut) with output wired to a file descriptor we control -> visible boot.
//
// Boot flow: krun_set_firmware() loads the EDK2 blob (Payload::Firmware), EDK2 runs the
// ESP bootloader (shim/GRUB) which loads Fedora's own kernel/initramfs off the disk.
//
// Usage: boot <firmware.fd> <disk.raw> <console_out_path> <ram_mib> <readonly 0|1> <input_fifo>
//
// The serial input is read from <input_fifo> (a named pipe opened O_RDWR so it never
// sees EOF and is kqueue-pollable). Drive GRUB/login from the host with: printf ... > fifo.
//
// krun_start_enter() never returns; run under a timeout/background and read the console.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>

#define KRUN_DISK_FORMAT_RAW 0

extern int krun_set_log_level(unsigned int level);
extern int krun_create_ctx(void);
extern int krun_set_vm_config(unsigned int ctx_id, unsigned char num_vcpus, unsigned int ram_mib);
extern int krun_set_firmware(unsigned int ctx_id, const char *firmware_path);
extern int krun_add_disk2(unsigned int ctx_id, const char *block_id, const char *disk_path,
                          unsigned int disk_format, int read_only);
extern int krun_disable_implicit_console(unsigned int ctx_id);
extern int krun_add_serial_console_default(unsigned int ctx_id, int input_fd, int output_fd);
extern int krun_start_enter(unsigned int ctx_id);

#define CHECK(expr) do { \
    int _rc = (expr); \
    if (_rc < 0) { fprintf(stderr, "[spike] %s failed: rc=%d\n", #expr, _rc); return 1; } \
    else { fprintf(stderr, "[spike] %s -> %d\n", #expr, _rc); } \
} while (0)

int main(int argc, char **argv) {
    if (argc != 7) {
        fprintf(stderr, "usage: %s <firmware.fd> <disk.raw> <console_out> <ram_mib> <readonly 0|1> <input_fifo>\n", argv[0]);
        return 2;
    }
    const char *firmware = argv[1];
    const char *disk     = argv[2];
    const char *console  = argv[3];
    unsigned int ram_mib = (unsigned int)strtoul(argv[4], NULL, 10);
    int read_only        = atoi(argv[5]);
    const char *in_fifo  = argv[6];

    fprintf(stderr, "[spike] firmware=%s disk=%s console=%s ram=%u ro=%d\n",
            firmware, disk, console, ram_mib, read_only);

    // Tee serial output to both our log file and stdout so it shows in the task output.
    int out_fd = open(console, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (out_fd < 0) { perror("open console"); return 1; }

    krun_set_log_level(1); // error only — keep libkrun noise out of the serial capture

    int ctx = krun_create_ctx();
    if (ctx < 0) { fprintf(stderr, "[spike] krun_create_ctx failed: %d\n", ctx); return 1; }
    fprintf(stderr, "[spike] ctx=%d\n", ctx);

    CHECK(krun_set_vm_config((unsigned)ctx, 4, ram_mib));
    CHECK(krun_set_firmware((unsigned)ctx, firmware));
    CHECK(krun_add_disk2((unsigned)ctx, "root", disk, KRUN_DISK_FORMAT_RAW, read_only));
    // Replace the output-dropped implicit firmware serial with our own captured one.
    CHECK(krun_disable_implicit_console((unsigned)ctx));
    // The 1.17.4 bottle asserts input_fd != -1 and registers it with kqueue. Open the
    // input FIFO O_RDWR so it's kqueue-pollable and never EOFs (we hold a write ref).
    // Host drives the guest serial with: printf '...' > <in_fifo>.
    int in_fd = open(in_fifo, O_RDWR);
    if (in_fd < 0) { perror("open in_fifo"); return 1; }
    CHECK(krun_add_serial_console_default((unsigned)ctx, in_fd, out_fd));

    fprintf(stderr, "[spike] entering guest (krun_start_enter, never returns on success)...\n");
    int rc = krun_start_enter((unsigned)ctx);
    fprintf(stderr, "[spike] krun_start_enter returned %d (unexpected)\n", rc);
    return rc < 0 ? 1 : 0;
}
