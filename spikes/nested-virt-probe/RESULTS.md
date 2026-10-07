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

## Known gap

Snapshot/suspend does not carry EL2 state: libkrun's vCPU save list (`SNAPSHOT_SYS_REGS`,
`SNAPSHOT_ICC_REGS` in `hvf/src/lib.rs`) deliberately excludes the `*_EL2`, `CNTHP_*`,
`ICC_SRE_EL2` and `ICH_*` registers, because they read `HV_UNSUPPORTED` without nesting. With
`--nested-virt` those registers hold live guest state (the guest's own hypervisor), so a
snapshot or suspend of a nested VM is not expected to restore correctly until the list is made
conditional on `nested_enabled`. Not tested.
