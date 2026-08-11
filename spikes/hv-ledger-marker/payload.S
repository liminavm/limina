// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/*
 * hv-ledger-marker guest payload — bare-metal arm64, MMU off, loaded at
 * RAM_BASE = 0x80000000, entered at offset 0.
 *
 * Dirties one u64 in every 16 KiB page of the G-range (0x90000000, 1 GiB =
 * 65536 pages) so each page becomes guest-touched dirty, then signals DONE via
 * an MMIO store to 0x10000000 and parks.
 */

.text
.global _start
_start:
    movz x20, #0x1000, lsl #16      // MMIO base 0x10000000
    movz x21, #0x9000, lsl #16      // G-range base 0x90000000
    movz x2,  #0x5a5a
    movz x3,  #1, lsl #16           // 65536 pages

1:  str  x2, [x21]
    add  x21, x21, #4, lsl #12      // +16384
    subs x3, x3, #1
    b.ne 1b

    movz x2, #0xD05E
    str  x2, [x20]                  // DONE
2:  b    2b
