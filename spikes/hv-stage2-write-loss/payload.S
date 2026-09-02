// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * hv-stage2-write-loss guest payload — bare-metal arm64, loaded at
 * RAM_BASE = 0x80000000, entered at offset 0 with the MMU off.
 *
 * Host-provided flags word at RAM_BASE + 0x2000:
 *   bit 0  enable the MMU with an identity map: first GiB Device (MMIO exits),
 *          RAM GiB Normal write-back cacheable — the shape a Linux guest has
 *   bit 1  DC ZVA the DATA range before filling it (init_on_alloc's clear_page)
 *   bit 2  fill with 16-byte SIMD stores (STR Qn — glibc memcpy's shape) instead of STP Xn,Xm
 *
 * Fills DATA (0x80100000, LEN bytes) with a salted pattern, then hands
 * control to the host, which may change the stage-2 mapping of DATA. The guest
 * then recounts its own pattern (guest view), refills with a second salt and
 * hands over again.
 *
 * Checkpoint protocol: MMIO stores to 0x10000000 (unmapped IPA -> exits).
 *   +0x18 CKPT0  val = salt: a fill is about to start (the host may race it)
 *   +0x00 CKPT1  val = salt (1): fill done, host inspects + applies its op
 *   +0x08 CKPT2  val = guest-counted mismatching words after the host op
 *   +0x10 CKPT3  val = salt (2): refill done, host inspects again
 *   +0x20 DONE
 *
 * Word at offset `off` = off | (salt << 32); the following word is its bitwise
 * complement, so no word is ever zero.
 */

.text
.global _start
_start:
    movz x20, #0x1000, lsl #16      // MMIO base 0x10000000
    movz x21, #0x8010, lsl #16      // DATA base 0x80100000
    movz x22, #0x4, lsl #16         // LEN 0x40000 (256 KiB)
    movz x19, #0x8000, lsl #16      // RAM base
    ldr  x23, [x19, #0x2000]        // flags

    movz x1, #0x30, lsl #16         // CPACR_EL1.FPEN = 0b11: SIMD usable at EL1
    msr  CPACR_EL1, x1
    isb

    tbz  x23, #0, mmu_done
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
mmu_done:

    tbz  x23, #1, zva_done
    mov  x3, #0
1:  add  x6, x21, x3
    dc   zva, x6
    add  x3, x3, #64
    cmp  x3, x22
    b.lt 1b
    dsb  sy
zva_done:

    movz x9, #0x1                   // salt

fill:
    str  x9, [x20, #0x18]           // CKPT0: fill starting
    mov  x3, #0
    tbnz x23, #2, 4f
1:  add  x4, x3, x9, lsl #32
    mvn  x5, x4
    add  x6, x21, x3
    stp  x4, x5, [x6]
    add  x3, x3, #16
    cmp  x3, x22
    b.lt 1b
    b    5f
4:  add  x4, x3, x9, lsl #32
    mvn  x5, x4
    add  x6, x21, x3
    mov  v0.d[0], x4
    mov  v0.d[1], x5
    str  q0, [x6]
    add  x3, x3, #16
    cmp  x3, x22
    b.lt 4b
5:  dsb  sy
    cmp  x9, #1
    b.ne 3f
    str  x9, [x20, #0x00]           // CKPT1
    b    check
3:  str  x9, [x20, #0x10]           // CKPT3
    b    done

check:
    mov  x3, #0
    mov  x7, #0
2:  add  x4, x3, x9, lsl #32
    mvn  x5, x4
    add  x6, x21, x3
    ldp  x10, x11, [x6]
    cmp  x10, x4
    cinc x7, x7, ne
    cmp  x11, x5
    cinc x7, x7, ne
    add  x3, x3, #16
    cmp  x3, x22
    b.lt 2b
    str  x7, [x20, #0x08]           // CKPT2 = guest mismatches
    movz x9, #2
    b    fill

done:
    movz x2, #0xD05E
    str  x2, [x20, #0x20]
1:  b    1b
