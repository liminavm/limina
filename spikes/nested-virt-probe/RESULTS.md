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
- ~33 µs per nested exit, round trip to the L1 VMM's userspace. Not measured: the same exit
  without nesting for comparison, an L2 Linux boot, or L2 device I/O.
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

libkrun's snapshot carries exactly the round-tripping set when a vCPU has EL2
(`SNAPSHOT_EL2_SYS_REGS`, `SNAPSHOT_ICH_REGS`, `ICH_LIST_REGS` in `hvf/src/lib.rs`), and a
restore refuses a snapshot saved with EL2 into a VM without it, or the reverse.
