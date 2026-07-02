# M9.0 spike #2 — HVF full vCPU + GIC state round-trip into a fresh VM

**Date:** 2026-07-01 · macOS 26.5, Apple M1 Max, Xcode 26.4 CLT SDK.
**Question (gates M9.1/M9.2):** can Hypervisor.framework get/set ALL vCPU state + the
in-kernel GICv3 state into a **fresh VM created in the same process**, such that the guest
continues correctly — including a virtual-timer interrupt armed *before* the snapshot?

## VERDICT: GREEN

5/5 runs pass. The guest is snapshotted mid-computation at checkpoint A (registers,
sysregs, SIMD, GIC all live; CNTV timer armed and counting), the VM is fully destroyed
(`hv_vcpu_destroy` + `hv_vm_destroy`), a fresh VM+GIC+vCPU is created **in the same
process**, every piece of state is set back, and the guest continues to checkpoint B with:

| oracle | control (uninterrupted) | restored | match |
|---|---|---|---|
| final checksum (folds x1..x28 subset, q0..q7 lanes, 13 EL1 sysregs, SP, FPCR/FPSR, intids, irq count) | `0xb11500091e26ada6` | `0xb11500091e26ada6` | ✅ |
| timer interrupts taken | 2 | 2 | ✅ |
| intids seen by the guest IRQ handler | 27, 27 (EL1 vtimer PPI) | 27, 27 | ✅ |
| guest-side errors (sync exception / CNTVCT backwards) | none | none | ✅ |

Checksum is identical and deterministic across all runs. Interrupt #1 was armed
**pre-snapshot** (CNTV_CVAL ~125 ms out) and deliberately allowed to expire *inside* the
gap (host sleeps 150 ms before resuming) — it fired correctly from restored state.
Interrupt #2 was armed post-restore. Both delivered as PPI 27 through the restored GIC.

## The vehicle

- `payload.S` — flat bare-metal arm64 EL1 payload (built like `spikes/hvf-trap-probe`),
  loaded at `0x8000_0000`. Fills x1..x28 + q0..q7 from a seed, writes real-kernel EL1
  sysregs (TPIDR_EL0/RO/EL1, VBAR, SCTLR tweak, TTBR0/1, MAIR, TCR, CONTEXTIDR, CNTKCTL,
  FPCR/FPSR; MMU off), brings up the **in-kernel GICv3** at libkrun's exact layout
  (dist `0x09fd_0000`, redist `0x09fe_0000` = `MMIO_MEM_START - sizes`, per
  `hvfgicv3.rs:89-101`; sizes asserted at runtime: dist 64 KiB, redist 128 KiB),
  arms CNTV, MMIO-writes checkpoint A, then post-restore folds everything into a checksum,
  takes 2 timer IRQs, and MMIO-writes checkpoint B.
- `roundtrip.c` — host driver, direct HVF calls, ad-hoc signed with
  `com.apple.security.hypervisor`. Three sequential VMs in ONE process:
  control → snapshot-at-A → fresh-VM restore. `build.sh` builds+signs+runs
  (needs the Bash sandbox off; `sh build.sh`, log in `run.log`).

## Exact state saved + restored

- **GP/PSTATE:** X0–X30, PC (advanced +4 past the checkpoint store before saving), CPSR,
  FPCR, FPSR — all get/set `HV_SUCCESS`.
- **SIMD:** Q0–Q31 via `hv_vcpu_get/set_simd_fp_reg` — all OK.
- **EL1 sysregs: 118 of 120 probed read OK; ALL 118 accepted `hv_vcpu_set_sys_reg` post-run;
  0 readback mismatches.** The list: DBGB/DBGW VR+CR 0–15, MDCCINT, MDSCR, **MIDR, MPIDR,
  all ID_AA64\*** (PFR0/1, ZFR0, SMFR0, DFR0/1, ISAR0/1, MMFR0/1/2 — yes, ID regs accept
  set on a fresh vCPU), SCTLR, ACTLR, CPACR, TTBR0/1, TCR, all 10 pointer-auth key regs,
  SPSR_EL1, ELR_EL1, SP_EL0, SP_EL1, AFSR0/1, ESR, FAR, PAR, MAIR, AMAIR, VBAR,
  CONTEXTIDR, TPIDR_EL1, CNTKCTL, CSSELR, TPIDR_EL0, TPIDRRO_EL0, **CNTV_CTL, CNTV_CVAL**,
  CNTP_CTL, CNTP_CVAL.
  - The only 2 read failures: `CNTVOFF_EL2`, `CNTHCTL_EL2` → `HV_UNSUPPORTED` (expected —
    no nested EL2; the vtimer offset has its own API, below). `CNTP_TVAL_EL0` read fine
    but is derived from CVAL — excluded from restore by design (restoring both would
    double-apply).
  - **Named question "does any EL1 sysreg reject set post-run?" → NO.** Zero rejections.
- **vtimer:** `hv_vcpu_get/set_vtimer_offset` (0x0 here) and `get/set_vtimer_mask` — OK.
- **Pending interrupt lines:** `hv_vcpu_get/set_pending_interrupt` IRQ+FIQ (both false at
  checkpoint A; the pending timer lives in CNTV_CVAL + GIC state, not here) — OK.
- **GIC blob:** `hv_gic_state_create` works with a live (not-running) vCPU →
  **126343 bytes** (a binary plist, `bplist00…` — versioned, as the header promises).
  `hv_gic_set_state` on the fresh VM (after vCPU create + MPIDR set) → `HV_SUCCESS`, and a
  post-restore `hv_gic_state_create` readback is **byte-identical**.
- **GIC CPU interface (NOT in the blob — `hv_gic_state.h` says so explicitly):** the 11
  `hv_gic_get/set_icc_reg` regs. 10 read+wrote OK (PMR, BPR0/1, AP0R0, AP1R0, CTLR, SRE_EL1,
  IGRPEN0/1). The **only rejections in the whole spike**:
  - `ICC_RPR_EL1` set → `HV_DENIED (0xfae94007)` — running-priority is read-only status;
    benign (saved value was the idle priority 0xff; no interrupt was active at snapshot).
    Caveat for M9: snapshot at a point where no IRQ is mid-service (active, un-EOI'd), or
    verify the blob carries active-priority state — this spike didn't exercise that case.
  - `ICC_SRE_EL2` get → `HV_UNSUPPORTED` (no EL2) — expected.
- **RAM:** plain memcpy out, fresh `mmap` + memcpy + `hv_vm_map` at the same IPA. Fine.

## Other findings the M9 design needs

- **Same-process VM re-create WORKS.** `hv_vm_destroy` → `hv_vm_create` → full rebuild,
  three times in one process, all `HV_SUCCESS`. No fork/exec needed (limina's restore
  relaunches the worker anyway, but nothing forces it).
- **Teardown → rebuild → full state restore = ~12 ms** (64 MiB RAM; dominated by the
  memcpy + 126 KB GIC plist round-trip). vCPU/GIC state cost is negligible next to the
  multi-GiB RAM dump M9.1 already budgets for.
- **With the in-kernel GICv3, the EL1 vtimer PPI is delivered entirely in-kernel:
  `HV_EXIT_REASON_VTIMER_ACTIVATED` never fired** (0 exits in all phases, both timers,
  including the expired-during-gap one). The libkrun-style vtimer-mask dance is a
  userspace-GIC artifact; the in-kernel-GIC world (which M9 should standardize on) doesn't
  need it. The restored-CVAL-in-the-past case simply delivered the IRQ as soon as the
  guest unmasked DAIF.
- **Restoring the vtimer offset verbatim means guest CNTVCT jumps by the wall-clock gap**
  (guest saw +177 ms ≈ teardown 12 ms + the deliberate 150 ms sleep + slack), monotonic,
  never backwards. `hv_vcpu_set_vtimer_offset` accepts the set, so M9.1's plan — bump
  CNTVOFF by the suspend duration on restore for a continuous CLOCK_MONOTONIC — has its
  mechanism confirmed.
- **Order that worked (no other order tried, none needed):** vm create → gic create (same
  config) → map RAM → vcpu create → **set MPIDR** (redistributor↔vCPU match) →
  `hv_gic_set_state` → ICC regs → sysregs → GP/PC/CPSR/SIMD → vtimer offset+mask →
  pending lines → run.

## What this means for M9.1/M9.2

The two **ASSUMED** rows in `docs/design/m9-suspend-resume.md` §3 are now VERIFIED for the
single-vCPU case: HVF's register API round-trips the full vCPU state (including debug
regs, pauth keys, ID regs, and live timer state), and `hv_gic_state_get_data` →
`hv_gic_set_state` round-trips the in-kernel GIC byte-identically — the "highest-risk
item" holds; the userspace-GicV3 fallback is not needed. **M9.1 and M9.2 are unblocked.**

Not covered here (known deltas to productize, none look gating): multi-vCPU (per-vCPU
redistributor/ICC state — the blob format already covers all redistributors; MPIDR must be
set per-vCPU before `hv_gic_set_state`), SPIs pending at snapshot time (virtio interrupts —
the blob carries distributor pend state; unexercised), snapshot mid-IRQ-service (active
priority vs the read-only `ICC_RPR_EL1`), SME state (absent on M1 Max), and the guest MMU
being ON (TTBR/TCR/MAIR round-trip proven, translation not exercised — Linux under M9.1's
libkrun `save_state`/`restore_state` is the real test).

## Files

- `payload.S` — the guest vehicle (flat binary; constants mirror `roundtrip.c`)
- `roundtrip.c` — host driver (control / snapshot / restore phases + verdict)
- `build.sh` — build + codesign + run (`build-only` arg to skip the run)
- `hv.entitlements` — `com.apple.security.hypervisor` (as in `spikes/balloon-madvise`)
- `run.log` — a full captured GREEN run (all 118 sysreg values, GIC blob hash, verdict)
