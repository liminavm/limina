# Our own firmware, and ACPI for the guest

Status: the firmware is Cardea (`github.com/liminavm/cardea`, vendored at `third_party/cardea`),
and edk2 is retired. ACPI is the default machine (`hardware.machine`, `--machine`); the device-tree
machine remains a per-VM choice and is what a `--kernel` boot always gets.

## Decision

1. **Replace edk2 with a firmware we write in Rust**, including the UEFI implementation stock
   bootloaders need: Cardea, its own repository under `liminavm`, MIT-licensed.
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
| Graceful shutdown with no agent | `PNP0C0C` power button signalled through the PL061's ACPI GPIO events (done; a stopped Fedora 44 guest powers off in 1-3 s) | the `gpio-keys` power button, which keeps this working on the device-tree machine |
| TPM on Debian | ACPI `TPM2` table + CRB (`tpm_crb`, built into both distros' kernels) | the 90 s per-boot wait on a TIS device Debian has no driver for |
| EAS energy model on a stock kernel | MADT GICC efficiency classes + `_CPC` (done, below): `cppc_cpufreq` registers an energy model from them (`populate_efficiency_class`), `topology_init_cpu_capacity_cppc()` sets capacities | the DKMS energy-model module the roadmap calls EAS's last gate; `qemu,virtual-cpufreq` |
| Native battery and AC | `PNP0C0A` + `ACPI0003` with AML over a small MMIO register block | virtio-i2c SBS battery (and stock `i2c-virtio`'s switch-root panic) |
| Thermal pressure | ACPI thermal zones | nothing today |
| Boot logo through to the desktop | BGRT (already built by `BootGraphicsResourceTableDxe`) | the blank middle of Plymouth's `bgrt` theme |
| Real vCPU hotplug | GICC "online capable" + GED; mainline selects `ACPI_HOTPLUG_CPU` wherever `ACPI_PROCESSOR && HOTPLUG_CPU` | PSCI offlining, where `nproc` lies — needs a boot test of the guest kernels we target |
| Windows on ARM | needs UEFI + ACPI + TPM2/CRB + Secure Boot (+ PCI) | — |

`platform_profile` is **not** a stock win: ACPI lifts the `acpi_disabled` gate
(`drivers/acpi/platform_profile.c`) but a provider driver must still register, and stock arm64
has none. It stays agent-driven or an enhanced-tier driver.

No ACPI table gives a guest a console on both the display and the serial line, as the device
tree's `stdout-path` does: an SPCR is parsed in `setup_arch`, before the display's console
registers, so it becomes the preferred console and the display's is never enabled (printk's
`register_console` enables a default only while no console is preferred). So the ACPI machine's
serial console is a setting, `hardware.serial_console` / `--serial-console`, off by default:
`both` hands the firmware `console=ttyAMA0 console=tty0 plymouth.ignore-serial-consoles` in
`/chosen/libkrun,extra-bootargs`, which Cardea appends to a Linux kernel's load options at
`StartImage` (the display stays `/dev/console` and keeps the splash; the serial line gets the
kernel's console and a getty); `only` writes an SPCR, for a guest whose display is no use. Off,
the serial line carries only the firmware's and the loader's output.

### From ACPI — enhanced tier

Limina-specific ACPI devices or `_DSM` methods our kernel and drivers read without the agent
(e.g. a `platform_profile` provider).

## What ACPI changes for existing features

Driver bindings verified against kernel source.

| Today (device tree) | Under ACPI | Work |
|---|---|---|
| `/cpus` nodes | MADT GICC per vCPU **plus** an `ACPI0007` processor device per vCPU (`_UID` = the GICC's processor UID) and a PPTT | without the processor devices Linux registers no CPU devices: topology/cacheinfo WARN, lscpu shows 0 sockets, qemu-ga's vCPU calls abort |
| virtio-mmio `virtio,mmio` | `LNRO0005` | mechanical |
| PL011 `arm,pl011` | `ARMH0011` | done; a console on it only on request (`serial_console`, above) |
| xHCI `generic-xhci` + `dma-coherent` | `PNP0D10` + `_CCA` | mechanical |
| PL031 `rtc-pl031` | **no ACPI ID** — the guest keeps only `rtc-efi` (its `rtc0` on both machines; the device tree adds the PL031 as `rtc1`) | firmware runtime `GetTime`/`SetTime` and `Get/SetWakeupTime`; host-sleep sleeptime injection rides `rtc-efi` already |
| `gpio-keys` power/sleep/restart/wake | **no ACPI table** in `gpio_keys.c` | **done**: the PL061 (`ARMH0061`) with an `_AEI` listing lines 3, 4 and 6 as GpioInt events; `_E03`/`_E04` notify `PNP0C0C`/`PNP0C0E` (0x80), line 6 is `ExclusiveAndWake` and `_E06` notifies the sleep button with 0x02. The same eventfds drive it, so the M9.2 wake works in s2idle (the guest's own wake IRQ) and deep. KEY_RESTART has no ACPI button. Check: `spikes/acpi-guest/buttons.sh` |
| `qemu,virtual-cpufreq` + `capacity-dmips-mhz` | `_CPC` revision 3 per processor device + MADT GICC efficiency classes, emitted only when some vCPUs are little | **done**: highest = nominal = the vCPU's capacity, lowest = lowest nonlinear at the ladder's bottom step; desired, delivered and reference are SystemMemory registers in the vCPU's window of the cpufreq device (reference counts wall-clock ns, delivered falls behind it by the share desired stood below highest). `\_SB._OSC` must grant CPC, CPCv2 and flexible address space, or arm64 rejects SystemMemory registers; registers declare their real width, since Linux masks a read with `GENMASK(bit_width - 1, 0)`. No `_PSD`: Linux writes only a domain's leader under SW_ANY, so each vCPU is its own policy. A stock F44 guest gets `cppc_cpufreq`, the capacities and an energy-model perf domain per vCPU, and so EAS. Linux turns EAS off once the online CPUs are all one capacity, which is why hotplug offlines big vCPUs first. The counters cost −0.34 % of a fully busy guest on Linux 7.2+ and −0.88 % on 6.19 (`spikes/acpi-cppc-cost/RESULTS.md`); FFH is out, since HVF exposes no AMU and an FFH entry without one makes Linux reject the whole `_CPC` |
| TIS TPM `tcg,tpm-tis-mmio` | `MSFT0101` + a TPM2 table, start method 6 | **done**: no interrupt in `_CRS` (tpm_tis polls, as on the device tree); `tpm_tis` refuses `MSFT0101` without the TPM2 table. CRB stays optional |
| virtio-i2c SBS battery | an I2C client of the virtio-i2c adapter | **done**: `BAT0` inside the adapter's `LNRO0005` device, `I2cSerialBusV2` at 0x0b naming it, `PRP0001` + `_DSD` compatible `sbs,sbs-battery`. Needs the transport to activate virtio-i2c before DRIVER_OK (below) |
| (no PMU node) | GICC performance interrupt 0 | none: HVF exposes no PMU on either machine; the kernel's per-CPU "No ACPI PMU IRQ" notice is the only difference |
| `display-cutouts.md` reads `/sys/firmware/devicetree/base/compatible` | absent | identify the platform from SMBIOS instead |

Interrupts keep the conventions the device tree uses on HVF: SPIs edge-triggered (the in-kernel GIC
only pulses), PPIs level.

**An ACPI-described device exists before its bus does.** Its node is in the namespace from boot,
so udev loads the client driver at coldplug; a device-tree client only appears when its bus
registers. Linux's virtio-i2c registers its adapter inside its own probe, before DRIVER_OK, so an
ACPI client probes then and its first transfer waits forever on a device that is not running yet.
The libkrun transport therefore activates a device that asks for it
(`VirtioDevice::activates_before_driver_ok`, only virtio-i2c) on the first queue notification
after FEATURES_OK, as QEMU effectively does. Any new bus-attached ACPI client needs the same check.

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

**The BGRT image** must be `EfiBootServicesData` (Linux before 7.1 accepts no other type) and must
sit low in RAM: arm64 Linux treats boot services data as free memory and reserves the image only
after `efi_bgrt_init` has read its header, while its early allocations come from the top of RAM.
The edk2 fork copies the BMP into the lowest free range at ReadyToBoot; ours must do the same.

**Variable store migration**: limina converts an existing edk2 store to ours on first boot under
the new firmware, so `BootOrder`, boot entries and MOK state carry over (no extra fallback boot).

**Coupling to change in the same series**:
- the display pool's "guest driver ready" detector keys on the first `GET_EDID`, relying on the
  firmware never sending one (`graphics.md`); a firmware that reads EDID breaks it;
- `test-boot.sh`'s firmware-rev check, `vm-start-preflight.md`'s firmware checks, the debug-port
  `dep.edk2` field, the fork-rebase routine and the edk2 ledger.

## Phases

0. **ACPI on the edk2 fork — works, opt-in (`limina --acpi`).** libkrun
   (`devices/src/acpi.rs`) generates RSDP/XSDT/FADT/MADT/GTDT/PPTT/DSDT into the window at
   `ACPI_START` and names it with a `libkrun,acpi-tables` device-tree node; the fork's
   `KrunAcpiPlatformDxe` maps the window, chooses ACPI when the node exists (device tree
   otherwise) and installs the tables. A stock F44 guest boots EFI+venus on ACPI with no device
   tree visible, every virtio device and the xHCI gadgets bound, only `rtc-efi` as its RTC, and a host
   stop powering it off through the ACPI power button. The same firmware without `--acpi` boots
   the device-tree machine unchanged.
1. **ACPI parity and stock wins** on edk2. Done: power, sleep and wake buttons; the TPM; the SBS
   battery; the Limina logo on the guest's boot splash (Plymouth's `bgrt` theme, Fedora's
   default, draws the firmware's BGRT image; `spikes/acpi-guest/plymouth-limina.png`); CPPC and
   efficiency classes (see the table). Open: `rtc-efi` wakeup time. ACPI is the default; the
   device-tree machine stays selectable per VM (the control center's Machine popup) and is what
   `--kernel`/L1 boots get. A suspended guest resumes on the machine it was saved on.
2. **Rust firmware, device-tree machine — done.** Cardea passes the whole suite on the device-tree
   machine, Fedora and Debian LUKS and the TPM tests included.
3. **Rust firmware, ACPI machine; edk2 retired — done.** A stock F44 guest boots Cardea on ACPI
   with every device of phase 1 bound and powers off through the ACPI power button. No
   variable-store migration: Cardea's store is bit-compatible with edk2's emulated-NV layout
   (`third_party/cardea/crates/varstore`), so existing VMs keep their variables.
4. **Owned-firmware features**: host boot policy, multi-head EDID-aware GOP, the boot-time budget,
   kernel loading for `--kernel`/L1 (ACPI everywhere), Secure Boot.

## Open questions

- Cardea's license (MIT) and the licenses of the crates it links (`fdt` is MPL-2.0), which need
  the same counsel review as the rest of the tree before a release tag.
