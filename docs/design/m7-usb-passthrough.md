# M7 — USB passthrough (design + as-built)

Goal: hand a host (macOS) USB device to the Linux guest. Strategy (from
`docs/research/06-usb-passthrough.md`): **USB/IP**, because the *guest* side is 100% upstream —
stock `vhci_hcd` (a virtual host controller; no real EHCI/XHCI) + `usbip attach`. limina writes
only the **host** half: a USB/IP server whose device backend is libusb, or on macOS 27 an
`IOUSBHostDevice` opened through AccessoryAccess. Staged plan **C → B → D**: prove over TCP,
ship over vsock, optionally add a native virtio-usb device later.

This milestone is **enhanced-tier** (per the two-tier tenet): a stock guest simply lacks USB and
still boots/runs fine; the custom kernel is the entry fee for *USB*, never for the VM.

## Status

| Phase | What | State |
|---|---|---|
| 1 | Guest kernel: enable USB + `USBIP_VHCI_HCD` + class drivers + `uinput` | ✅ shipped, verified |
| 2 | Host `limina-usbip` crate: USB/IP wire protocol + backend trait + CDC-ACM mock + libusb backend | ✅ shipped, 17 unit tests |
| 3a | L1 test: the guest-side USB/IP stack is present (`vhci_hcd`/usbip/uinput) | ✅ shipped, GREEN on HVF |
| 3b | Full **mock-attach** end-to-end over vsock — a device enumerates in the guest, no hardware | ✅ shipped, GREEN on HVF |
| 4 | **Real-device** passthrough — AccessoryAccess on macOS 27 (headers read), root capture via the shared privileged helper below that (proven) | ◻ DEFERRED |

## Phase 1 — kernel (as built)

`scripts/build-test-kernel.sh`'s FRAG heredoc gained (all `=y`, the L1 kernel is all-builtin):
`USB_SUPPORT, USB, USB_COMMON, USBIP_CORE, USBIP_VHCI_HCD`, class drivers
`USB_ACM, USB_SERIAL(+FTDI_SIO, +CP210X), HID, USB_HID, SCSI, BLK_DEV_SD, USB_STORAGE`, and
`INPUT_UINPUT`. The build's verify loop asserts the key symbols survive `olddefconfig`.
**`vhci_hcd` needs no real host controller** — it is itself the (virtual) HCD. The product
(modular) kernel needs the same symbols folded into `build-kernel-rpm.sh`'s fragment (root-critical
ones `=y`, the rest may be `=m`) — *not yet done* (no in-guest USB consumer ships until Phase 3b/4).

## Phase 2 — the host `limina-usbip` crate (as built)

`crates/limina-usbip` — a transport-agnostic USB/IP **server** (exporter):

- **`proto.rs`** — the wire protocol, byte-exact to `Documentation/usb/usbip_protocol.rst` +
  `drivers/usb/usbip/`. Two families on one connection: the 8-byte **op_** header
  (`DEVLIST`/`IMPORT`) + `usbip_usb_device` (0x138 B), and the 48-byte **URB** header
  (`CMD_SUBMIT`/`RET_SUBMIT`/`CMD_UNLINK`/`RET_UNLINK`). **Endianness: every header field is
  big-endian EXCEPT the raw 8-byte control `setup` (little-endian, passed through verbatim).**
- **`backend.rs`** — `UsbBackend` / `UsbDevice` traits (enumerate, import, control/bulk/interrupt)
  so the server is hardware-independent.
- **`mock.rs`** — a hardware-free **CDC-ACM** device: canned descriptors + bulk loopback. Answers
  `GET_DESCRIPTOR`/`SET_CONFIGURATION`/CDC class requests so a guest enumerates it as `/dev/ttyACM0`.
  This is what makes the pipeline testable with no physical USB.
- **`server.rs`** — `serve(stream, backend)`: op_ phase (answer DEVLIST, then IMPORT/claim) →
  URB phase (translate each SUBMIT to a backend transfer, reply RET_SUBMIT). Works over any
  `Read + Write` (TCP for the prototype, **vsock for the shipping path**).
- **`libusb.rs`** (feature `libusb`, default on) — the real backend via `rusb`; maps each USB/IP op
  to a rusb call, claims every interface. Builds + links the host libusb 1.0.

17 unit tests; clippy `-D warnings` + fmt clean with and without the `libusb` feature.

## Phase 3a — guest stack present (as built)

`limina-init` gained `limina.usb_probe`: it checks `/sys/devices/platform/vhci_hcd.0`,
`/sys/bus/platform/drivers/vhci_hcd`, `/sys/bus/usb`, `/dev/uinput` and emits a
`RESULT: <name> PRESENT|MISSING` line each. `crates/limina-test/tests/usb.rs` boots the L1 guest
with that flag and asserts all four PRESENT + a clean power-off — GREEN on HVF. A lost
`CONFIG_USB*` symbol flips a marker to MISSING (the RED guard for config drift). In the
`test-boot.sh` suite.

## Phase 3b — full mock-attach end-to-end (as built)

A device **actually enumerates** in the guest with no hardware and no networking, over vsock.
The key enabler (verified against `drivers/usb/usbip/vhci_sysfs.c`): **`vhci_hcd`'s attach store
parses `sscanf(buf, "%u %u %u %u", &port, &sockfd, &devid, &speed)` (all decimal) and checks only
`SOCK_STREAM` — no address-family restriction — so it accepts an `AF_VSOCK` fd**. So we skip the
stock `usbip` userspace tool entirely and drive the kernel directly:

1. **Host** (harness): `with_usbip_vsock(port)` reuses the *existing* single vsock bridge (no
   worker/supervisor change — it just points the init at `limina.usb_attach` instead of the control
   agent). `Guest::accept_usbip_mock` accepts the guest's connection and runs
   `limina_usbip::serve(stream, &MockBackend)` in a background thread.
2. **Guest** (`limina-init`, `limina.usb_attach=<port>`): `socket(AF_VSOCK)` → connect
   `CID_HOST:port`; send the 40-byte `OP_REQ_IMPORT(busid="1-1")`; read the 320-byte `OP_REP_IMPORT`
   → parse `busnum/devnum/speed`; write `"0 <sockfd> <devid> <speed>\n"` (decimal) to
   `/sys/devices/platform/vhci_hcd.0/attach` (`devid = (busnum<<16)|devnum`). The kernel's
   `vhci_hcd` then runs URB traffic against our server; usbcore enumerates the device and `cdc-acm`
   binds it. The hand-rolled client mirrors `limina-usbip/src/proto.rs` (no crate pulled into the guest).
3. **Assert**: `/dev/ttyACM0` appears → RESULT marker → the harness asserts it.

**Verified live** (`tests/usb.rs::mock_cdc_acm_device_enumerates_in_guest_via_usbip`, GREEN on HVF) —
the guest console shows the real chain:
```
vhci_hcd.0: devid(65538) speed(2) speed_str(full-speed)
usb 1-1: new full-speed USB device number 2 using vhci_hcd
cdc_acm 1-1:1.0: ttyACM0: USB ACM device
```

## Phase 4 — real-device passthrough (hardware-gated; the macOS claiming gate, characterized)

Swap `MockBackend` → `LibusbBackend` (already written; builds + links host libusb 1.0.29) and select
a host device by busid. The remaining work is purely the **macOS claiming gate**, now characterized
empirically with `spikes/usb-probe` against a real **SoloKeys Solo 2** (VID:PID `1209:BEEE`):

**Three layers, in order:**
1. **USB TCC permission** (`com.apple.security.device.usb`) gates **enumeration**. Before the user
   grants the one-time dialog, `libusb` sees **0 devices**; after, it sees + `libusb_open`s the device
   and reads all descriptors/strings (control transfers to EP0 work). An unanswered dialog reads as
   "no devices", not as an error.
2. **Interface claiming** is the real gate. `libusb_claim_interface` succeeds **only for interfaces no
   macOS class driver holds**. The Solo 2 is *composite* and macOS binds **both** its interfaces —
   interface 0 = **CCID** (smartcard), interface 1 = **HID** (the FIDO/U2F interface) — so both claims
   return `LIBUSB_ERROR_ACCESS` with `kernel_driver_active=YES`. (The device showed `!matched` at the
   *device* level but Apple owns it at the *interface* level — the composite reality of §1.5.)
3. **Seizing an Apple-claimed device** requires the **restricted, Apple-managed
   `com.apple.vm.device-access`** entitlement (libusb uses `IOUSBHostObjectInitOptionsDeviceCapture`).
   **Empirically: this entitlement CANNOT be ad-hoc signed** — adding it SIGKILLs the process at launch
   (AMFI, exit 137), while removing it (keeping only the USB-TCC entitlement) runs fine. It needs an
   **Apple-granted provisioning profile** (request via Apple; UTM/QEMU hit exactly this wall).

**Proven as root on the real Solo 2 via `sudo spikes/usb-probe/run.sh` (2026-06-27):**
```
== claim test (the macOS gate)  [running as ROOT] ==
  interface 0: kernel_driver_active=YES(bound)  detach=OK  claim=OK ✅
  interface 1: kernel_driver_active=no          detach=n/a  claim=OK ✅
```
Interface 0 (CCID) detached and claimed with no entitlement, and detaching it **also freed
interface 1** (HID): capture is device-level. Root needs no entitlement for
`IOUSBHostObjectInitOptionsDeviceCapture`; the entitlement alternative is
`com.apple.vm.device-access` plus `IOServiceAuthorize()`
(`IOUSBHost.framework/Headers/IOUSBHostDefinitions.h:141-149`), Apple-managed and requested through
an Apple rep.

**macOS 27: AccessoryAccess.** `AAUSBAccessoryManager` matches devices and shows Apple's consent
UI from the app; `-[AAUSBAccessory openWithServiceQueue:completionHandler:]` then gives the
process exclusive use and returns an `IOUSBHostDevice`, with no root and no libusb
(`AccessoryAccess.framework/Headers/AAUSBAccessory.h:61-80`). It needs the managed
`com.apple.developer.accessory-access.usb` entitlement on a provisioning profile
(`AAUSBAccessoryManager.h:32`). Full header reading in `docs/research/06-usb-passthrough.md` §1.5;
**nothing of it has been exercised.**

**The plan: three rungs, the highest the host and the grant allow.**

| Rung | Host | Grant | Who opens the device | Backend |
|---|---|---|---|---|
| 1. AccessoryAccess | macOS 27+ | `com.apple.developer.accessory-access.usb` (profile) | supervisor matches + gets consent, passes the `AAUSBAccessory` over XPC; **worker opens it** | IOUSBHost (new) |
| 2. Free-to-claim | any supported (15+) | USB TCC only | worker | `LibusbBackend` |
| 3. Root capture | any supported | root, via `limina-privhelperd` | helper captures, serves USB/IP | `LibusbBackend` |

Rejected: a DriverKit `.dext` (`com.apple.developer.driverkit.transport.usb`, also managed, weeks of
dext work, no benefit) and codeless kexts / SIP-off (dead or dev-only).

- **Rung 1 splits by process because the API does.** The manager "presents UI on behalf of your
  application" and must run in "an ordinary application, that is, one that appears in the Dock"
  (`AAUSBAccessoryManager.h:30`), so matching, consent and hotplug live in the supervisor; the
  accessory is XPC-encodable and the header expects "another worker process of this client
  application" to open it (`AAUSBAccessory.h:67-68`). The open `IOUSBHostDevice` then lives in the
  worker, next to the transport — no USB/IP server in a separate process and no privilege boundary.
- **Rung 1 needs a second backend.** libusb cannot adopt an already-open `IOUSBHostDevice`, so an
  IOUSBHost backend implements the same `limina-usbip` backend trait (and, later, the xHCI
  `UsbDeviceModel`, `usb-xhci.md` §3.6). Rungs 2 and 3 keep `LibusbBackend`.
- **Rung 1 depends on the channel decision.** The entitlement rides on a provisioning profile;
  whether Apple grants it for a Developer ID build as well as a MAS one is unverified, and
  `distribution.md` §2.1 owns that decision. Without the grant,
  macOS 27 falls back to rungs 2–3.
- **Rung 3 is the only one that needs the shared root broker.** It is the first client of
  `limina-privhelperd` (`privileged-helper.md`); with rung 1 in place it shrinks to "Apple-claimed
  devices on macOS 15–26, or without the AA grant". Not CI-testable (root + a physical device);
  validated by the manual `sudo` spike against the Solo 2.
- **Capture is device-level on every rung we know of:** while the guest holds a composite device,
  the host loses all of it. Whether AA's `open` takes devices Apple's class drivers hold (HID,
  CCID, mass storage) or refuses them with `AAErrorCodeInvalidAccessoryState` is not stated in the
  headers and is the first thing to measure — it decides whether rung 1 covers the FIDO-key class
  or only what rung 2 already reaches.
- **The wire pipeline is proven** by the mock (3b): every rung drops a backend into the same
  `serve()`. Remaining code: device selection (`--usb VID:PID`, opt-in), the IOUSBHost backend and
  the supervisor-side AA listener for rung 1, the helper for rung 3.

**To measure before building rung 1** (on a macOS 27 host): the consent UI and whether it
persists per device; HID/CCID/mass storage through `open`; whether our launchd-started worker,
which receives its fds over Mach (`crates/limina-launch`), is accepted as a worker process of the
app; transfer latency and throughput against `LibusbBackend` on the same device; isochronous at
all; behaviour across host sleep, device re-enumeration (DFU, mode switches) and fast user switch
(the header says accessories disconnect and come back, `AAUSBAccessoryListener.h:22-27`); Developer
ID + hardened runtime vs the App Sandbox.

**Empirical oracle for rungs 2–3:** `spikes/usb-probe/run.sh [VID PID]` opens+detaches+claims and
classifies the device (free-to-claim / root-claimable / Apple-claimed / other); run it plain for the
userspace gate and `sudo …` for the root path.

## Files

- `crates/limina-usbip/` — the host server (proto/backend/mock/server/libusb).
- `scripts/build-test-kernel.sh` — kernel USB config.
- `guest/limina-init/src/main.rs` — `limina.usb_probe` (3a); `limina.usb_attach` (3b, todo).
- `crates/limina-test/tests/usb.rs` — the L1 guest-stack test.
- `spikes/usb-probe/` — the libusb claim probe (macOS gate oracle).
- `docs/research/06-usb-passthrough.md` — the inventory this design realizes.
