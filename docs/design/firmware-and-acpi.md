# Our own firmware, and ACPI for the guest

Status: accepted direction. Phase 0 (ACPI on the edk2 fork) is in progress; the Rust firmware is
not started.

## Decision

1. **Replace edk2 with a firmware we write in Rust**, including the UEFI implementation stock
   bootloaders need. edk2 (`liminavm/edk2`, `ArmVirtKrun`) stays until the Rust firmware passes
   the stock floor.
2. **Describe the machine to the guest with ACPI instead of a device tree.** The VMM generates the
   tables from the same `VmResources` it builds the device tree from; the firmware only installs
   them.
3. **PCI/ECAM is a separate decision**, not part of this one: a device tree describes ECAM as well
   as ACPI does. It is listed under §Opportunities only because Windows needs it.

There is no machine-type versioning: we control every deployment, so the cutover is one step.
Parked snapshots do not survive it (restore refuses a machine whose devices changed,
`docs/hardening-backlog.md`, the snapshot device-layout rule); resume and shut them down before upgrading.

## Two kernel rules that shape everything

- **arm64 Linux uses ACPI or a device tree, never both.** It takes ACPI only when the device tree
  is empty or a stub, or `acpi=force` is given (`arch/arm64/kernel/acpi.c`, the `dt_is_stub()`
  test before `enable_acpi()`). So the firmware must **withhold the device-tree configuration
  table** when it publishes ACPI. In edk2 that is the `PlatformHasAcpi` vs `PlatformHasDeviceTree`
  choice: `FdtClientDxe` installs the FDT table only on the latter.
- **arm64 Linux finds ACPI only through the EFI configuration table.** A direct `--kernel` boot
  stays on the device tree unless the firmware loads the kernel itself (EFI stub). L1 tests and the
  `--kernel` vehicle therefore keep the device tree until the Rust firmware can boot them.

## Opportunities

### From owning the firmware (no ACPI needed)

- **Variable store we design.** Today's store is edk2's emulated layout in a mapped file with no
  fault-tolerant write (`efi-vars.md`); a torn write reformats it, and runtime `SetVariable` of a
  new variable fails with EINVAL, which blocks `bootctl` installs, host-set `BootOrder` and factory
  reset. Our store: a format limina can read and edit offline, two-slot atomic commit, Secure Boot
  ready.
- **Boot policy from the host**: boot order, one-shot boot, boot-to-recovery. The firmware reports
  boot progress and failures to the host (an unreadable ESP is an error the UI shows, not a silent
  fall-through to PXE).
- **Display from the first frame**: the firmware reads the host EDID, drives every head at native
  resolution and scale, and draws our logo. Ends the firmware-phase scale race, the 1280×800
  console pin, and secondary displays collapsing on reboot.
- **No edk2 failure classes**: DEBUG `CpuDeadLoop`, the consoleless-device-tree assert (the
  dropped PL011 we always attach), TPL bugs. libkrun's two edk2-only workarounds (unsized queues
  snapped to max, virtio-input Inactive on reset) can go.
- **Native build** with `cargo`; no container, no BaseTools.
- **Boot time**: the firmware phase is unmeasured today; L1 direct boot takes 0.3 s against ~10 s
  to login on the UKI chain. Ours is instrumented from the start, with a budget.
- **SMBIOS on every boot path**, once the firmware loads kernels itself: `--kernel` and L1 boots
  get DMI identity and Type 11 credentials.
- **Nested virt correct by construction**: EL2 timer handling and the right hypervisor-timer PPI.
- **Secure Boot and measured boot fully ours**, including the vTPM-on-SEP direction.
- **Not ACPI-gated, can ship any time**: VMGenID (`microsoft,vmgenid` binds from a device tree),
  the M9.4 reseed-on-clone item.

### From ACPI — stock tier (no guest components)

| Opportunity | Mechanism | Replaces |
|---|---|---|
| Graceful shutdown with no agent | `PNP0C0C` power button signalled through a GED (`ACPI0013`) event; logind honours it | GPIO `gpio-keys` button stock Fedora ignores; the QGA/SIGKILL ladder rungs |
| TPM on Debian | ACPI `TPM2` table + CRB (`tpm_crb`, built into both distros' kernels) | the 90 s per-boot wait on a TIS device Debian has no driver for |
| EAS energy model on a stock kernel | MADT GICC efficiency classes + `_CPC`: `cppc_cpufreq` registers an energy model from them (`populate_efficiency_class`), `topology_init_cpu_capacity_cppc()` sets capacities | the DKMS energy-model module the roadmap calls EAS's last gate; `qemu,virtual-cpufreq` |
| Native battery and AC | `PNP0C0A` + `ACPI0003` with AML over a small MMIO register block | virtio-i2c SBS battery (and stock `i2c-virtio`'s switch-root panic) |
| Thermal pressure | ACPI thermal zones | nothing today |
| Boot logo through to the desktop | BGRT (already built by `BootGraphicsResourceTableDxe`) | the blank middle of Plymouth's `bgrt` theme |
| Real vCPU hotplug | GICC "online capable" + GED; mainline selects `ACPI_HOTPLUG_CPU` wherever `ACPI_PROCESSOR && HOTPLUG_CPU` | PSCI offlining, where `nproc` lies — needs a boot test of the guest kernels we target |
| Windows on ARM | needs UEFI + ACPI + TPM2/CRB + Secure Boot (+ PCI) | — |

`platform_profile` is **not** a stock win: ACPI lifts the `acpi_disabled` gate
(`drivers/acpi/platform_profile.c`) but a provider driver must still register, and stock arm64
has none. It stays agent-driven or an enhanced-tier driver.

SPCR is for headless VMs only: it makes the kernel use the UART as a console, which forces Plymouth
into details mode. Windowed VMs get no SPCR.

### From ACPI — enhanced tier

Limina-specific ACPI devices or `_DSM` methods our kernel and drivers read without the agent
(e.g. a `platform_profile` provider).

## What ACPI changes for existing features

Driver bindings verified against kernel source.

| Today (device tree) | Under ACPI | Work |
|---|---|---|
| virtio-mmio `virtio,mmio` | `LNRO0005` | mechanical |
| PL011 `arm,pl011` | `ARMH0011` | mechanical; SPCR only when headless |
| xHCI `generic-xhci` + `dma-coherent` | `PNP0D10` + `_CCA` | mechanical |
| PL031 `rtc-pl031` | **no ACPI ID** — the guest RTC becomes `rtc-efi` | firmware runtime `GetTime`/`SetTime` and `Get/SetWakeupTime`; host-sleep sleeptime injection rides `rtc-efi` already |
| `gpio-keys` power/sleep/restart/wake | **no ACPI table** in `gpio_keys.c` | power → `PNP0C0C`, sleep → `PNP0C0E`, via GED. **KEY_WAKEUP is the M9.2 resume mechanism** (`m9.2-quiesced-snapshot.md` step 4): needs a wake-capable GED event, re-proven with the wake spike. KEY_RESTART has no ACPI button. |
| `qemu,virtual-cpufreq` | **device-tree only** | `cppc_cpufreq` with `_CPC` registers in VMM MMIO |
| `capacity-dmips-mhz` + `cpu-map` | CPPC capacities + efficiency classes | see the EAS row above |
| TIS TPM `tcg,tpm-tis-mmio` | `MSFT0101` (TIS) or CRB | add a CRB front-end to the vTPM; prefer CRB |
| virtio-i2c SBS battery | `PNP0C0A`/`ACPI0003` | new MMIO battery block |
| `display-cutouts.md` reads `/sys/firmware/devicetree/base/compatible` | absent | identify the platform from SMBIOS instead |

Interrupts keep the conventions the device tree uses on HVF: SPIs edge-triggered (the in-kernel GIC
only pulses), PPIs level.

## The UEFI implementation

A single-image firmware, `no_std` Rust for `aarch64-unknown-none`, that *provides* UEFI. No
PEI/DXE split: reset vector → MMU and stacks → parse the VMM's device tree (firmware-internal; never
handed to an ACPI guest) → drivers → boot manager → OS loader → runtime services.

**Boundary with the VMM.** The VMM owns the machine description (device tree for the firmware,
ACPI and SMBIOS for the OS) and boot policy inputs; the firmware owns UEFI. Tables arrive in a
VMM-filled region named by a device-tree node, as SMBIOS and the variable store do today.

**Surface the stock floor needs** (shim, GRUB, systemd-boot, systemd-stub, the Linux EFI stub,
Debian's installer):

- *Boot services*: pages and pool, memory map, events and timers with TPL semantics, the protocol
  database (install, handle, open, locate, device-path lookup), `LoadImage`/`StartImage`/`Exit`
  with PE/COFF relocation, `ExitBootServices`, `Stall`, watchdog, CRC32, configuration tables.
- *Runtime services*: variables (incl. `QueryVariableInfo`), time and wakeup time,
  `ResetSystem` via PSCI, `SetVirtualAddressMap`/`ConvertPointer`, monotonic count; an
  `EFI_RT_PROPERTIES_TABLE` stating what is supported.
- *Protocols*: LoadedImage (+ device path), DevicePath (+ to-text), BlockIo, DiskIo, partition
  discovery, SimpleFileSystem on FAT **with write** (systemd-boot boot counting renames files),
  SimpleTextInput/Ex, SimpleTextOutput, GOP, RNG, TCG2 with an event log whose replay matches the
  PCRs (`vtpm.md`), Memory Attribute (shim and the kernel stub apply NX through it).
- *Configuration tables*: ACPI 2.0 RSDP or FDT (per machine), SMBIOS 3, TCG final events, memory
  attributes.
- *Drivers*: virtio-mmio blk, gpu (GOP), input (ConIn; reset at `ExitBootServices`, which the USB
  HID gadget hand-off relies on — `usb-hid-keyboard.md`), rng, console; PL011; PL031; TPM
  (TIS, then CRB).

**Later**: Secure Boot (authenticated variables, db/dbx/KEK/PK, Authenticode verification in
`LoadImage`), Windows (fuller UEFI and ACPI, PCI).

**Variable store migration**: limina converts an existing edk2 store to ours on first boot under
the new firmware, so `BootOrder`, boot entries and MOK state carry over (no extra fallback boot).

**Coupling to change in the same series**:
- the display pool's "guest driver ready" detector keys on the first `GET_EDID`, relying on the
  firmware never sending one (`graphics.md`); a firmware that reads EDID breaks it;
- `test-boot.sh`'s firmware-rev check, `vm-start-preflight.md`'s firmware checks, the debug-port
  `dep.edk2` field, the fork-rebase routine and the edk2 ledger.

## Phases

0. **ACPI on the edk2 fork.** libkrun generates RSDP/XSDT/FADT/MADT/GTDT/DSDT for aarch64
   (`acpi_tables`, already a dependency for x86_64) into a reserved region below RAM and names it
   with a `libkrun,acpi-tables` device-tree node. A Krun ACPI DXE in the fork maps the region,
   chooses ACPI when the node exists, and installs the tables. Opt-in per VM. Done when a stock
   F44 guest boots EFI+venus on ACPI with every device bound and no device tree visible.
1. **ACPI parity and stock wins** on edk2: GED power and sleep buttons, the wake path, CRB TPM,
   CPPC + efficiency classes, battery/AC, `rtc-efi` wakeup time, BGRT logo. Then ACPI becomes the
   default and the device-tree machine remains only for `--kernel`/L1.
2. **Rust firmware, device-tree machine**: parity with `KRUN_EFI.gop.fd` on the floor tests
   (`boot::fedora_stock_image_efi_boots_to_userspace`, `..._renders_to_gop`), Debian LUKS, the TPM
   tests.
3. **Rust firmware, ACPI machine; edk2 retired.** Variable-store migration ships here.
4. **Owned-firmware features**: host boot policy, multi-head EDID-aware GOP, the boot-time budget,
   kernel loading for `--kernel`/L1 (ACPI everywhere), Secure Boot.

## Open questions

- Where the firmware lives: an in-tree `firmware/` workspace built by `cargo xtask` (proposed), or
  its own repository under `liminavm`.
- Its license, which needs the same counsel review as the rest of the tree before a release tag.
- The KEY_WAKEUP replacement: a GED event marked wake-capable, proven against s2idle with the
  M9.2 wake spike.
