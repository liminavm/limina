# Results — 2026-08-10, dev Mac (M1 Max, macOS 26.5)

The mechanism spike for the FRQ/balloon stage-2 unmap fix
(`spikes/hv-ledger-gap/RESULTS.md` round 8c: stop offering the guest pages we
told the OS we're done with; heal the guest's re-touch with a hand-rolled mmu
notifier). One run, VERDICT GREEN.

## Verdict: GREEN — unmap → fault → REUSE + remap → retry works end-to-end

A real HVF vCPU, 64 MiB RAM mapped RWX, three 16 KiB pages `hv_vm_unmap`ped +
`MADV_FREE_REUSABLE`d mid-run at the guest's request; the guest then touches
all three. Every fault healed by `MADV_FREE_REUSE` + `hv_vm_map`(RWX) from the
vCPU thread itself, mid-exit, with NO PC advance (retry semantics). All
checkpoint values correct; the healed store is visible through the host VA
(same backing, no tearing).

## Q1–Q3: every guest access shape faults with a VALID physical_address

| shape | EC | ISV | xfsc | physical_address |
|---|---|---|---|---|
| `str` (simple store) | 0x24 data abort | 1 | 0x7 | exact faulting PA |
| instruction fetch (`blr` into the page) | **0x20 instruction abort** | — | 0x7 | exact faulting PA |
| `stp` (the memset/memcpy shape) | 0x24 data abort | **0** | 0x7 | exact faulting PA |

- xfsc 0x7 = stage-2 translation fault, level 3 — the implementation's
  remappable-fault gate is `xfsc in 0b000100..=0b000111` (translation fault,
  any level).
- **EC 0x20 must be handled** — libkrun's vCPU loop currently has no arm for
  it (unknown EC → clean VM stop). A guest that reallocates a reported-free
  page as page cache and executes from it (any binary load) hits this.
- ISV=0 aborts carry no register/size info but the PA is all the handler
  needs — never interpret the ISS before checking the PA against guest RAM.

## Q4: heal-from-the-vCPU-thread works, retry is exact

`MADV_FREE_REUSE` + `hv_vm_map`(page, RWX) called between `hv_vcpu_run` calls
on the faulting vCPU's own thread succeeds; NOT advancing the PC re-executes
the faulting instruction, which then completes against the restored mapping.
The guest's read-back and the host-VA read agree (0x22) — same backing pages.

## Q5: API semantics — PRECISE bookkeeping is mandatory

| probe | result |
|---|---|
| `hv_vm_map` over an already-mapped range | **HV_ERROR** |
| `hv_vm_map` spanning a hole + live mappings | **HV_ERROR** |
| `hv_vm_map` of exactly the hole | HV_SUCCESS |
| `hv_vm_unmap` of an already-unmapped page | HV_SUCCESS |
| `hv_vm_unmap` spanning unmapped + mapped | HV_SUCCESS |
| `MADV_FREE_REUSE` on a non-reusable range | rc=0 (harmless no-op) |

⇒ **`hv_vm_map` refuses ANY overlap with a live mapping (even partial), so the
fault handler must map exactly the released ranges: the implementation needs a
precise released-GPA range set** (balloon inserts on release, fault handler
subtracts what it remaps). Blind chunk-map is impossible; blind
unmap-then-map would "work" (unmap tolerates anything) but transiently yanks
live pages from under concurrent vCPUs — rejected. `MADV_FREE_REUSE` being a
tolerant no-op on non-reusable memory means the handler can REUSE whatever it
remaps without tracking which parts were actually reclaimed.

## What this de-risks for the libkrun implementation

1. Release path: `hv_vm_unmap` (before `madvise` + `add_used`) needs no
   special ordering care with running vCPUs — a concurrent touch simply
   faults and heals.
2. Fault path: one handler keyed ONLY on "PA inside guest RAM + translation
   fault", shared by EC 0x24 and EC 0x20; cancel the PC advance; remap
   `[chunk window] ∩ [released set]` + REUSE it; livelock cap per PA
   (this probe used 8; anything recurring past the cap = fatal, clean stop).
3. Chunking: hv_vm_map cost is per-call, population is lazy — the chunk
   window bounds fault *count*, over-billing bounds come from REUSE scope.

Build/run: `./build.sh` (sandbox off; brew llvm for the bare-metal payload,
system clang + `com.apple.security.hypervisor` for the driver).
