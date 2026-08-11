// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * balloon-unmap-fault guest payload — bare-metal arm64, MMU off, loaded at
 * RAM_BASE = 0x80000000, entered at offset 0.
 *
 * Exercises the three guest-access shapes the balloon unmap fix must heal
 * after the host hv_vm_unmaps a page the guest then touches:
 *   phase 1: simple STR (data abort, ISV=1)
 *   phase 2: BLR into the page (instruction abort, EC 0x20)
 *   phase 3: STP/LDP (data abort, ISV=0 — the memset/memcpy shape)
 *
 * Checkpoint protocol: MMIO stores to 0x10000000 (unmapped IPA -> exits).
 *   +0x00 CKPT1  host unmaps DATA/EXEC/STP pages (and MADV_FREE_REUSABLEs them)
 *   +0x08 CKPT2  val = LDR readback of DATA after the healed STR (expect 0x22)
 *   +0x10 CKPT3  val = return of the healed BLR (expect 0x33)
 *   +0x18 CKPT4  val = sum of the healed LDP pair (expect 0x99)
 *   +0x20 DONE
 */

.text
.global _start
_start:
    movz x20, #0x1000, lsl #16      // MMIO base  0x10000000
    movz x21, #0x8010, lsl #16      // DATA page  0x80100000
    movz x22, #0x8020, lsl #16      // EXEC page  0x80200000 (host plants mov x0,#0x33; ret)
    movz x23, #0x8030, lsl #16      // STP  page  0x80300000

    // phase 0: touch DATA while it is still mapped
    movz x2, #0x11
    str  x2, [x21]

    // CKPT1: host unmaps the three test pages before resuming us
    movz x2, #0x1
    str  x2, [x20, #0x00]

    // phase 1: STR to the now-unmapped DATA page, then read it back
    movz x2, #0x22
    str  x2, [x21]
    ldr  x3, [x21]
    str  x3, [x20, #0x08]

    // phase 2: execute from the now-unmapped EXEC page
    blr  x22
    str  x0, [x20, #0x10]

    // phase 3: STP (ISV=0) to the now-unmapped STP page, then LDP it back
    movz x4, #0x44
    movz x5, #0x55
    stp  x4, x5, [x23]
    ldp  x6, x7, [x23]
    add  x6, x6, x7
    str  x6, [x20, #0x18]

    movz x2, #0xD05E
    str  x2, [x20, #0x20]
1:  b    1b
