# USB controller design: emulated xHCI for stock-tier gadgets (M7 ∩ M14)

Status: **proposed** (2026-07-24). Companion docs: `docs/design/m7-usb-passthrough.md`
(USB/IP passthrough, mock GREEN), `docs/fido-authenticator.md` (the shipped uhid/agent
FIDO transport), roadmap §M7 / §M14 (which decided "FINAL = VMM-emulated xHCI").

## 1. Why an emulated controller, and why now

Two features converge on the same missing piece:

- **M14 FIDO / Touch ID.** The CTAP2-on-SEP authenticator core is shipped and
  live-verified, but its transport is `limina-agent` + `/dev/uhid` over vsock —
  **enhanced-tier only**. The stock tier (pristine Fedora, no agent) needs the
  authenticator to appear as a plain USB HID device the guest discovers by itself.
- **M14 fingerprint.** The impersonated match-on-chip (MOC) reader *must* be a USB
  device: libfprint binds drivers by VID/PID on the USB bus. There is no uhid
  shortcut here at all — this feature is impossible without a USB bus in the guest.
- **M7 passthrough** (bonus, not a driver of the design): USB/IP requires
  `kernel-modules-extra` in the guest (vhci modules are not in a default Fedora
  install), so it is inherently enhanced-tier. An emulated controller with a libusb
  backend could later serve stock guests too, but M7 stays on the proven USB/IP
  path for now.

Per the two-tier guarantee: the controller is additive (a stock guest without it
just has no USB bus, exactly as today), and it *serves* the stock tier (a stock
guest **with** it gets FIDO + fingerprint with zero guest components).

## 2. Prior art survey (2026-07-24)

- **libkrun: none.** Zero USB-related issues/PRs/branches across `libkrun/libkrun`,
  `krunkit`, `libkrunfw`; no USB code or deps in the tree (grep-verified in our
  vendored checkout). We would be first — no upstream shape to conform to.
- **crosvm** has the only Rust xHCI (`devices/src/usb/`, BSD-3-Clause): full
  controller core (TRB ABI, rings, slots, transfers, streams, isoch) wrapped as a
  PCI device with a plain **level INTx `IrqLevelEvent` — no MSI-X**, so nothing
  about the core assumes PCI interrupts. Productized only for host passthrough
  (Linux usbfs), but its 2024 **`fido_backend`** synthesizes a U2F-HID USB device
  backed by a host hidraw fd — the exact "software-defined HID authenticator behind
  xHCI" shape we want, with the SEP in place of hidraw.
- **QEMU** splits `hcd-xhci.c` (~3.7k lines, core) from `hcd-xhci-pci.c` and
  `hcd-xhci-sysbus.c` (`TYPE_XHCI_SYSBUS`: one MMIO region + N wired IRQs, used by
  sbsa-ref at a fixed address). Its USB device model (`USBDeviceClass`) is a small
  vtable around packets with **deferred completion** (`USB_RET_ASYNC` +
  `usb_packet_complete`). FIDO story tops out at U2F/CTAP1 (`u2f-emulated` via
  libu2f-emu; CTAP2 through `u2f-passthru` is buggy — QEMU #2293); real CTAP2 comes
  only from the external **canokey-qemu** (full FIDO2 state machine in a library,
  QEMU doing only packet plumbing via endpoint callbacks). No fingerprint-reader
  emulation exists in QEMU or anywhere else.
- **Apple Vz** added its own xHCI on macOS 15 (`VZXHCIControllerConfiguration`) but
  the only device is USB mass storage — no passthrough, no custom devices. Validates
  xHCI-in-a-macOS-VMM; also re-validates why we're not on Vz.
- **Guest driver side (verified in Fedora dist-git + rpmfind):** Linux
  `xhci-plat` binds DT `compatible = "generic-xhci"` (binding: one `reg`, **one
  `interrupts`**, optional `dma-coherent`). Fedora aarch64 (f43/f44/rawhide):
  `CONFIG_USB_XHCI_HCD=y`, `CONFIG_USB_XHCI_PLATFORM=m` with `xhci-plat-hcd.ko` in
  **kernel-modules-core** (always installed, autoloads via OF modalias). A pristine
  stock guest drives this controller with zero changes. (The usbip/vhci modules by
  contrast live in kernel-modules-extra — the M7 two-tier gap noted above.)
- **libfprint:** its "virtual" drivers are UNIX-socket CI shims, not USB — useless
  as guest-facing prior art. Nobody has emulated a real MOC reader at the USB level;
  libfprint's in-tree **umockdev recordings** of real device traffic are the best
  protocol oracle for whichever reader we impersonate. Avoid SDCP-speaking targets
  (Egis newer firmware): SDCP needs a factory-provisioned device cert we cannot
  forge; upstream goodixmoc/synaptics do not require it today.

## 3. Design overview

```
guest: xhci-plat → USB core → usbhid/fido-id → hidraw   (stock, zero components)
                              └ libfprint MOC driver     (stock, by VID/PID)
        │ MMIO (generic-xhci node, 64 KiB window, 1 edge SPI)
libkrun: XhciDevice (BusDevice)               ← mechanism, patches/libkrun
         ├ register file + command/event/transfer rings (vcpu thread + worker)
         └ Vec<Arc<dyn UsbDeviceModel>> root ports      ← trait defined in libkrun
limina:  gadget impls                          ← policy, crates/limina
         ├ FidoGadget → crates/limina/src/fido (CTAPHID core, unchanged) → SEP
         └ FprintGadget (wave 3) → Touch ID verify
```

Mechanism/policy split as usual: libkrun carries the controller and the
`UsbDeviceModel` trait; limina implements the gadgets (they need the SEP dylib,
LAContext, per-VM stores — none of which belongs in libkrun). Plumbing follows the
battery precedent: a `VmResources.usb_devices: Vec<Arc<dyn UsbDeviceModel>>` field
set by `limina-vmm` (`build_resources`), no C ABI needed (we consume the crates
directly; a `krun_add_usb_*` shim can be added later for upstream).

### 3.1 Controller: platform xHCI in libkrun

New `src/devices/src/usb/xhci/` module (feature-gated `usb`), wired like every
device we've added (0042 battery / 0047 snd pattern):

- **`DeviceType::Xhci`** variant + `create_xhci_node` in `fdt/aarch64.rs`:
  `compatible = "generic-xhci"`, `reg = <base 0x10000>`, one interrupt,
  `dma-coherent`. 64 KiB window means generalizing the hard-coded 4 KiB
  `MMIO_LEN` bump allocator in `device_manager/{hvf,kvm}/mmio.rs` — registration
  grows a size parameter (straightforward; the allocator is a plain bump).
- **`BusDevice` impl** services the register file synchronously on the vcpu
  thread: capability regs (HCIVERSION 0x100, AC64=1 since guest RAM starts at
  0x8000_0000, CSZ=0 → 32-byte contexts, no scratchpad), operational regs
  (USBCMD/USBSTS/CRCR/DCBAAP/CONFIG + PORTSC×4), runtime regs (interrupter 0
  only: IMAN/IMOD/ERSTSZ/ERSTBA/ERDP), doorbells. **4 root ports, USB2 protocol
  only** (one Supported Protocol extended cap; FIDO HID is full-speed, MOC
  readers are full/high-speed — USB3 adds nothing but PORTSC complexity).
- **Threading (gpu/snd pattern):** doorbell writes enqueue work and return; a
  dedicated worker thread walks command/transfer rings, calls into the device
  models, writes the event ring via a `GuestMemoryMmap` clone
  (`read_obj`/`write_obj` at guest-physical — the same facility virtio queue code
  uses), and asserts the SPI cross-thread through the `IrqChip` handle.
- **Interrupts:** HVF injection is `hv_gic_set_spi` — as used today, a one-shot
  pulse (our RTC/GPIO declare their SPIs edge-triggered for this reason). Do the
  same: edge-triggered SPI, pulse on each event-ring batch while IMAN.IE is set,
  and **re-pulse on IMAN.IP write-1-clear if the event ring is non-empty** (the
  classic lost-edge guard; QEMU/crosvm get this for free from level INTx, we
  don't). IMOD can start as a no-op — HID event rates are trivial.

### 3.2 Scope: what we implement vs. deliberately skip

xHCI's reputation for complexity comes mostly from features our gadgets never
touch. v1 implements: slot enable/disable, address device, configure/evaluate
endpoint, the control-transfer TRB sequence (setup/data/status), interrupt and
bulk endpoints, event ring + ERST, port reset/status, stop/reset-endpoint and
set-TR-dequeue (fprintd and error paths need them honest).

Deliberately deferred (each returns the spec'd error until needed): **streams**
(only UAS uses them), **isochronous** (webcams someday, behind passthrough),
secondary interrupters, MSI (n/a on sysbus), scratchpads, 64-byte contexts,
bandwidth negotiation (report success), power management beyond PORTSC basics.

QEMU's own FIXME list flags "endpoint stopped/reset with transfers in flight" as
its acknowledged rough edge; our HID-first scope keeps the in-flight set to at
most one pending interrupt-IN TRB per device, which makes those state machines
tractable rather than heroic.

**Build fresh, don't port.** crosvm's core is the reference (BSD-3; its
`xhci_abi.rs` TRB definitions can be adapted nearly mechanically), but the
controller proper drags crosvm's `EventLoop`/`AsyncJobQueue`/`base`/PCI infra
that doesn't map to libkrun's synchronous-BusDevice + worker-thread style, and
carries streams/isoch scope we're skipping. A scope-trimmed fresh implementation
in libkrun idiom is smaller, upstreamable, and reviewable; QEMU is the behavioral
second opinion when the spec is ambiguous.

### 3.3 The device-model trait (the QEMU lesson that matters most)

Both mature stacks converged on *packets with deferred completion*, and our FIDO
core requires it (Touch ID prompts block for seconds; the shipped CTAPHID core
already runs CBOR on a worker and pumps KEEPALIVE every ~100 ms). Sketch:

```rust
pub trait UsbDeviceModel: Send + Sync {
    fn descriptors(&self) -> &DeviceDescriptors;      // device/config/interface/HID/endpoint
    fn handle_control(&self, xfer: ControlTransfer);  // completes now or later via xfer.complete()
    fn handle_transfer(&self, ep: EpAddr, xfer: Transfer); // interrupt/bulk; may hold the TRB
    fn reset(&self);
}
```

Key semantic: an interrupt-IN transfer with no data ready is simply **held** (the
TRB stays on the ring) until the gadget completes it — xHCI's natural analogue of
NAK, and exactly how the FIDO gadget waits for a report or keepalive. Completion
from any thread; the controller turns it into a transfer event + doorbell-thread
wakeup. This is canokey-qemu's split too: the full CTAP2 state machine lives
behind a narrow packet API — ours already does (`crates/limina/src/fido/mod.rs`
is explicitly transport-agnostic, six unit tests and two live clients against it).

### 3.4 Gadget 1: FIDO HID key (wave 2)

Reuses `crates/limina/src/fido` unchanged. The gadget provides: HID report
descriptor identical to the uhid one (usage page 0xF1D0, 64-byte IN/OUT), the
same vendor-neutral 0x1d6b:0x0f1d identity, EP0 HID class requests, one
interrupt-IN + one interrupt-OUT endpoint mapping to `on_report`/report frames.
Guest sees a plain USB FIDO key; fido-id/udev/browsers need nothing. The two
wire-traced CTAPHID lessons (keepalive cadence, canonical getInfo CBOR) carry
over untouched since the core is shared.

### 3.5 Gadget 2: impersonated MOC fingerprint reader (wave 3, own design doc)

The controller work only needs to guarantee: vendor-specific interfaces, bulk +
interrupt endpoints, honest stop/reset-endpoint. Target selection happens in the
wave-3 doc; constraints already known: pick a **non-SDCP** MOC device whose
libfprint driver source is the spec, use libfprint's umockdev recordings as the
protocol oracle, advertise an impossibly-high firmware version so fwupd never
engages, MOC-verify = host Touch ID (never match-on-host).

### 3.6 Future: passthrough backend (not scheduled)

A `UsbDeviceModel` impl wrapping `limina-usbip`'s `LibusbBackend` would give
stock-tier real-device passthrough through the same controller once
`limina-privhelperd` exists (capture still needs root). UTM/QEMU-on-macOS
experience says the hard part is the host capture, not the controller — which M7
already solved to the root-capture stage. Nothing in this design blocks it; the
trait is deliberately the same shape as `limina-usbip::UsbDevice`.

## 4. Kernel/config notes

- **Stock:** nothing to do (xhci-plat in kernel-modules-core, autoloads).
- **Test kernel** (`scripts/build-test-kernel.sh` FRAG): add
  `CONFIG_USB_XHCI_HCD=y`, `CONFIG_USB_XHCI_PLATFORM=y` (+ keep the existing
  USB_HID etc.) so L1/L2 can exercise the controller; extend the olddefconfig
  verify-loop list. Note the Fedora symbol is `USB_XHCI_PLATFORM` (not `_PLAT`).
- **Enhanced 16k kernel** (`build-kernel-rpm.sh`): same two symbols (=y or =m);
  needed before enhanced-tier guests see the controller.

## 5. Testing (RED-first, `crates/limina-test`)

- **L1 xhci-enumerate:** boot test kernel with the controller + no devices; assert
  `xhci-hcd` registers and the root hub enumerates (console/sysfs oracle, like the
  M7 3a test).
- **L1 fido-gadget:** controller + FIDO gadget; assert the guest sees a HID device
  with usage page 0xF1D0 (`/sys/bus/usb` + hidraw node), then run the raw-CTAPHID
  python probe (the M14 oracle) through hidraw for INIT+getInfo. This is the USB
  twin of the existing uhid path verification.
- **L2 (enhanced image):** `fido2-token -I` / `fido2-cred` against the USB gadget
  in a full boot, parallel to the existing FIDO guard; store keepalive behavior
  under a blocked mock-SEP to catch regressions in deferred completion.
- Unit layer: TRB ring walkers and the control-transfer state machine are pure
  functions over `GuestMemoryMmap` — property-test the cycle-bit/link-TRB
  handling (this is where xHCI implementations historically break, and where
  QEMU's guest-triggerable CVEs lived — treat ring pointers as hostile input).

## 6. Wave plan

1. **Controller bring-up:** MMIO/FDT/IRQ plumbing + register file + rings; oracle
   = stock guest's own xhci-plat binds, root hub up, `lsusb` empty but sane.
2. **FIDO gadget** — ✅ **done (2026-07-24, Stage C).** libkrun carries a generic
   `HidReportPipe` gadget (mechanism, patch 0098); limina wires it to the CTAPHID/SEP
   authenticator (policy). Chosen split is the **proxy** (option a): the worker's gadget is a
   thin transport shuttling 64-byte CTAPHID frames over a UNIX socket (`--fido-socket`) to the
   supervisor's `FidoAuthenticator` — one authenticator, one store, one keepalive engine
   (`crate::fido::pump`), shared with the uhid path. Gated on `sep::available()` + store, cold-
   plugged at VM start. Oracle = `l1_xhci_fido_authenticator` (hidraw usage page 0xF1D0 →
   CTAPHID INIT + getInfo, presence-free). Browser/pam_u2f/fido2-cred re-run on the USB path is
   the manual Touch-ID follow-up. Nothing retired: the uhid path stays for agent-tier guests
   (capability detection is additive per CLAUDE.md).
3. **Fingerprint reader:** own design doc (target device selection, protocol
   corpus, enrollment UX), then gadget implementation.
4. *(unscheduled)* passthrough backend behind privhelperd.

## 7. Open questions

- Whether `hv_gic_set_spi(irq, false)` gives us usable level semantics (would
  simplify the re-pulse guard) — probe during wave 1; edge + re-pulse is the safe
  default and matches shipped RTC/GPIO precedent.
- Hotplug: PORTSC connect-status-change events let gadgets attach/detach at
  runtime (needed eventually for passthrough attach). Wave 1 can ship with
  cold-plug only, but keep PORTSC change-bit plumbing honest from the start.
- Upstreaming: the controller + trait are exactly "mechanism in libkrun" — plan
  to offer the series upstream once wave 2 is green (there is no competing
  upstream design; we define the shape).
