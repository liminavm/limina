// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * replayer-ipc guest payload: bare-metal arm64 at RAM_BASE = 0x80000000, entered with the MMU
 * off. The worker uses it to read and write shared memory through stage 2, so the claim "the
 * guest sees the replayer's memory" rests on a guest access, not on hv_vm_map returning success.
 *
 * MMU preamble as in spikes/userptr-remap-metal/payload.S: identity map, first GiB Device (MMIO
 * at 0x10000000 exits), 0x80000000..0xFFFFFFFF Normal write-back.
 *
 * Control block at RAM_BASE + 0x3000: +0x00 op (1 read32, 2 write32), +0x08 address, +0x10 value.
 * MMIO: +0x00 READY (wait for a command), +0x08 REPORT (the value read).
 */

.text
.global _start
_start:
    movz x20, #0x1000, lsl #16
    movz x19, #0x8000, lsl #16

    add  x0, x19, #0x1000
    movz x1, #0x701
    str  x1, [x0, #0]
    movz x1, #0x701
    movk x1, #0x4000, lsl #16
    str  x1, [x0, #8]
    movz x1, #0x705
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
    ldr  x1, [x10, #0]
    ldr  x2, [x10, #8]
    ldr  x3, [x10, #16]
    cmp  x1, #1
    b.ne 2f
    ldr  w4, [x2]
    str  x4, [x20, #0x08]           // REPORT
    b    cmdloop
2:  cmp  x1, #2
    b.ne cmdloop
    str  w3, [x2]
    dsb  sy
    b    cmdloop
