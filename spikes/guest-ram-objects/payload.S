// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * hv-ledger-marker guest payload — bare-metal arm64, MMU off, loaded at
 * RAM_BASE = 0x80000000, entered at offset 0.
 *
 * Dirties one u64 in every 16 KiB page of a 1 GiB target range (65536 pages)
 * so each page becomes guest-touched dirty, then signals DONE via an MMIO
 * store to 0x10000000 and parks.
 *
 * The target base is re-loaded from the MAILBOX (RAM_BASE + 0x10000) at the
 * top of every pass, so the host can retarget between DONEs (the double-bill
 * cells run passes over H as well as G). Single-range modes just leave the
 * mailbox at G_GPA.
 */

.text
.global _start
_start:
    movz x20, #0x1000, lsl #16      // MMIO base 0x10000000

    // Outer loop: touch every page of the target range, signal DONE, repeat
    // forever. The host decides per-DONE whether to release/retarget and
    // resume (cycle/double modes) or stop running the vCPU.
0:  movz x22, #0x8001, lsl #16      // mailbox 0x80010000
    ldr  x21, [x22]                 // target range base
    movz x2,  #0x5a5a
    movz x3,  #1, lsl #16           // 65536 pages

1:  str  x2, [x21]
    add  x21, x21, #4, lsl #12      // +16384
    subs x3, x3, #1
    b.ne 1b

    movz x2, #0xD05E
    str  x2, [x20]                  // DONE (host may release G here)
    b    0b
