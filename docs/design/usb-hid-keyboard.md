# USB HID keyboard gadget: a keyboard for the pre-driver window

Status: **implemented** (2026-08-23), pending validation against a real encrypted-root guest.
Companion docs: `docs/design/usb-xhci.md` (the emulated
controller and the gadget seam), `docs/research/04-input-and-keyboard.md` (the virtio-input
keyboard), `docs/input-and-windows.md` (the input stack as a whole).

## 1. The gap

Between `ExitBootServices` and the moment the guest kernel binds `virtio_input`, the guest has
**no keyboard**. The firmware's keyboard is the firmware's own (`VirtioKeyboardDxe`, ConIn), and
it is explicitly torn down on the way out; Linux's is `virtio_input`. Anything that prompts from
the initramfs — a LUKS passphrase, the dracut or systemd emergency shell, a fsck confirmation —
therefore cannot be answered.

**Neither major initramfs generator ships `virtio_input`** (both source-verified 2026-08-23):

- `initramfs-tools` (Debian, Ubuntu) mentions virtio exactly twice, both
  `modules="$modules virtio_pci virtio_mmio"` in `hook-functions`. `virtio_input.ko` lives in
  `kernel/drivers/virtio/`, which no category copies, so even `MODULES=most` omits it.
- `dracut-ng` (Fedora, RHEL, openSUSE) installs `virtio virtio_ring virtio_pci` and
  `virtio_blk virtio_scsi` generically (`modules.d/70kernel-modules/module-setup.sh`) — and no
  `=drivers/virtio` directory, so `virtio_input` is likewise absent.

Debian is the sharp case only because an encrypted root makes it fatal: the guest is **unusable**,
not degraded. Fedora's enhanced tier dodges the same gap solely because `install-enhanced.sh:247`
writes `add_drivers+=" … virtio_input …"` into `/etc/dracut.conf.d/90-limina.conf` — a fix that
has, by definition, not run on a guest we have never enhanced.

This breaks the compatibility floor: a stock guest must be *degraded*, never *unusable*.

## 2. Why a USB keyboard is the fix

Every distro must support a USB keyboard at a bare-metal LUKS prompt, so — unlike `virtio_input`
— the USB HID stack **is** in every stock initramfs, in both generators:

- `initramfs-tools` base copies `kernel/drivers/usb/host` (xhci-plat) and `kernel/drivers/hid`
  (usbhid, hid-generic).
- `dracut-ng` installs `hid_generic` unconditionally, plus `usbhid`, `xhci-hcd`,
  **`xhci-plat-hcd`** (the driver our platform controller binds) and `=drivers/hid`.

A USB keyboard therefore closes the whole class with **zero guest-side action**, where any scheme
that injects a module into the initramfs only ever covers the distros we teach it about.

The controller is already present in every VM: `--usb` is on by default (`crates/limina/src/main.rs:578`,
`usb_enabled()`; `--no-usb` opts out) and limina always passes it to the worker.

## 3. The handoff is self-correcting

The virtio-input device is activated exactly when the guest driver reaches DRIVER_OK. The
firmware resets it on its way out — `VirtioKeyboardExitBoot()` calls
`SetDeviceStatus(Dev->VirtIo, 0)` (`OvmfPkg/VirtioKeyboardDxe/VirtioKeyboard.c:1185-1202`) —
which, with the libkrun reset→`Inactive` fix, yields an unconditional timeline:

| phase                          | virtio keyboard          | keys carried by |
|--------------------------------|--------------------------|-----------------|
| firmware / GRUB                | Activated (VirtioKeyboardDxe) | virtio     |
| ExitBootServices → initramfs   | **Inactive** (reset at exit-boot) | **USB gadget** |
| after pivot, `virtio_input` binds | Activated             | virtio          |
| reboot / kexec                 | reset → Inactive         | USB gadget      |

The whole policy is one line: **route keys to the USB gadget while the virtio keyboard is not
activated, to virtio otherwise.** It does not depend on the guest's initramfs carrying
`virtio_mmio` — the firmware performs the reset itself.

We do not have to ask libkrun for that state, because we already hold an object whose lifetime
*is* it. libkrun creates the events-provider instance inside `InputWorker::work()` — reached only
from `activate()` — and drops it when `reset()` joins that worker
(`devices/src/virtio/input/{device,worker}.rs`). That instance is ours (`backends::FdEvents`), so
it raises a flag when constructed and lowers it when dropped. No new libkrun API, no C-ABI query.

## 4. The four pieces

1. **The gadget** — `HidReportPipe::new_in_only` (`devices/src/usb/report_pipe.rs`). The existing
   pipe is shaped for a *data* pipe (CTAPHID): two interrupt endpoints, one `report_len` in both
   directions, an unbounded FIFO that must never drop a frame. An input device is the other
   shape, and the constructor carries both differences. One interrupt-IN endpoint: a keyboard's
   only host→device report is the LED byte, which arrives as a SET_REPORT control, and an
   interrupt-OUT endpoint with no matching Output item is a descriptor mismatch. And a
   **perishable** FIFO: capped, keeping the newest, and emptied when the guest reads the HID
   report descriptor. The held-IN discipline, SET_IDLE/SET_PROTOCOL and GET_REPORT carry over
   unchanged. Identity and report descriptor (policy) are in `crates/limina-vmm/src/usb_kbd.rs`.
2. **evdev→HID** — `crates/limina-input/src/hidkbd.rs`: the usage-page-0x07 table (the inverse of
   the kernel's `hid_keyboard[]`, so a report comes back out of `hid-generic` as the `KEY_*` the
   supervisor sent) and the diffed 8-byte report the wire form is derived from.
3. **The activation oracle** — `backends::FdEvents`'s create/drop bracket (§3). One
   `Arc<AtomicBool>`, shared with the router; the pointer devices pass `None`.
4. **The router** — `crates/limina-input/src/router.rs`, interposed on the keyboard socket: it
   reads what the supervisor writes and forwards to a fresh datagram pair the virtio events
   backend reads, or turns it into HID reports. `Route::step` is the whole policy and is pure.

Always plugged, with host-side routing — not runtime hotplug. The cost is one idle keyboard in
the guest's device list. Presenting an unplug is reachable (`usb/xhci/device.rs:566` already does
it for snapshot reconcile), but it adds a race with a keystroke in flight, for cosmetics. The
gadget is cold-plugged *after* the FIDO and fingerprint gadgets so it takes a new port rather
than renumbering theirs, which an existing snapshot would see as an unplug.

The router also closes a defect that predates the gadget: nothing drained the keyboard socket
while the virtio device was inactive, so events written in that window sat in the socket buffer
and were delivered in a burst the moment the guest bound the driver. A passphrase typed at a LUKS
prompt would replay into the session that prompt unlocked. The router now drains unconditionally,
and the gadget's own FIFO is perishable for the same reason.

## 5. Load-bearing rules

- **`bInterfaceSubClass` / `bInterfaceProtocol` stay 0/0.** EDK2's `UsbKbDxe` binds only
  boot-protocol keyboards, so 0/0 keeps the gadget invisible to the firmware. Linux's
  `hid-generic` binds on class 3 regardless. This is not a shortcut: EFI ConIn aggregates every
  keyboard it finds, so a boot-protocol gadget alongside `VirtioKeyboardDxe` makes **every
  keystroke at GRUB arrive twice**. Boot-protocol and VirtioKeyboardDxe are mutually exclusive —
  see §7.
- **On the USB→virtio flip, emit one all-released report on the USB side, before forwarding the
  event that triggered the flip.** Otherwise a key held across the moment the driver bound stays
  down in the guest forever: the gadget never sends its release and virtio never saw its press.
  Do not re-assert modifiers on virtio — the guest driver is fresh and holds no state.
- **A queued input report is stale, not pending.** A keystroke produced while nothing was
  listening must never be delivered to the driver that eventually binds; it types into whatever
  now owns the console. Hence both the cap and the flush on report-descriptor read — the
  controller calls `reset()` only for a Reset Device command, which Linux does not issue during a
  normal enumeration, so that control is the only bind signal the gadget can see.
- **evdev autorepeat produces no HID report.** The key is already in the array and USB hosts run
  their own typematic; forwarding it re-types.
- **Keys with no HID usage do not type in the initramfs window.** That is the media transport
  keys (`hidkbd::KEYS_WITHOUT_HID_USAGE`), which live on the consumer page. A passphrase is
  ASCII; this is accepted, not worked around, and a test forces the choice for any key added
  later.

## 6. Why virtio-input remains the keyboard for the running system

The USB gadget is the floor for the pre-driver window and nothing more.

- **virtio-input is an evdev pipe, not a HID pipe.** `KEY_*` codes cross verbatim and the guest's
  evdev node mirrors what we send. USB forces a lossy round trip (`KEY_*` → HID usage →
  `hid-generic` → `KEY_*`) in which only codes with a HID usage exist at all — a real constraint
  for the fn/media-key buckets, which would need a second consumer-control collection to express
  a subset of what virtio already carries.
- **Capability declaration.** `krun_input_config` states identity, `EV_*` bitmaps, `INPUT_PROP_*`
  and `absinfo` per device — the substrate for the per-connector absolute pointer and for how the
  guest's libinput classifies each device. HID report descriptors cannot express that, and cannot
  be re-stated at runtime.
- **No rollover ceiling, no report state.** Every virtio event is discrete; HID sends a diffed
  report (6KRO in boot shape) where one lost report desynchronizes held-key state.
- **Latency.** virtio events land on the eventq directly; each HID report costs an endpoint
  round-trip through the xHCI engine. Fine for a passphrase, wrong for typing, useless at pointer
  motion rates.
- **The pointer stays virtio regardless** (abs axes fitted per display, hi-res scroll), so
  virtio-input is kept either way; moving only the keyboard off it buys nothing.
- **It is the half we own end-to-end.** The USB path runs through the guest's `hid-generic`.

## 7. Why `VirtioKeyboardDxe` stays

Reaching the gadget from the firmware would mean adding `XhciDxe`, `UsbBusDxe`, `UsbKbDxe` and —
because `XhciDxe` is a `PciIo` driver while our controller is a DT `generic-xhci` platform
device — `NonDiscoverablePciDeviceDxe` plus a registration DXE, to a platform
(`ArmVirtPkg/ArmVirtKrun.dsc`) that carries **no USB stack at all** today. That is four drivers
and a shim on a boot-critical path, replacing one vendored driver already validated at the GRUB
menu (`ArmVirtKrun.dsc:336`, `ArmVirtKrun.fdf:163`).

Revisit only if the firmware needs the USB stack for an independent reason (pre-boot FIDO, USB
boot media). At that point dropping `VirtioKeyboardDxe` is nearly free — and, per §5, becomes
mandatory the moment the gadget declares boot protocol.

## 8. Verification

Unit level (`cargo test`): the routing policy, the handoff, the reboot, the rollover and the
report shape are pinned in `router.rs` and `hidkbd.rs`; the endpoint/subclass shape and the
perishable-FIFO rules in `report_pipe.rs` and `usb_kbd.rs`.

What only a running guest can answer:

- **The gap and its close, on one image:** `module_blacklist=virtio_input` on the existing Fedora
  test image reproduces the pre-driver window for the whole boot, so typing must work with the
  gadget and not without it — no Debian image needed.
- **An encrypted root**, which is the case that is *fatal* rather than annoying: type the
  passphrase at a real LUKS prompt.
- **The flip:** once the guest binds `virtio_input`, keys arrive exactly once.
- **Snapshot/restore:** the gadget takes a new port, so an old capture must come back through the
  `model_id` reconcile as an honest unplug (`usb/xhci/device.rs:534`).
- **The premise itself**, rather than the generator source:
  `lsinitramfs /boot/initrd.img-$(uname -r) | grep -E 'xhci|usbhid|hid-generic'`, and
  `modinfo virtio_input` for the post-pivot half.

## 9. Deferred

Consumer-control (media) keys, a USB pointer, and runtime unplug of the gadget. None are needed
for the window this closes.
