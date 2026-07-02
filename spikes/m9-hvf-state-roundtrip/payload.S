/*
 * m9-hvf-state-roundtrip guest payload — a bare-metal arm64 EL1 vehicle that exercises
 * vCPU + GIC state broadly so the host driver (roundtrip.c) can prove/disprove that
 * Hypervisor.framework round-trips FULL state into a fresh VM (M9.0 spike #2).
 *
 * Flow (see roundtrip.c for the host side):
 *   1. stack, CPACR (FP on), VBAR (vector table below)
 *   2. fill EL1 sysregs a real kernel uses with recognizable dummies (MMU stays OFF)
 *   3. bring up the in-kernel GICv3 (redistributor wake, PPI 27 + SPI 40 enabled,
 *      ICC_PMR/IGRPEN1) — GICD/GICR bases mirror libkrun's layout
 *      (third_party/libkrun/src/devices/src/legacy/hvfgicv3.rs:89-101)
 *   4. fill x1..x28 and q0..q7 with seed-derived constants (x0 = seed from host)
 *   5. arm the EL1 virtual timer (CNTV, ~125ms), IRQs still masked
 *   6. CHECKPOINT A: write magic 0xA11CE to MMIO 0x10000000 — the host snapshots +
 *      tears the VM down + restores into a fresh VM right here
 *   7. post-restore: fold GP + SIMD + sysregs + SP into a running checksum (x29),
 *      check CNTVCT_EL0 monotonicity, unmask IRQs, take timer interrupt #1 (armed
 *      pre-snapshot — proves CNTV_CVAL/vtimer state crossed the gap), re-arm (~15ms),
 *      take interrupt #2 (armed post-restore)
 *   8. CHECKPOINT B: write final checksum + irq count + DONE magic, then spin
 *
 * Register conventions:
 *   x0        seed (never modified)
 *   x1..x23,x28  filled constants — must survive the round-trip (folded into x29)
 *   x9,x10    main-flow scratch
 *   x24..x26  IRQ-handler / late scratch
 *   x27       interrupt counter (handler-owned)
 *   x29       running checksum
 *
 * Build: build.sh (clang --target=aarch64-linux-gnu + llvm-objcopy, like
 * scripts/build-hvf-trap-probe.sh). Flat binary, loaded at 0x8000_0000, entry at base.
 */

    .equ RAM_BASE,   0x80000000
    .equ STACK_TOP,  0x80100000
    .equ SCRATCH,    0x80200000      /* RAM cell for the pre-A CNTVCT reading */
    .equ MMIO_BASE,  0x10000000
    /* libkrun in-kernel GICv3 layout: dist = MMIO_MEM_START(0x0a000000) - 64K - 128K,
     * redist above it (hvfgicv3.rs:89-90). roundtrip.c asserts HVF reports these sizes. */
    .equ GICD_BASE,  0x09fd0000
    .equ GICR_BASE,  0x09fe0000

    /* MMIO checkpoint/info offsets (host mirrors these in roundtrip.c) */
    .equ M_CKPT_A,    0x00           /* write 0xA11CE: snapshot point            */
    .equ M_FINAL_SUM, 0x08           /* final checksum                           */
    .equ M_IRQ_COUNT, 0x10           /* interrupts taken                         */
    .equ M_DONE,      0x18           /* write 0xD05E: checkpoint B / done        */
    .equ M_INTID,     0x20           /* info: intid seen by the IRQ handler      */
    .equ M_ERR,       0x28           /* fatal: sync-exception ESR / logic error  */
    .equ M_VCT_A,     0x30           /* info: CNTVCT just before checkpoint A    */
    .equ M_VCT_DELTA, 0x38           /* info: CNTVCT delta across the snapshot   */
    .equ M_VCT_FINAL, 0x40           /* info: CNTVCT at checkpoint B             */

    .section .text
    .globl _start
_start:
    /* ---- 1. stack + FP/SIMD + vectors ---- */
    ldr     x9, =STACK_TOP
    mov     sp, x9
    mov     x9, #(3 << 20)           /* CPACR_EL1.FPEN = 0b11: no FP/SIMD traps  */
    msr     cpacr_el1, x9
    isb
    adr     x9, vectors
    msr     vbar_el1, x9

    /* ---- 2. EL1 sysreg fills (dummy but readable; MMU stays OFF) ---- */
    add     x9, x0, #0x11
    msr     tpidr_el0, x9
    add     x9, x0, #0x12
    msr     tpidrro_el0, x9
    add     x9, x0, #0x13
    msr     tpidr_el1, x9
    mov     x9, #0x77
    msr     contextidr_el1, x9
    mov     x9, #0x3                 /* EL0PCTEN|EL0VCTEN — harmless             */
    msr     cntkctl_el1, x9
    ldr     x9, =0x81000000
    msr     ttbr0_el1, x9
    ldr     x9, =0x82000000
    msr     ttbr1_el1, x9
    ldr     x9, =0x00000000004404FF  /* MAIR: a few plausible attr fields        */
    msr     mair_el1, x9
    ldr     x9, =0x0000000500103519  /* TCR: plausible-ish fields, unused (MMU off) */
    msr     tcr_el1, x9
    mrs     x9, sctlr_el1            /* SCTLR tweak: set nTWI(16) + nTWE(18)     */
    orr     x9, x9, #(1 << 16)
    orr     x9, x9, #(1 << 18)
    msr     sctlr_el1, x9
    isb
    mov     x9, #(1 << 22)           /* FPCR.RMode = 0b01                        */
    msr     fpcr, x9
    mov     x9, #(1 << 27)           /* FPSR.QC                                  */
    msr     fpsr, x9

    /* ---- 3. GICv3 bring-up ---- */
    mov     x9, #7                   /* ICC_SRE_EL1: SRE|DFB|DIB                 */
    msr     S3_0_C12_C12_5, x9
    isb

    /* redistributor wake: clear GICR_WAKER.ProcessorSleep, wait !ChildrenAsleep */
    ldr     x9, =GICR_BASE
    ldr     w10, [x9, #0x14]
    bic     w10, w10, #(1 << 1)
    str     w10, [x9, #0x14]
1:  ldr     w10, [x9, #0x14]
    tbnz    w10, #2, 1b

    /* GICR SGI frame: PPIs/SGIs group 1, enable PPI 27 (EL1 virtual timer) */
    ldr     x9, =(GICR_BASE + 0x10000)
    mov     w10, #-1
    str     w10, [x9, #0x80]         /* GICR_IGROUPR0                            */
    mov     w10, #(1 << 27)
    str     w10, [x9, #0x100]        /* GICR_ISENABLER0                          */

    /* distributor: ARE | EnableGrp1; SPI 40 as a host-relay fallback lane */
    ldr     x9, =GICD_BASE
    mov     w10, #0x12
    str     w10, [x9, #0x0]          /* GICD_CTLR                                */
    ldr     w10, [x9, #0x84]         /* GICD_IGROUPR1 (intids 32..63)            */
    orr     w10, w10, #(1 << 8)
    str     w10, [x9, #0x84]
    mov     w10, #(1 << 8)
    str     w10, [x9, #0x104]        /* GICD_ISENABLER1 -> intid 40              */
    mov     x10, #0x6000
    add     x10, x9, x10
    str     xzr, [x10, #(8 * 40)]    /* GICD_IROUTER40 = affinity 0.0.0.0        */

    mov     x9, #0xff                /* ICC_PMR_EL1: lowest priority mask        */
    msr     S3_0_C4_C6_0, x9
    mov     x9, #1                   /* ICC_IGRPEN1_EL1: enable group 1          */
    msr     S3_0_C12_C12_7, x9
    isb

    /* ---- 4. fill GP + SIMD with seed-derived constants ---- */
    .irp i, 1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,28
    add     x\i, x0, #\i
    eor     x\i, x\i, x\i, lsl #\i
    .endr
    mov     x24, #0
    mov     x25, #0
    mov     x26, #0
    mov     x27, #0                  /* interrupt counter                        */
    mov     x29, #0                  /* checksum accumulator                     */

    .irp j, 0,1,2,3,4,5,6,7
    add     x9, x0, #(0x40 + \j)
    eor     x9, x9, x9, lsl #(7 + \j)
    fmov    d\j, x9                  /* q\j low 64                               */
    add     x10, x9, #0x55
    mov     v\j\().d[1], x10         /* q\j high 64                              */
    .endr

    /* ---- 5. arm CNTV timer #1 (~cntfrq/8 = 125ms), IRQs still masked ---- */
    mrs     x9, cntfrq_el0
    lsr     x9, x9, #3
    msr     cntv_tval_el0, x9
    mov     x9, #1
    msr     cntv_ctl_el0, x9

    /* stash pre-A CNTVCT in RAM (rides the snapshot) + tell the host */
    mrs     x9, cntvct_el0
    ldr     x10, =SCRATCH
    str     x9, [x10]
    ldr     x10, =MMIO_BASE
    str     x9, [x10, #M_VCT_A]

    /* ---- 6. CHECKPOINT A: the snapshot happens on this store ---- */
    ldr     x9, =0xA11CE
    str     x9, [x10, #M_CKPT_A]

    /* =====================================================================
     * Everything below runs in the FRESH VM after restore (or uninterrupted
     * in the control run). All folded values must be identical in both.
     * ===================================================================== */

    /* ---- 7a. fold surviving GP regs ---- */
    .irp i, 1,2,3,4,5,6,7,8,11,12,13,14,15,16,17,18,19,20,21,22,23,28
    add     x29, x29, x\i
    .endr
    eor     x29, x29, x29, lsl #13

    /* fold SIMD lanes */
    .irp j, 0,1,2,3,4,5,6,7
    umov    x9, v\j\().d[0]
    add     x29, x29, x9
    umov    x9, v\j\().d[1]
    add     x29, x29, x9
    .endr

    /* fold sysregs + SP + FPCR/FPSR */
    mrs     x9, tpidr_el0
    add     x29, x29, x9
    mrs     x9, tpidrro_el0
    add     x29, x29, x9
    mrs     x9, tpidr_el1
    add     x29, x29, x9
    mrs     x9, contextidr_el1
    add     x29, x29, x9
    mrs     x9, ttbr0_el1
    add     x29, x29, x9
    mrs     x9, ttbr1_el1
    add     x29, x29, x9
    mrs     x9, mair_el1
    add     x29, x29, x9
    mrs     x9, tcr_el1
    add     x29, x29, x9
    mrs     x9, vbar_el1
    add     x29, x29, x9
    mrs     x9, sctlr_el1
    add     x29, x29, x9
    mrs     x9, cntkctl_el1
    add     x29, x29, x9
    mrs     x9, fpcr
    add     x29, x29, x9
    mrs     x9, fpsr
    add     x29, x29, x9
    mov     x9, sp
    add     x29, x29, x9

    /* ---- 7b. CNTVCT must not go backwards across the gap ---- */
    ldr     x24, =SCRATCH
    ldr     x24, [x24]
    mrs     x25, cntvct_el0
    subs    x26, x25, x24
    b.hs    2f
    ldr     x25, =MMIO_BASE          /* counter went BACKWARDS: fatal            */
    ldr     x24, =0xBAD7
    str     x24, [x25, #M_ERR]
2:  ldr     x25, =MMIO_BASE
    str     x26, [x25, #M_VCT_DELTA]

    /* ---- 7c. take interrupt #1 (timer armed PRE-snapshot) ---- */
    msr     daifclr, #2
3:  cmp     x27, #1
    b.lt    3b

    /* re-arm (~cntfrq/64 = 15ms) and take interrupt #2 (armed post-restore) */
    mrs     x9, cntfrq_el0
    lsr     x9, x9, #6
    msr     cntv_tval_el0, x9
    mov     x9, #1
    msr     cntv_ctl_el0, x9
4:  cmp     x27, #2
    b.lt    4b

    /* ---- 8. CHECKPOINT B ---- */
    add     x29, x29, x27            /* fold the interrupt count                 */
    ldr     x10, =MMIO_BASE
    str     x29, [x10, #M_FINAL_SUM]
    str     x27, [x10, #M_IRQ_COUNT]
    mrs     x9, cntvct_el0
    str     x9, [x10, #M_VCT_FINAL]
    ldr     x9, =0xD05E
    str     x9, [x10, #M_DONE]
5:  b       5b

/* ---- IRQ handler: EOI, count, fold the intid, disarm the timer ---- */
irq_handler:
    mrs     x24, S3_0_C12_C12_0      /* ICC_IAR1_EL1                             */
    ldr     x25, =MMIO_BASE
    str     x24, [x25, #M_INTID]     /* info for the host log                    */
    msr     cntv_ctl_el0, xzr        /* disarm (one-shot; main flow re-arms)     */
    msr     S3_0_C12_C12_1, x24      /* ICC_EOIR1_EL1                            */
    isb
    add     x27, x27, #1
    add     x29, x29, x24            /* fold the intid (27 expected)             */
    eret

/* ---- any sync exception is fatal: report ESR and park ---- */
sync_err:
    mrs     x24, esr_el1
    ldr     x25, =MMIO_BASE
    str     x24, [x25, #M_ERR]
6:  b       6b

    .ltorg

/* ---- vector table (VBAR_EL1), 2 KiB aligned, 0x80 per entry ---- */
    .macro ventry, target
    .balign 0x80
    b       \target
    .endm

    .balign 2048
vectors:
    ventry  sync_err                 /* 0x000 sync,  current EL, SP0             */
    ventry  irq_handler              /* 0x080 IRQ,   current EL, SP0             */
    ventry  sync_err                 /* 0x100 FIQ,   current EL, SP0             */
    ventry  sync_err                 /* 0x180 SErr,  current EL, SP0             */
    ventry  sync_err                 /* 0x200 sync,  current EL, SPx             */
    ventry  irq_handler              /* 0x280 IRQ,   current EL, SPx             */
    ventry  sync_err                 /* 0x300 FIQ,   current EL, SPx             */
    ventry  sync_err                 /* 0x380 SErr,  current EL, SPx             */
    ventry  sync_err                 /* 0x400 sync,  lower EL, aarch64           */
    ventry  sync_err                 /* 0x480 IRQ,   lower EL, aarch64           */
    ventry  sync_err                 /* 0x500 FIQ,   lower EL, aarch64           */
    ventry  sync_err                 /* 0x580 SErr,  lower EL, aarch64           */
    ventry  sync_err                 /* 0x600 sync,  lower EL, aarch32           */
    ventry  sync_err                 /* 0x680 IRQ,   lower EL, aarch32           */
    ventry  sync_err                 /* 0x700 FIQ,   lower EL, aarch32           */
    ventry  sync_err                 /* 0x780 SErr,  lower EL, aarch32           */

    .globl _end
_end:
