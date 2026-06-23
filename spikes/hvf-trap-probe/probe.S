/*
 * hvf-trap-probe — a minimal bare-metal arm64 "kernel" that exercises libkrun's HVF
 * PSCI/exception handling, used as the deterministic vehicle for the panic->graceful test
 * (crates/limina-test/tests/hvf_graceful.rs).
 *
 * It issues an UNMODELED PSCI function (PSCI_FEATURES, 0x8400000A — which libkrun does not
 * implement) and then a standard PSCI SYSTEM_OFF (0x84000008):
 *
 *   - Before the fix, the unmodeled HVC panics the worker process (SIGABRT) — the VM never
 *     powers off and the supervisor reports an abnormal exit.
 *   - After the fix, libkrun returns PSCI_RET_NOT_SUPPORTED, the guest falls through to
 *     SYSTEM_OFF, and the worker exits cleanly (0). The worker also logs
 *     "unhandled PSCI/SMC function 0x8400000a; returning NOT_SUPPORTED".
 *
 * Position-independent and self-contained (no relocations, no data deps, ignores the FDT in
 * x0), so it runs at whatever address libkrun loads it. The arm64 Image header
 * (Documentation/arm64/booting.rst) lets libkrun's linux_loader accept it as a raw Image,
 * exactly like the libkrunfw kernel the L1 guest reuses.
 *
 * Build: scripts/build-hvf-trap-probe.sh  ->  target/hvf-trap-probe/Image
 */
    .section .text
    .globl _start
_start:
    b       entry                 /* code0: branch to the kernel entry                  */
    .long   0                     /* code1 (reserved)                                   */
    .quad   0                     /* text_offset: 0 (we are position-independent)       */
    .quad   _end - _start         /* image_size (resolved at assembly time, same section)*/
    .quad   0                     /* flags: LE, 4 KiB pages                              */
    .quad   0                     /* res2                                               */
    .quad   0                     /* res3                                               */
    .quad   0                     /* res4                                               */
    .byte   0x41, 0x52, 0x4d, 0x64 /* magic "ARM\x64" (0x644d5241) at offset 56         */
    .long   0                     /* res5: PE header offset (0 = none)                  */
entry:
    /* x0 = FDT address (ignored). Probe an unmodeled PSCI function id. */
    movz    x0, #0x000a
    movk    x0, #0x8400, lsl #16  /* x0 = 0x8400000A (PSCI_FEATURES, unmodeled)         */
    hvc     #0                    /* pre-fix: panics the VMM; post-fix: NOT_SUPPORTED   */

    /* Clean shutdown via PSCI SYSTEM_OFF so the worker exits 0 (the GREEN oracle). */
    movz    x0, #0x0008
    movk    x0, #0x8400, lsl #16  /* x0 = 0x84000008 (PSCI SYSTEM_OFF)                  */
    hvc     #0

halt:
    wfi
    b       halt
    .globl _end
_end:
