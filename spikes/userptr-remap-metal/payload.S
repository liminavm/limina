// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * userptr-remap-metal guest payload — bare-metal arm64, loaded at
 * RAM_BASE = 0x80000000, entered at offset 0 with the MMU off.
 *
 * Turns the MMU on with an identity map (first GiB Device so MMIO exits, the
 * RAM GiB Normal write-back cacheable — the shape a Linux guest has), then runs
 * a host-driven command loop over a list of 16 KiB guest pages.
 *
 * Control block at RAM_BASE + 0x3000, written by the host before it resumes:
 *   +0x00 cmd    1 FILL, 2 CHECK, anything else DONE
 *   +0x08 salt
 *   +0x10 n      number of pages
 * Page list at RAM_BASE + 0x4000: n guest-physical addresses, 16 KiB aligned.
 *
 * The word at guest-physical address a is  a ^ (salt << 48)  — keyed on the
 * address, not on an offset, so a checker that sees the pages in any order
 * (the host alias reorders them) can compute what each word should be.
 *
 * MMIO stores to 0x10000000 (unmapped IPA -> exits):
 *   +0x00 READY   the guest waits for a command
 *   +0x08 REPORT  val = mismatching words of the last CHECK
 *   +0x20 DONE
 */

.text
.global _start
_start:
    movz x20, #0x1000, lsl #16      // MMIO base 0x10000000
    movz x19, #0x8000, lsl #16      // RAM base

    // Level-1 table at RAM_BASE + 0x1000: four 1 GiB blocks (T0SZ=32, 4 KiB granule).
    add  x0, x19, #0x1000
    movz x1, #0x701                 // 0x00000000: Device-nGnRnE (attr 0), AF, SH=inner
    str  x1, [x0, #0]
    movz x1, #0x701
    movk x1, #0x4000, lsl #16       // 0x40000000: Device too
    str  x1, [x0, #8]
    movz x1, #0x705                 // 0x80000000: Normal WB (attr 1), AF, SH=inner
    movk x1, #0x8000, lsl #16
    str  x1, [x0, #16]
    movz x1, #0x705
    movk x1, #0xC000, lsl #16       // 0xC0000000: Normal
    str  x1, [x0, #24]
    dsb  sy
    movz x1, #0xFF00                // MAIR: attr0 Device-nGnRnE, attr1 Normal WB RW-alloc
    msr  MAIR_EL1, x1
    movz x1, #0x3520                // TCR: T0SZ=32 IRGN0=ORGN0=WB SH0=inner TG0=4K
    movk x1, #0x0080, lsl #16       //      EPD1
    movk x1, #0x0002, lsl #32       //      IPS=40-bit
    msr  TCR_EL1, x1
    msr  TTBR0_EL1, x0
    tlbi vmalle1
    dsb  sy
    isb
    mrs  x1, SCTLR_EL1
    orr  x1, x1, #(1 << 0)          // M
    orr  x1, x1, #(1 << 2)          // C
    orr  x1, x1, #(1 << 12)         // I
    msr  SCTLR_EL1, x1
    isb

    movz x14, #0x4000               // page size
cmdloop:
    str  xzr, [x20, #0x00]          // READY: the host writes the control block
    add  x0, x19, #0x3000
    ldr  x1, [x0, #0]               // cmd
    ldr  x9, [x0, #8]               // salt
    ldr  x2, [x0, #16]              // n
    add  x3, x19, #0x4000           // page list
    lsl  x9, x9, #48
    cmp  x1, #1
    b.eq fill
    cmp  x1, #2
    b.eq check
    str  xzr, [x20, #0x20]          // DONE
1:  b    1b

fill:
    mov  x4, #0
1:  cmp  x4, x2
    b.ge 3f
    ldr  x5, [x3, x4, lsl #3]
    mov  x6, #0
2:  add  x7, x5, x6
    eor  x10, x7, x9
    add  x8, x7, #8
    eor  x11, x8, x9
    stp  x10, x11, [x7]
    add  x6, x6, #16
    cmp  x6, x14
    b.lt 2b
    add  x4, x4, #1
    b    1b
3:  dsb  sy
    b    cmdloop

check:
    mov  x4, #0
    mov  x12, #0
1:  cmp  x4, x2
    b.ge 3f
    ldr  x5, [x3, x4, lsl #3]
    mov  x6, #0
2:  add  x7, x5, x6
    ldp  x10, x11, [x7]
    eor  x13, x7, x9
    cmp  x10, x13
    cinc x12, x12, ne
    add  x8, x7, #8
    eor  x13, x8, x9
    cmp  x11, x13
    cinc x12, x12, ne
    add  x6, x6, #16
    cmp  x6, x14
    b.lt 2b
    add  x4, x4, #1
    b    1b
3:  str  x12, [x20, #0x08]          // REPORT
    b    cmdloop
