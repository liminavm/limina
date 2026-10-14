# nested-virt-probe — can a limina guest run KVM?

libkrun already has the HVF half (`hv_vm_config_set_el2_enabled`, EL2 entry state, EL2 + GICv3
advertised in `ID_AA64PFR0_EL1`: `third_party/libkrun/src/hvf/src/lib.rs`, `nested_enabled`).
limina-vmm's `--nested-virt` sets `VmResources::nested_enabled` and refuses up front where
`check_nested_virt()` says HVF has no EL2 (M1/M2).

The guest (`guest/`, static-musl PID 1) prints the kernel's kvm lines, then runs a one-vCPU KVM
VM whose code stores an incrementing counter to an unmapped address in a loop. Each store is an
MMIO exit to the probe, which checks reason, address and value and times the `KVM_RUN` round
trip. A PASS means a nested guest ran 20 000 loop iterations and each store arrived with the right value.

- `run.sh <limina-vmm> <Image> out/initramfs.cpio <workdir> [--nested-virt] [--ipa-granule 4k]` —
  direct kernel boot.
- `mkesp.sh <Image> out/initramfs.cpio <grubaa64.efi> <esp.img>`, then
  `limina-vmm --firmware KRUN_EFI.gop.fd --disk esp.img --console <log> --no-snd --no-battery
  --nested-virt` — the real boot chain: our GOP firmware → Fedora's GRUB → kernel.
- Build the guest: `cd guest && cargo build --release`, then
  `../vz27-custom-virtio/mkinitramfs.py guest/target/aarch64-unknown-linux-musl/release/kvmprobe-guest out/initramfs.cpio`.

## Measured 2026-10-07, dogfood Mac (M4 Pro, macOS 27.0.1), from `/tmp`, 2 vCPU / 1 GiB

Test kernels `target/test-guest/kernel/Image-16k` and `Image` (4 KiB pages), worker with
`--nested-virt` built from this tree.

| Boot | EL2 | kvm init | nested VM, 20 000 MMIO exits | p50 / p99 µs per exit |
|---|---|---|---|---|
| 16 KiB kernel, direct, no flag | no | `HYP mode not available` | no `/dev/kvm` | — |
| 16 KiB kernel, direct, `--nested-virt` | all CPUs | `Hyp nVHE mode initialized successfully` | PASS | 32.7 / 48.8 |
| 4 KiB kernel, direct, `--nested-virt --ipa-granule 4k` | all CPUs | nVHE initialized | PASS | 32.8 / 48.0 |
| 16 KiB kernel via firmware + GRUB, no flag | no | `HYP mode not available` | no `/dev/kvm` | — |
| 16 KiB kernel via firmware + GRUB, `--nested-virt` | all CPUs | nVHE initialized | PASS | 32.8 / 44.3 |

- The firmware (`KRUN_EFI.gop.fd`) and GRUB run unchanged at EL2; the console up to the EFI stub
  is identical with and without the flag.
- KVM runs **nVHE**, and reports `IPA Size Limit: 36 bits (Reduced IPA size, limited VM/VMM
  compatibility)`: VMMs inside the guest must ask for a ≤ 36-bit IPA space (the probe asks
  for `KVM_CAP_ARM_VM_IPA_SIZE`; one that hard-codes the 40-bit default gets `EINVAL`).
- ~33 µs per nested exit, round trip to the L1 VMM's userspace. An L2 Linux boot and its device
  I/O: see *A real L2* below.
- 4 KiB stage-2 granule and EL2 combine fine.

## EL2 state in a snapshot: which registers round-trip

`el2-regs.c` (`build-el2-regs.sh` builds, signs and runs it): with EL2 enabled, plant
distinctive values, run the vCPU at EL2 to an MMIO exit, read every EL2 / GIC-hypervisor
register, then set each into a fresh nested VM's vCPU and read it back.

Measured 2026-10-12, dogfood Mac (M4 Pro, macOS 27.0.1):

- **Every EL2 system register HVF names round-trips**, planted values included: `HCR`, `SCTLR`,
  `TCR`, `TTBR0/1`, `VTCR`, `VTTBR`, `VBAR`, `MAIR`, `ELR`, `SPSR`, `ESR`, `FAR`, `HPFAR`,
  `SP`, `TPIDR`, `CPTR`, `MDCR`, `VPIDR`, `VMPIDR`, `CNTHCTL`, `CNTHP_CTL/CVAL`, `CNTVOFF`
  (all `_EL2`). `CNTHP_TVAL_EL2` reads back a few ticks lower: it is derived from the CVAL and
  the counter, like `CNTP_TVAL_EL0`, and is not restored.
- **`CNTVOFF_EL2` is not HVF's vtimer offset.** Setting one leaves the other alone: it is the
  guest hypervisor's offset for its own guests, so a snapshot keeps both.
- **`ICC_SRE_EL2` and the ICH registers** (`HCR`, `VMCR`, `AP0R0`, `AP1R0`, `LR0-7`) round-trip.
  `ICH_VTR_EL2` reads `0x90580007`: eight list registers, and `LR8-15` read `HV_UNSUPPORTED`.
  `ICH_VTR/MISR/EISR/ELRSR_EL2` are read-only status and refuse a write with `HV_DENIED`.
  `ICH_VMCR_EL2` normalises a planted value on the run (`0xf0000003` → `0xf04c000b`) and then
  round-trips.
- The `ICV_*` registers read with EL2 on and are views of the ICH state (`ICV_AP0R0` = the
  planted `ICH_AP0R0`); `ICV_RPR_EL1` refuses a write.
- The VM-wide GIC state blob is 126314 bytes with and without EL2: no nested state in it.
- With EL2, `hv_vcpu_get_pending_interrupt` answers `HV_UNSUPPORTED`: the in-kernel GIC owns the
  vCPU's IRQ/FIQ lines. Every EL1 register of the non-nested snapshot, the ICC registers, the
  vtimer offset and mask still read.
- libkrun's binding for `MDCR_EL2` was `0xe019`; the SDK's is `0xe089` (`HV_BAD_ARGUMENT`
  otherwise). It was the only one of the 186 register ids that disagreed with the SDK.

libkrun's snapshot carries exactly the round-tripping set when a vCPU has EL2
(`SNAPSHOT_EL2_SYS_REGS`, `SNAPSHOT_ICH_REGS`, `ICH_LIST_REGS` in `hvf/src/lib.rs`), and a
restore refuses a snapshot saved with EL2 into a VM without it, or the reverse.

## A real L2: Fedora's KVM running QEMU guests

`l2/` drives it, from a limina checkout on an M3+ Mac: `l2/host-run.sh boot <disk> <log>
--nested-virt` boots a headless clone, `l2/setup.sh` (as root in the guest) builds a busybox
initramfs and an 8 MiB random disk, and `l2/boot.sh` boots the guest's own kernel as an L2 under
`qemu-system-aarch64 -accel kvm -M virt,gic-version=3,highmem=off` (the 36-bit IPA puts the virt
machine's high MMIO out of reach), with a virtio-blk disk the L2 checksums. `l2/hang-catch.sh`
loops one-vCPU L2 boots until one hangs and records the L1 KVM's view of it; `l2/l1-bench.sh`
is the L1 workload set below.

Measured 2026-10-12, dogfood Mac (M4 Pro, macOS 27.0.1), F44 enhanced test image (kernel
7.1.8-limina16k.4), 4 vCPU / 6 GiB:

- An L2 boots, reads its disk with the right checksum and powers off in 3.7-4.9 s.
- **Suspend and resume of a nested guest work**, with an L2 running across it: the L2's console
  kept ticking after the resume, its clock jumping by the suspended interval, and a fresh L2 boots
  after the resume. A managed VM records `nested_virt = true` in its suspend record and its resume
  is started with `--nested-virt`; a flat run started without the flag refuses the snapshot by
  name and keeps it.
- Offlining and re-onlining vCPUs leaves KVM working on them.

### L2 boots hang about one time in ten, in HVF's list-register delivery

Of 25 L2 boots, 2-3 never finish, at any point of the boot, on any L1 CPU, with one L2 vCPU or
two. A hung L2's vCPU thread spins at 97% CPU, executing `WFI` ~27 000 times a second. The L1
KVM's `vgic-state` shows why it never sleeps: the L2's virtual timer (PPI 27, hardware-mapped,
level) has its line high and enabled, so KVM sees an interrupt pending, re-enters the L2 at
once, and the L2 idles again without ever acknowledging it. The interrupt is in KVM's list
register; the L2 is never shown it.

With the L1 kernel booted with **`kvm-arm.vgic_v3_group1_trap=1`**, KVM traps the L2's group-1
CPU-interface accesses and emulates them from its own list-register state instead of leaving them
to HVF's emulated `ICV_*` registers: **75 of 75 L2 boots** pass (25 two-vCPU, 50 of the one-vCPU
`hang-catch.sh` loop that otherwise hangs on its first runs). That places the fault in HVF's
presentation of list registers to the nested guest; nothing in libkrun touches ICH state while
the guest runs. QEMU's HVF backend (10.2 there) has no nested support to compare against.

### What EL2 costs a guest that runs no VM

`l1-bench.sh`, the same clone booted with and without `--nested-virt` (two runs each):

| Workload | EL2 off | EL2 on |
|---|---|---|
| `perf bench sched pipe`, two CPUs (µs/op) | 5.9-6.4 | 25.0 |
| `perf bench sched pipe`, both tasks on one CPU (µs/op) | 1.06-1.11 | 4.9-5.2 |
| 3000 fork+exec (s) | 0.45-0.47 | 0.63-0.69 |
| 2000 × 1 ms sleep (s) | 2.23-2.30 | 2.59 |
| 1 GiB direct-I/O disk read (s) | 0.17-0.19 | 0.21-0.28 |
| 4 M 512-byte `dd` syscalls, 1 GiB sha256 | same | same |

A context switch costs ~5x as much with EL2, on one CPU as across two, and the L1's KVM is not
the cause: booted with `kvm-arm.mode=none` the numbers are the same (4.9-5.3 / 24.9-26.0). With
EL2, HVF also takes the guest's `WFI` itself (libkrun saw none), so the cost is inside HVF.
System-register traps reaching libkrun are rare at L1 (a handful at boot); each L2 entry and
exit costs one `CNTHCTL_EL2` read and write through libkrun, ~23 000 pairs per L2 boot.

## Timer interrupts at EL2: what the firmware can tick on

With `--nested-virt` the firmware and GRUB run at EL2. `el2-timer-irq.c`
(`build-el2-timer-irq.sh`) runs a few dozen instructions at EL2 that enable HVF's GIC and the
CPU interface, arm one timer, unmask IRQs and WFI; the IRQ vector acknowledges and reports the
INTID. A timer that does not interrupt within 500 ms is reported with its `CTL.ISTATUS` and
`GICR_ISPENDR0`.

Measured 2026-10-14, dogfood Mac (M4 Pro, macOS 27.0.1); `hv_gic_get_intid` names INTID 26 for
the EL2 physical timer:

| HCR_EL2 | CNTV, EL1 virtual (27) | CNTP, EL1 physical (30) | CNTHP, EL2 physical (26) |
|---|---|---|---|
| libkrun's (`IMO`/`FMO`/`AMO` set) | never fires: ISTATUS 0 | taken | taken |
| libkrun's + `TGE` (what the firmware runs with) | never fires | taken | taken |
| `TGE` only | never fires | pending, not taken | pending, not taken |
| `TGE` + `IMO`/`FMO`/`AMO` | never fires | taken | taken |
| `RW` only | never fires | pending, not taken | pending, not taken |

- **The EL1 virtual timer never fires at EL2.** `CNTV_CTL_EL0` never reports ISTATUS, against a
  2.7 ms deadline computed from `CNTVCT_EL0`. Linux is unaffected (its kernel runs at EL1, where
  CNTV works); the firmware's `ArmGenericTimerVirtCounterLib` drove CNTV at EL2, so it never got
  a timer interrupt, no UEFI timer event fired, and every console input driver (virtio keyboard,
  USB keyboard, serial terminal), which all poll from one, saw no keys. GRUB's countdown reads the
  counter, so the boot went on: issue #16. The firmware now uses CNTP at EL2
  (`ArmGenericTimerVirtPhyCounterLib` in the edk2 fork).
- **`TGE` alone does not route IRQs to EL2** under HVF, though architecturally it implies
  `IMO`/`FMO`/`AMO`; the interrupt stays pending and `WFI` keeps returning. The explicit bits are
  needed. The firmware's SEC (`PrePeiCore`) ORs them into the value libkrun sets, so it has them.
- libkrun's device-tree timer node names PPI 12 (INTID 28) as the hypervisor timer; HVF's is
  INTID 26. Nothing reads that entry today (Linux runs nVHE; the firmware uses CNTP).

Keys during GRUB's countdown on a clone of a stock Debian 13 VM (`--ipa-granule 4k`, firmware
built from the fork's branch): with nested virtualization on, the shipped firmware ignores them
both over serial and through `limina input` (virtio-input), and the boot reaches the LUKS prompt;
the fixed firmware moves GRUB's highlight on both paths. With it off, both firmwares take the
serial keys.
