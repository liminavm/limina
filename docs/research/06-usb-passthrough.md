# 06 — USB Device Passthrough

Scope: how to expose host (macOS) USB devices to a Linux guest running under
libkrun on Apple Silicon. libkrun ships **no USB device of any kind** today, so
this document inventories the realistic ways to add it — USB/IP over a guest
network/vsock path, a new virtio-USB host device backed by libusb, possible
vhost-user reuse, and per-class shortcuts — and recommends a staged plan
(USB/IP first, native virtio-USB later).

> **Verification status.** The libkrun tree was read directly. The "no USB"
> finding and the C-API line numbers below are confirmed from source. The one
> item that could **not** be confirmed from local source is whether the
> libkrunfw guest kernel ships `CONFIG_USBIP_VHCI_HCD` — that is the #1 blocking
> open question (§5). macOS/USB-IP/libusb facts are from established knowledge
> and flagged where confidence is lower.

---

## 1. What exists today

### 1.1 libkrun: no USB, full stop

- **Confirmed:** there is **no `usb` directory** under
  `src/devices/src/virtio/` and **no `krun_*usb*` symbol** in
  `include/libkrun.h` (1455 lines). A case-insensitive `usb` grep across the
  whole libkrun tree returns exactly **one** hit —
  `src/arch_gen/src/x86/bootparam.rs` — which is an unrelated x86 boot-params
  field, not USB the bus. So libkrun has zero USB code.
- libkrun's device set is exactly (from the
  `src/devices/src/virtio/` listing): `balloon`, `block`, `console`, `fs`,
  `gpu`, `input`, `net`, `rng`, `vsock`, plus `vhost_user` and the shared
  scaffolding (`mmio.rs`, `queue.rs`, `device.rs`, `mod.rs`,
  `descriptor_utils.rs`). No USB.
- Consequence: **nothing in libkrun speaks USB**. Any USB story is net-new
  code (in libkrun or in a guest agent) — there is no partial implementation
  to extend.

### 1.2 The device facilities we *can* build on

Even without USB, libkrun gives us the plumbing a USB feature needs:

| Facility | What it gives us | Where (approx) | Status |
|---|---|---|---|
| **virtio-vsock** | Host↔guest stream/datagram socket over a guaranteed channel; ideal transport for USB/IP without real networking | `src/devices/src/virtio/vsock/` | exists (confirmed) |
| **virtio-net** | Guest gets an IP NIC (passt/gvproxy/tap; cf. `krun_add_net_*` at `include/libkrun.h:401,443,479`); USB/IP-over-TCP could ride this | `src/devices/src/virtio/net/` | exists (confirmed) |
| **virtio device scaffolding** | Queue handling, MMIO transport, IRQ injection, feature negotiation — the boilerplate a new virtio-usb device would copy | `src/devices/src/virtio/mmio.rs`, `queue.rs`, `device.rs`, `mod.rs` | exists (confirmed) |
| **vhost-user frontend** | Lets an *external* process implement a virtio device; a vhost-user-usb backend could attach here without touching the VMM core | `src/devices/src/virtio/vhost_user/` | exists (confirmed) |
| **C API pattern** | `krun_add_*` builder calls store config on the ctx; a `krun_add_usb_*` would follow the same shape | `src/libkrun/src/lib.rs`, `include/libkrun.h` | pattern only |

The recently-added **display/input** subsystems are the *template* for how a
new host-backed device is wired through the C API and a host-side backend
object — worth mirroring for a future native USB device. Confirmed signatures:
`krun_add_display(ctx, width, height)` at `include/libkrun.h:629`;
`krun_add_input_device(ctx, config_backend, config_backend_size, ...)` at
`include/libkrun.h:722` (note the opaque `const void *config_backend` +
`size_t` pattern — exactly the shape a `krun_add_usb*` would copy); and the
fd-handoff variant `krun_add_input_device_fd(ctx, input_fd)` at
`include/libkrun.h:736`. The virtio-console fd/port API
(`krun_add_virtio_console_multiport` at `:1361`,
`krun_add_console_port_inout` at `:1384`) is the model for the serial shortcut
in Option G.

### 1.3 What Homebrew already gives the host

- **libusb** (installed): user-space USB on macOS via IOKit/IOUSBHost under the
  hood. This is the realistic backend for *either* a USB/IP server *or* a
  native virtio-USB host device. libusb on macOS supports control, bulk,
  interrupt, and isochronous transfers, plus **hotplug callbacks**
  (`libusb_hotplug_register_callback`) — though see §1.6 for the macOS hotplug
  caveat.
- **qemu** (installed): QEMU's `usb-host` (libusb-backed) and its USB/IP-ish
  paths are reference implementations we can read; QEMU also ships
  `hw/usb/host-libusb.c`, the canonical libusb-passthrough device.
- **No `usbipd` for macOS in Homebrew** by default. The Linux `usbip` userspace
  tools are Linux-only; the well-known cross-platform server is
  **`usbipd-win`** (Windows). On macOS we would either (a) port/use a libusb
  USB/IP server (several exist, e.g. `usbip-libusb` / `vhci`-compatible
  servers), or (b) write a small one. **[CONFIRM]** what, if anything, brew
  exposes; assume we ship our own server.

### 1.4 The Linux guest side (already upstream)

The Linux kernel already contains everything needed for the *guest* end of
USB/IP — this is the biggest reason USB/IP is the fast path:

| Guest kernel config | Role |
|---|---|
| `CONFIG_USBIP_CORE` | USB/IP core |
| `CONFIG_USBIP_VHCI_HCD` | **Virtual Host Controller** — makes remote USB devices appear as a local USB bus in the guest |
| `CONFIG_USBIP_VHCI_HC_PORTS` / `_NR_HCS` | port/host-controller counts |
| `CONFIG_USB` + class drivers | so the guest can actually *use* the attached device (storage, HID, serial, etc.) |

The guest userspace tool is `usbip` (from `linux/tools/usb/usbip`), which does
`usbip attach -r <host> -b <busid>` and pokes
`/sys/devices/platform/vhci_hcd.0/attach`. **The kernel bundled by libkrunfw
must have `VHCI_HCD` enabled** — this is the single most important guest-side
prerequisite.

> **CONFIRMED BLOCKER (worse than expected).** The libkrunfw kernel configs in
> this checkout set **`# CONFIG_USB_SUPPORT is not set`** in *every* arch
> profile: `config-libkrunfw_aarch64:2151`, `config-libkrunfw_x86_64:1389`,
> and the riscv64/sev/tdx/windows variants likewise. With `USB_SUPPORT` off,
> the guest kernel has **no USB subsystem at all** — not just a missing
> `VHCI_HCD`. So USB/IP cannot work until we rebuild libkrunfw's kernel with,
> at minimum: `CONFIG_USB_SUPPORT=y`, `CONFIG_USB=y`,
> `CONFIG_USBIP_CORE=m/y`, `CONFIG_USBIP_VHCI_HCD=m/y`, plus the desired
> `CONFIG_USB_*` class drivers (storage, HID, ACM/serial, etc.). libkrunfw is
> built from these config files via its `Makefile`, so this is a config edit +
> firmware rebuild, not a code change — but it **is** a hard prerequisite for
> the *entire* USB feature, every option below included. Note the configs *do*
> already carry `CONFIG_USB_OHCI_LITTLE_ENDIAN=y`, a stale leftover that is
> inert while `USB_SUPPORT` is off.

### 1.5 macOS device-claiming reality (the hard part)

To hand a USB device to the guest, the host process must take exclusive control
of it away from macOS's own drivers:

- **Two levels of claiming.** A device exposes a *device* object
  (`IOUSBHostDevice` / legacy `IOUSBDevice`) and one or more *interface*
  objects (`IOUSBHostInterface` / `IOUSBInterface`). macOS class drivers
  (`IOUSBHostHIDDevice`, `IOUSBMassStorageDriver`/`AppleUSBStorage`, CDC/ECM,
  audio, etc.) bind to **interfaces**, not the whole device.
- **libusb on macOS** opens the device via IOKit. For interfaces already bound
  by an Apple kernel driver, `libusb_claim_interface` will fail unless the
  kernel driver is detached. On Linux libusb can auto-detach
  (`libusb_set_auto_detach_kernel_driver`); **on macOS that call is a no-op /
  unsupported** — you cannot generically detach an arbitrary Apple driver from
  user space the way you can on Linux.
- **What actually works on macOS:**
  - **Whole-device capture** for devices with **no matching Apple driver** (or
    only a generic one): libusb can open and claim freely. Most "interesting"
    passthrough targets (FTDI/CP210x serial without the Apple CDC match,
    security keys, custom hardware, many cameras, SDR dongles) fall here.
  - For devices Apple *does* claim (mass storage, standard HID keyboards/mice,
    Apple-recognized audio), you must **prevent or undo the kernel match**:
    - **Code Signing / Entitlement:** to seize a device, the host process
      generally needs the entitlement
      `com.apple.developer.driverkit.allow-any-userclient-access` *or* the
      classic **`com.apple.vm.device-access`** entitlement (used by
      virtualization software for USB/IO passthrough). Parallels/VMware/UTM
      rely on privileged helpers + entitlements for this. **[CONFIRM]** exact
      entitlement string and whether it requires a special Apple grant
      (some are restricted/managed entitlements).
    - **DriverKit / `IOUSBHost`:** the modern path is a **DriverKit (.dext)**
      USB driver that matches the device with higher priority and exports it to
      user space, or using the `IOUSBHost` framework's
      `IOUSBHostDevice`/`...Interface` ObjC/Swift API with a matching dictionary
      that wins over Apple's. This needs the
      `com.apple.developer.driverkit.transport.usb` family of entitlements and
      App Store / notarization-grade signing.
    - **`kextunload`-style detach is not available** for in-kernel Apple USB
      drivers per-device; the supported lever is matching priority + DriverKit,
      or simply choosing devices Apple doesn't grab.
- **Bottom line:** for a v1, target the *libusb-claimable* device set
  (no-Apple-driver / generic devices). Mass storage and standard HID are
  explicitly harder and may require DriverKit + special entitlements.

### 1.6 macOS hotplug & transfer-type caveats

- **Hotplug:** libusb hotplug on macOS historically required the IOKit run loop
  to be serviced and had gaps; modern libusb supports it but **confirm with a
  spike** that `libusb_hotplug_register_callback` fires reliably for
  arrive/leave on macOS 26. Fallback: poll `libusb_get_device_list`.
- **Isochronous transfers** (webcams, USB audio): supported by libusb on macOS
  but timing-sensitive; over USB/IP, isochronous is the weakest area — the
  USB/IP protocol carries iso packets but latency/jitter across the vsock/net
  hop frequently breaks real-time audio/video. Treat **iso as out-of-scope for
  v1**.
- **Bulk/interrupt/control:** well supported by libusb on macOS and by USB/IP;
  these cover storage, serial, HID, security keys, printers, most "useful"
  passthrough.

---

## 2. How it works end to end

### 2.1 USB/IP path (recommended first)

```
 Linux guest                                 macOS host (limina process or helper)
 ┌───────────────────────┐                   ┌──────────────────────────────────┐
 │ app → class driver     │                  │ USB/IP server (Rust)              │
 │   (usb-storage/hid/...) │                 │   ├─ libusb_open(dev)             │
 │ usbcore                 │                 │   ├─ claim interface(s)           │
 │ vhci_hcd  ◄────URB────► │ USB/IP packets  │   ├─ submit transfers            │
 │ usbip-utils (attach)    │◄══════════════► │   └─ IOKit/IOUSBHost via libusb   │
 └─────────┬───────────────┘  vsock or TCP   └──────────────────────────────────┘
           │
           └─ virtio-vsock (preferred) or virtio-net (TCP) inside libkrun
```

Control/data flow:

1. **Enumeration.** The host server enumerates with libusb, builds USB/IP
   `OP_REP_DEVLIST` entries (busid, vendor/product, device descriptor summary).
2. **Attach.** Guest `usbip attach` (or our agent) opens a USB/IP connection,
   sends `OP_REQ_IMPORT` for a busid; server claims the device interfaces via
   libusb and replies `OP_REP_IMPORT`. `vhci_hcd` then exposes a new virtual
   USB port; usbcore enumerates the device *inside the guest* and binds the
   guest's class driver.
3. **I/O.** Every guest URB becomes a `USBIP_CMD_SUBMIT` PDU; the server
   translates it to a `libusb_submit_transfer` (sync or async). Completion comes
   back as `USBIP_RET_SUBMIT`. Unlinks map to `USBIP_CMD_UNLINK` /
   `USBIP_RET_UNLINK`.
4. **Transport.** Standard USB/IP is TCP/3240. We can run it over the guest's
   virtio-net IP, **or better** carry the same byte stream over **virtio-vsock**
   (no networking dependency, lower overhead, isolated). Carrying USB/IP over
   vsock requires either a tiny guest-side shim that bridges a vsock connection
   to `vhci_hcd`'s expected socket fd, or patching `vhci_hcd`'s attach to accept
   a vsock fd. The fd-handoff model is feasible because `vhci_hcd` attaches to a
   **socket fd**, and `AF_VSOCK` sockets are just fds — the kernel piece that
   matters is whether `vhci_hcd` validates the socket family.

### 2.2 Native virtio-USB path (later)

```
 Linux guest                       libkrun VMM                   macOS host
 ┌──────────────┐  virtio queues  ┌───────────────┐  libusb    ┌──────────┐
 │ usbcore       │◄──────────────►│ virtio-usb dev │◄─────────►│ IOUSBHost│
 │ (virtio HCD?) │  CMD/EVENT/     │ (new in src/   │  transfers │ device   │
 │  or vudc      │  DATA queues    │  devices)      │            └──────────┘
 └──────────────┘                 └───────────────┘
```

There is **no standard `virtio-usb` device** in the virtio spec the way there
is virtio-blk/net/gpu. Two sub-options:

- **(a) virtio-vhci / vUDC bridge:** reuse the USB/IP *protocol* but carry it
  on a dedicated virtio transport instead of vsock/TCP. The guest still uses
  `vhci_hcd`; libkrun implements a virtio device whose payload is USB/IP PDUs.
  This is "USB/IP without a socket" and is the cleanest incremental step from
  the recommended v1.
- **(b) a real virtio-usb HCD:** define a virtio device that the guest's USB
  core attaches to as a host controller. This requires a **guest kernel driver
  we write/patch** plus a matching libkrun device. Highest effort; only worth it
  if USB/IP's latency proves unacceptable.

### 2.3 macOS API layer (shared by both paths)

- **libusb** → **IOUSBHost.framework** (modern) / **IOKit IOUSBLib** (legacy)
  → IOUSBHostFamily kext. Open device, set configuration, claim interface,
  submit transfers, handle async completions on libusb's event thread (which
  must service the IOKit/CFRunLoop).
- The limina process must run an **event loop** for libusb async I/O
  (`libusb_handle_events` on a dedicated thread), bridged to the virtio/vsock
  worker.

---

## 3. Options inventory for limina

### Option A — Do nothing / reuse upstream
- **What:** ship without USB; rely on virtio-fs for file sharing, virtio-net for
  network devices, clipboard sharing for the common "move data" case.
- **Pros:** zero work; covers a surprising fraction of real needs (files,
  network printers as IPP, etc.).
- **Cons:** no security keys, no serial/embedded dev boards, no cameras, no
  USB storage hot-attach, no Android/iOS device tethering. A clear gap vs
  Parallels.
- **Verdict:** acceptable for first milestone (boot the Fedora image), not
  acceptable as a Parallels replacement long-term.

### Option B — USB/IP over virtio-vsock, host server in limina (RECOMMENDED v1)
- **What:** write a Rust USB/IP server in limina using **libusb** (the `rusb`
  crate), expose it on an `AF_VSOCK` listener via libkrun's existing vsock
  device; guest uses upstream `vhci_hcd` + a small attach helper/agent.
- **Pros:** guest side is **100% upstream kernel + standard usbip tool**;
  reuses libkrun's *existing* vsock device — **no libkrun VMM patch required**
  for transport; isolated from guest networking; well-trodden protocol;
  libusb is already installed.
- **Cons:** need `vhci_hcd` to accept a vsock fd (likely a tiny guest shim or a
  small `vhci_hcd` patch); macOS device-claiming limits (§1.5); iso transfers
  weak; must run a libusb event thread.
- **Constraint fit:** great on "willing to patch guest", great on "low effort",
  good on overhead.

### Option C — USB/IP over virtio-net (TCP/3240)
- **What:** identical server, but reachable over the guest's IP stack instead of
  vsock.
- **Pros:** *zero* guest patching — stock `usbip attach -r <hostip>` works as-is
  once networking is up; fastest possible spike.
- **Cons:** depends on working guest networking (gvproxy/passt host route to a
  host-only address); exposes a USB service on a network interface (security);
  slightly more overhead than vsock.
- **Verdict:** **the fastest prototype**; use it to validate the whole pipeline,
  then move transport to vsock for the shipping product.

### Option D — Native virtio device carrying USB/IP PDUs (mid-term)
- **What:** add a `virtio-usb` device to libkrun whose ring payload is the
  USB/IP wire protocol; guest binds it to `vhci_hcd` via a thin virtio glue
  driver.
- **Pros:** no socket/networking at all; tightest integration; clean device
  enumeration/hotplug semantics; can present per-device "ports".
- **Cons:** **patches libkrun** (new device, new `krun_add_usb*` C API) **and**
  needs a **guest driver** we write; more code than B/C.
- **Verdict:** the natural successor to B.

### Option E — Full native virtio-USB HCD (long-term, maybe never)
- **What:** a brand-new host-controller-class virtio device + guest HCD driver,
  not USB/IP-based.
- **Pros:** theoretically lowest latency; could support iso better.
- **Cons:** large new guest kernel driver + libkrun device; no upstream guest
  support; high maintenance. Only if D's latency is inadequate.

### Option F — vhost-user-usb
- **What:** implement USB as an out-of-process vhost-user backend attached to
  libkrun's vhost-user frontend.
- **Pros:** keeps USB/libusb complexity out of the VMM core; crash isolation;
  reuses libkrun's existing vhost-user frontend
  (`src/devices/src/virtio/vhost_user/`; the C API even enumerates vhost-user
  device type IDs — `KRUN_VIRTIO_DEVICE_CONSOLE/RNG/RTC` at
  `include/libkrun.h:742-744`).
- **Cons:** **no standard vhost-user-usb spec/device-type exists** — you'd be
  inventing one (effectively Option D delivered via vhost-user transport);
  vhost-user on macOS (shared-memory + eventfd emulation) is more friction than
  on Linux. Not a shortcut.

### Option G — Per-class shortcuts (complementary, not a USB story)
- **Serial:** expose host serial/USB-CDC as a **virtio-console**/PTY pair —
  trivial, covers FTDI/CP210x dev boards without any USB stack.
- **CCID/smartcard & FIDO:** a CCID/FIDO proxy agent could forward APDUs/CTAP
  over vsock without full device passthrough; large effort per class.
- **Mass storage:** mount on host, share via virtio-fs (already available).
- **Verdict:** ship serial-via-virtio-console early as a cheap win; treat the
  rest as USB/IP targets.

---

## 4. Recommendation

**Pursue Option C → B, with G (serial-via-virtio-console) as an early
complementary win, and D as the planned follow-on.**

1. **Spike with Option C (USB/IP over TCP):** stand up a Rust USB/IP server
   (built on the `rusb`/libusb crate) on the host, get the **libkrunfw guest
   kernel rebuilt with `CONFIG_USBIP_VHCI_HCD`**, and `usbip attach` a
   no-Apple-driver device (e.g. an FTDI dongle or a YubiKey) over the guest's
   IP. This proves enumeration + bulk/interrupt/control end to end with the
   least moving parts.
2. **Productize as Option B (USB/IP over vsock):** move the server onto an
   `AF_VSOCK` listener using libkrun's **existing** vsock device, and add a
   small **guest agent** that bridges the vsock connection into `vhci_hcd`
   (either a userspace fd-forwarder or a minimal `vhci_hcd` patch to accept
   `AF_VSOCK`). No VMM device patch needed for transport.
3. **Later, Option D:** if we want zero-config hotplug and no socket plumbing,
   add a dedicated `virtio-usb` device to libkrun carrying USB/IP PDUs + a thin
   guest driver, exposed via a new `krun_add_usb_*` C API mirroring the
   display/input device pattern.

**What must be patched / built:**

- **libkrunfw guest kernel:** enable `CONFIG_USBIP_CORE`,
  `CONFIG_USBIP_VHCI_HCD`, and the needed `CONFIG_USB_*` class drivers (storage,
  HID, serial/CDC-ACM, etc.). **(Required, blocking.)**
- **limina host:** new Rust module — libusb-backed USB/IP server (enumerate,
  claim, transfer translation, async event thread), device-selection UI/CLI,
  and (for B) a vsock listener.
- **Guest agent (for B):** vsock↔vhci_hcd bridge, or a small `vhci_hcd` kernel
  patch to accept an `AF_VSOCK` fd.
- **macOS signing/entitlements:** code-sign limina with the device-access
  entitlement needed to claim USB devices (§1.5); document the
  DriverKit/entitlement requirement for the harder Apple-claimed device classes
  as a *future* tier.
- **libkrun (only for D):** new `virtio-usb` device under
  `src/devices/src/virtio/usb/`, C API `krun_add_usb*`, build feature `usb`.

---

## 5. Open questions / things to prototype

1. **Guest kernel:** does the stock libkrunfw kernel have `VHCI_HCD`? If not,
   confirm the libkrunfw build lets us add it and that the resulting kernel
   still boots the Fedora image. **(Blocking; verify first.)**
2. **vsock fd into vhci_hcd:** will `vhci_hcd`'s attach accept an `AF_VSOCK`
   socket fd unmodified, or is a kernel patch / userspace bridge required?
   Prototype both the fd-forwarder and the patch.
3. **macOS claiming matrix:** empirically classify a set of real devices
   (FTDI, YubiKey, USB mass-storage stick, a webcam, a USB keyboard) into
   "libusb claims freely" vs "Apple driver blocks" — drives v1 scope.
4. **Entitlements:** determine the exact entitlement(s) needed (and whether they
   require an Apple-granted/managed entitlement) to claim Apple-bound interfaces
   without DriverKit; check what UTM/krunkit do today.
5. **Hotplug on macOS 26:** does `libusb_hotplug_register_callback` fire
   reliably for arrive/leave, or must we poll?
6. **Isochronous viability:** measure jitter for USB audio/webcam over
   USB/IP-via-vsock; decide whether iso is ever in scope.
7. **Throughput:** benchmark a USB3 mass-storage device over USB/IP-vsock vs
   virtio-fs; if virtio-fs wins decisively, deprioritize storage passthrough.
8. **libkrunfw build knobs:** confirm libkrunfw's Makefile/config layout lets us
   inject extra `CONFIG_USB*`/`CONFIG_USBIP*` symbols and that the bundled
   firmware blob is rebuildable from this checkout. (The C-API shape to mirror
   for `krun_add_usb*` is already settled — see `krun_add_input_device` at
   `include/libkrun.h:722`.)

---

## 6. References

Local source (read this session; line numbers confirmed):
- `include/libkrun.h:629` — `krun_add_display`; `:722` `krun_add_input_device`; `:736` `krun_add_input_device_fd`; `:1361`/`:1384` virtio-console multiport/inout-port. C-API template for a future `krun_add_usb*`. Confirmed: **no `usb` symbol** in the 1455-line header.
- `src/devices/src/virtio/` — full device set: `balloon block console fs gpu input net rng vsock vhost_user` + scaffolding (`mmio.rs queue.rs device.rs mod.rs descriptor_utils.rs`). Confirmed: **no `usb/`**.
- `src/devices/src/virtio/vsock/` — transport for Option B.
- `src/devices/src/virtio/vhost_user/` — for Option F evaluation.
- `src/libkrun/src/lib.rs` — `krun_add_*` builder pattern (ctx-stored config).
- `src/arch_gen/src/x86/bootparam.rs` — the **only** "usb" string match in the tree (unrelated x86 boot field).
- `~/Projects/limina/third_party/libkrunfw/` — guest kernel config; **still to verify** `CONFIG_USBIP_VHCI_HCD` (blocking, §5 #1).
- `include/libkrun_input.h` — input-device backend vtable, template for Option D.

External (could not fetch this session — verify URLs/details):
- Linux USB/IP protocol & `vhci_hcd`: `Documentation/usb/usbip_protocol.rst`, `drivers/usb/usbip/` in the kernel tree.
- libusb API + macOS backend notes: <https://libusb.info> (hotplug, iso, macOS caveats).
- `rusb` crate (Rust libusb bindings): <https://docs.rs/rusb>.
- Apple **IOUSBHost** framework & DriverKit USB transport entitlements: Apple Developer docs.
- QEMU `hw/usb/host-libusb.c` — reference libusb passthrough device.
- `usbipd-win` — reference USB/IP server architecture: <https://github.com/dorssel/usbipd-win>.
