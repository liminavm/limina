# M7 — USB passthrough (design + as-built)

Goal: hand a host (macOS) USB device to the Linux guest. Strategy (from
`docs/research/06-usb-passthrough.md`): **USB/IP**, because the *guest* side is 100% upstream —
stock `vhci_hcd` (a virtual host controller; no real EHCI/XHCI) + `usbip attach`. limina writes
only the **host** half: a libusb-backed USB/IP server. Staged plan **C → B → D**: prove over TCP,
ship over vsock, optionally add a native virtio-usb device later.

This milestone is **enhanced-tier** (per the two-tier tenet): a stock guest simply lacks USB and
still boots/runs fine; the custom kernel is the entry fee for *USB*, never for the VM.

## Status

| Phase | What | State |
|---|---|---|
| 1 | Guest kernel: enable USB + `USBIP_VHCI_HCD` + class drivers + `uinput` | ✅ shipped, verified |
| 2 | Host `limina-usbip` crate: USB/IP wire protocol + backend trait + CDC-ACM mock + libusb backend | ✅ shipped, 17 unit tests |
| 3a | L1 test: the guest-side USB/IP stack is present (`vhci_hcd`/usbip/uinput) | ✅ shipped, GREEN on HVF |
| 3b | Full **mock-attach** end-to-end over vsock — a device enumerates in the guest, no hardware | ◻ designed (below); next autonomous phase |
| 4 | **Real-device** passthrough (libusb claim + the macOS device-access gate) | ◻ hardware-gated (below) |

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

## Phase 3b — full mock-attach end-to-end (designed; next autonomous phase)

Prove a device **actually enumerates** in the guest with no hardware and no networking, via vsock.
The key enabler (verified against `drivers/usb/usbip/vhci_sysfs.c`): **`vhci_hcd`'s attach store
only checks `SOCK_STREAM` — no address-family restriction — so it accepts an `AF_VSOCK` fd**. So we
skip the stock `usbip` userspace tool entirely and drive the kernel directly:

1. **Host** (harness, reusing the control-plane vsock bridge — `unix_ipc_port_map` already maps a
   guest `AF_VSOCK(CID_HOST:port)` to a host unix socket): listen on a USB/IP socket and run
   `limina_usbip::serve(stream, &MockBackend)` per connection. Needs a *second* vsock port wired
   alongside the control plane (the worker's `unix_ipc_port_map` is a HashMap — extend `add_vsock`
   to take an extra `(port, socket)` and thread `--usbip-vsock-*` through the supervisor + harness).
2. **Guest** (`limina-init`, new `limina.usb_attach=<port>`): `socket(AF_VSOCK)` → connect
   `CID_HOST:port`; send `OP_REQ_IMPORT(busid="1-1")`; read `OP_REP_IMPORT` → parse `busnum/devnum/
   speed`; write `"0 <sockfd> <devid> <speed>\n"` to `/sys/devices/platform/vhci_hcd.0/attach`
   (`devid = (busnum<<16)|devnum`). The kernel's `vhci_hcd` then runs URB traffic against our
   server; usbcore enumerates the device and `cdc-acm` binds it.
3. **Assert**: `/dev/ttyACM0` appears; optionally write→read it to confirm the mock's bulk loopback.
   Emit a RESULT marker; the harness asserts it.

Risk to retire when built: that `cdc-acm` actually binds the mock descriptors, and the
devid/speed encoding the kernel expects. All other unknowns are settled.

## Phase 4 — real-device passthrough (hardware-gated)

Swap `MockBackend` → `LibusbBackend` and select a host device by busid. The gating question is the
**macOS claiming gate**:

- libusb claims devices with **no matching Apple driver** freely (FTDI/CP210x serial w/o the Apple
  CDC match, FIDO security keys, custom hardware). Apple-bound classes (standard HID, mass storage,
  audio) need the **Apple-managed, restricted `com.apple.vm.device-access`** entitlement (or a
  DriverKit dext) — no self-serve grant; UTM/QEMU hit the same wall.
- **Empirical probe:** `spikes/usb-probe` opens + claims a target via libusb and reports per-step.
  A **SoloKeys Solo 2** FIDO key is present and shows `!matched` in IORegistry (no Apple driver
  bound) — the ideal free-to-claim v1 target. **Open:** the probe saw **0 devices** when run inside
  Claude's tool process (likely that process lacks the IOKit/Mach bootstrap a login session has);
  re-run in a normal shell (`spikes/usb-probe/run.sh`) to confirm a login-context process claims it
  cleanly with no entitlement. If yes → real passthrough needs no Apple grant for this device class.

## Files

- `crates/limina-usbip/` — the host server (proto/backend/mock/server/libusb).
- `scripts/build-test-kernel.sh` — kernel USB config.
- `guest/limina-init/src/main.rs` — `limina.usb_probe` (3a); `limina.usb_attach` (3b, todo).
- `crates/limina-test/tests/usb.rs` — the L1 guest-stack test.
- `spikes/usb-probe/` — the libusb claim probe (macOS gate oracle).
- `docs/research/06-usb-passthrough.md` — the inventory this design realizes.
