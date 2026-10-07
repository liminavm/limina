// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * doorbell-exit guest payload: bare-metal arm64, loaded at RAM_BASE = 0x80000000, entered at
 * offset 0 with the MMU off. Times guest -> VMM -> guest round trips with CNTVCT_EL0, which HVF
 * does not trap.
 *
 * MMU preamble as in spikes/userptr-remap-metal/payload.S: identity map, first GiB Device (so
 * stores to 0x10000000 exit), RAM Normal write-back (the shape a Linux guest has).
 *
 * Control block at RAM_BASE + 0x3000, written by the host before it resumes:
 *   +0x00 mode   1 store doorbell, 2 load doorbell (host supplies the value), 3 hvc #0,
 *                4 no exit (timer-read floor), 0 DONE
 *   +0x08 n      iterations
 *   +0x10 gap    CNTVCT ticks of busy-wait before each iteration
 *   +0x18 last   (guest writes) the last value a load doorbell returned
 * Results at RAM_BASE + 0x100000: n u64 round-trip times in CNTVCT ticks.
 *
 * MMIO at 0x10000000: +0x00 READY, +0x20 DONE, +0x40 store doorbell, +0x48 load doorbell.
 */

.text
.global _start
_start:
    movz x20, #0x1000, lsl #16      // MMIO base 0x10000000
    movz x19, #0x8000, lsl #16      // RAM base

    add  x0, x19, #0x1000
    movz x1, #0x701                 // 0x00000000: Device-nGnRnE (attr 0), AF, SH=inner
    str  x1, [x0, #0]
    movz x1, #0x701
    movk x1, #0x4000, lsl #16
    str  x1, [x0, #8]
    movz x1, #0x705                 // 0x80000000: Normal WB (attr 1), AF, SH=inner
    movk x1, #0x8000, lsl #16
    str  x1, [x0, #16]
    movz x1, #0x705
    movk x1, #0xC000, lsl #16
    str  x1, [x0, #24]
    dsb  sy
    movz x1, #0xFF00
    msr  MAIR_EL1, x1
    movz x1, #0x3520
    movk x1, #0x0080, lsl #16
    movk x1, #0x0002, lsl #32
    msr  TCR_EL1, x1
    msr  TTBR0_EL1, x0
    tlbi vmalle1
    dsb  sy
    isb
    mrs  x1, SCTLR_EL1
    orr  x1, x1, #(1 << 0)
    orr  x1, x1, #(1 << 2)
    orr  x1, x1, #(1 << 12)
    msr  SCTLR_EL1, x1
    isb

cmdloop:
    str  xzr, [x20, #0x00]          // READY
    add  x10, x19, #0x3000
    ldr  x1, [x10, #0]              // mode
    ldr  x2, [x10, #8]              // n
    ldr  x3, [x10, #16]             // gap ticks
    add  x4, x19, #0x100000         // results
    cbnz x1, 0f
    str  xzr, [x20, #0x20]          // DONE
1:  b    1b
0:  mov  x11, #0                    // i
iter:
    cbz  x3, 2f
    mrs  x5, CNTVCT_EL0
    add  x6, x5, x3
1:  mrs  x5, CNTVCT_EL0
    cmp  x5, x6
    b.lo 1b
2:  isb
    mrs  x7, CNTVCT_EL0
    cmp  x1, #1
    b.ne 3f
    str  x11, [x20, #0x40]
    b    9f
3:  cmp  x1, #2
    b.ne 4f
    ldr  x8, [x20, #0x48]
    b    9f
4:  cmp  x1, #3
    b.ne 9f
    hvc  #0
9:  isb
    mrs  x9, CNTVCT_EL0
    sub  x9, x9, x7
    str  x9, [x4, x11, lsl #3]
    add  x11, x11, #1
    cmp  x11, x2
    b.lo iter
    str  x8, [x10, #24]
    dsb  sy
    b    cmdloop
