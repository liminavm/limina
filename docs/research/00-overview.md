# limina — Architecture Overview

limina is a native macOS app that runs Linux desktop guests on Apple Silicon via
[libkrun](https://github.com/containers/libkrun) + Hypervisor.framework (HVF),
aiming to replace Parallels. This document is the executive synthesis of the
per-subsystem research under [`docs/research/`](.). Every claim here is distilled
from those docs, which carry the source `path:line` citations.

First milestone: boot `~/Projects/limina/Fedora-Workstation-43.raw` (a
60 GiB MBR image with an EFI partition) to a usable GNOME desktop.

---

## 1. The layered stack

```
+---------------------------------------------------------------------------+
|  limina.app  (Cocoa / AppKit, Swift+Rust)        host executable, code-signed|
|  - NSWindow + CAMetalLayer presenter           com.apple.security.hypervisor|
|  - NSEvent -> linux EV_* translation, keymap/Cmd-Option swap                |
|  - NSPasteboard <-> clipboard bridge (liminad)                                |
|  - lifecycle / supervises child VMM + gateway processes                     |
+----------------------------+----------------------------------------------+
                             | C ABI (krun_* , display/input vtables)
                             v
+---------------------------------------------------------------------------+
|  libkrun  (built from third_party, --features gpu,input,net,blk,...)        |
|                                                                            |
|  +-------------------+   +------------------------------------------------+ |
|  |  VMM control      |   |  virtio devices (worker threads)               | |
|  |  builder -> ctx   |   |  gpu | input | net | block | fs | vsock        | |
|  |  krun_start_enter |   |  console | rng | balloon | (snd: to build)     | |
|  +-------------------+   +------------------------------------------------+ |
|                             |            |              |                   |
|             rutabaga_gfx ----+            |              |                   |
|             (VirglRenderer)  |            |              |                   |
+------------------------------|------------|--------------|------------------+
        | dlopen               | display    | vsock IPC    | unixgram/stream
        v                      v vtable     v (AF_UNIX)    v (gvproxy)
+-----------------+  +----------------+ +-----------+ +------------------------+
| Hypervisor.fwk  |  | virglrenderer  | | liminad     | | gvproxy (user NAT)     |
| HVF (per-vCPU   |  | -> MoltenVK    | | clipboard | | vmnet helper (bridged, |
|  host threads,  |  | -> Metal       | | mem-policy| |   later, entitlement)  |
|  hv_gic GICv3)  |  | (Venus/Vulkan) | | agent     | |                        |
+-----------------+  +----------------+ +-----------+ +------------------------+
                                              ^
                                              | virtio-vsock (CID 2)
                                       +-------------------+
                                       |  Linux guest      |
                                       |  Fedora 43        |
                                       |  Mesa venus/zink  |
                                       |  limina-agent       |
                                       |  balloon driver   |
                                       +-------------------+
```

Process model: the **VMM runs in a dedicated child process** (krunkit-style),
because `krun_start_enter` loops forever and the guest's PSCI SYSTEM_OFF tears
the *whole process* down. The limina UI process supervises it and talks over the
vsock control plane + a shutdown eventfd.

---

## 2. The findings that shape everything

1. **libkrun is NOT headless.** It has a complete virtio-gpu (Venus/Vulkan via
   MoltenVK->Metal), a virtio-input vtable ABI, and a display-backend vtable.
   The single biggest *net-new* piece is a native macOS NSWindow + CAMetalLayer
   presenter — no libkrun patch needed for a first display. See
   [03](03-graphics-virtio-gpu-3d.md), [04](04-input-and-keyboard.md),
   [09](09-display-host-integration.md).

2. **The process dies on guest shutdown.** `krun_start_enter` never returns
   normally; guest PSCI SYSTEM_OFF -> `exit_evt` -> process teardown. limina must
   run the VMM as a child process and drive it via vsock + shutdown eventfd.
   See [01](01-libkrun-internals-and-api.md), [02](02-macos-hvf.md).

3. **Build our own libkrun.** The Homebrew 1.17.4 bottle works for spikes (gpu/
   input/net symbols confirmed) but lacks 1.18 APIs and, more importantly, every
   differentiating feature needs patches. Build from `third_party` with the
   right feature flags. See [01](01-libkrun-internals-and-api.md).

4. **Keep raw HVF; reject Virtualization.framework.** Vz is closed and forbids
   custom virtio devices, USB passthrough, fine-grained ballooning, and custom
   agents — and still needs the gated networking entitlement. libkrun's HVF
   backend (run loop, PSCI/SMP, in-kernel `hv_gic` GICv3, vtimer, WFI parking)
   already works. See [02](02-macos-hvf.md).

5. **Dynamic memory requires a libkrun patch.** A virtio-balloon device is
   attached unconditionally but only free-page-reporting is wired, the reclaim
   uses Linux `MADV_DONTNEED` (ineffective on macOS / wrong page granularity),
   and no `krun_*` API drives it. This is the hardest target feature. See
   [08](08-memory-and-dynamic.md).

6. **The guest kernel has USB entirely disabled** (`# CONFIG_USB_SUPPORT is not
   set` in every libkrunfw profile). USB passthrough is blocked until libkrunfw
   is rebuilt — and there is zero USB code in libkrun. See
   [06](06-usb-passthrough.md).

7. **Several "obvious" host paths are Linux-only on macOS:** the entire
   vhost-user path is `cfg(target_os="linux")` (so vhost-user audio/rtc/vsock do
   NOT work), and `krun_add_net_tap` is a macOS stub. Audio must be a *native*
   in-VMM virtio-snd device driving CoreAudio. See [11](11-audio-rosetta-misc.md),
   [07](07-networking.md).

8. **Rosetta is unavailable** (it's bound to Virtualization.framework). x86
   emulation must be guest-side via FEX-Emu / qemu-user-static. See
   [11](11-audio-rosetta-misc.md).

9. **A clean transport already exists for the control plane and clipboard:**
   virtio-vsock bridged to host AF_UNIX (`krun_add_vsock_port[2]`), coexisting
   with TSI networking, no patch needed. libkrun even does host->guest time sync
   over vsock port 123 already. See [10](10-guest-agent-and-vsock.md),
   [05](05-clipboard.md).

---

## 3. Feature -> status -> doc

| Feature | Exists in libkrun today? | What limina builds / patches | Doc |
|---|---|---|---|
| Boot Fedora .raw (M1) | EFI firmware + `krun_add_disk` path exists | EFI firmware sourcing; child-process VMM; boot bring-up | [01](01-libkrun-internals-and-api.md), [03](03-graphics-virtio-gpu-3d.md) |
| HVF backend / SMP / GIC | Yes — run loop, hv_gic, vtimer, WFI park | Harden panic paths; QoS hints; codesign exe | [02](02-macos-hvf.md) |
| Display / present | virtio-gpu + display vtable (CPU pull, full-frame) | **Native NSWindow + CAMetalLayer presenter** (biggest new piece) | [09](09-display-host-integration.md), [03](03-graphics-virtio-gpu-3d.md) |
| 3D accel | Venus->MoltenVK->Metal via rutabaga | Pass virgl flags; verify Apple blob patches in virglrenderer; defer zero-copy scanout | [03](03-graphics-virtio-gpu-3d.md) |
| Fullscreen / HiDPI | display geometry + EDID config (pre-boot only) | NSWindow toggleFullScreen; patch runtime resize/EDID/hotplug | [09](09-display-host-integration.md), [04](04-input-and-keyboard.md) |
| Mouse / abs+rel pointer | virtio-input vtable, shape set by capability bitmaps | Register abs tablet + rel mouse; switch on capture | [04](04-input-and-keyboard.md) |
| Keyboard + macOS combos | virtio-input vtable (verbatim events) | NSView events -> CGEventTap (toggle); host kVK->KEY_* table | [04](04-input-and-keyboard.md) |
| Keybindings / Cmd-Option swap | n/a (guest owns layout) | Host-side remap table edit | [04](04-input-and-keyboard.md) |
| Clipboard sharing | vsock<->AF_UNIX transport exists | limina-agent <-> liminad bridge; NSPasteboard polling; Wayland data-control | [05](05-clipboard.md) |
| USB passthrough | **None** (kernel USB disabled, no libkrun code) | Rebuild libkrunfw kernel; USB/IP over vsock; later native virtio-usb | [06](06-usb-passthrough.md) |
| NAT networking | virtio-net + unixgram/unixstream; TSI default | gvproxy via `krun_add_net_unixgram` + VFKIT; supervise gateway | [07](07-networking.md) |
| Bridged networking | virtio-net transport exists | vmnet helper + `com.apple.vm.networking` entitlement (later) | [07](07-networking.md) |
| Low memory overhead | demand-paged MAP_ANON guest RAM | Tune; 16 KiB-aware reclaim | [08](08-memory-and-dynamic.md) |
| Dynamic memory (balloon) | balloon device present, only reporting wired | **Patch:** fix reclaim (MADV_FREE_REUSABLE), implement inflate/deflate, add `krun_add_balloon` API + PSI agent | [08](08-memory-and-dynamic.md), [10](10-guest-agent-and-vsock.md) |
| Guest agent / control plane | vsock<->AF_UNIX, shutdown eventfd, timesync | limina-agent (vsock connect-out), CBOR protocol, virtiofs-overlay delivery | [10](10-guest-agent-and-vsock.md) |
| Audio | vhost-user-snd only (Linux-only path) | **Native in-VMM virtio-snd -> CoreAudio** | [11](11-audio-rosetta-misc.md) |
| x86 emulation | n/a (Rosetta unavailable) | Guest-side FEX-Emu / qemu-user-static (no libkrun patch) | [11](11-audio-rosetta-misc.md) |
| File sharing | virtiofs2/3 with DAX/shm (macOS-capable) | Use virtiofs + overlay APIs (also injects agent) | [11](11-audio-rosetta-misc.md), [10](10-guest-agent-and-vsock.md) |

---

## 4. Cross-cutting architectural decisions

| Decision | One-line rationale |
|---|---|
| Run the VMM in a dedicated **child process** | `krun_start_enter` loops forever and guest shutdown tears the process down; the UI must survive and supervise. |
| **Build libkrun from `third_party`** (gpu,input,net,blk,vhost-user) | Brew bottle lacks 1.18 APIs and every differentiator needs patches. |
| Keep **raw HVF via libkrun**, reject Virtualization.framework | Vz is closed and forbids the custom devices, USB, ballooning, and agents that are limina's whole point. |
| **Native AppKit UI** (NSWindow/CAMetalLayer/NSEvent), not GTK/SDL examples | Foreign event loops fight AppKit; examples are milestone-1 crutches only. |
| Single multiplexed **vsock control plane** (guest connects out) + shutdown eventfd | Coexists with TSI, needs no patch, and is the lifecycle/clipboard/mem channel. |
| **gvproxy user-mode NAT** as default networking | No root, no Apple-gated entitlement; bridged/vmnet is opt-in later. |
| **Codesign the limina executable** (not the dylib) with `com.apple.security.hypervisor` | HVF refuses to start without it; ad-hoc signing suffices, notarization-compatible. |
| **Mechanism in libkrun, policy in limina** (esp. balloon/PSI, keymap) | Keep patches minimal and upstreamable; behavior lives in the app. |
| Deliver the guest agent via **virtiofs overlay**, not editing the .raw | Keeps the user's disk image pristine. |
| **Defer** zero-copy scanout, hardware cursor, bridged net, USB, virtio-mem | All require patches/firmware rebuilds; not needed for milestone 1. |

---

## 5. Where to read next

- [01 — libkrun internals & API](01-libkrun-internals-and-api.md)
- [02 — macOS Hypervisor.framework](02-macos-hvf.md)
- [03 — Graphics: virtio-gpu & 3D](03-graphics-virtio-gpu-3d.md)
- [04 — Input & keyboard](04-input-and-keyboard.md)
- [05 — Clipboard](05-clipboard.md)
- [06 — USB passthrough](06-usb-passthrough.md)
- [07 — Networking](07-networking.md)
- [08 — Memory & dynamic ballooning](08-memory-and-dynamic.md)
- [09 — Display host integration](09-display-host-integration.md)
- [10 — Guest agent & vsock](10-guest-agent-and-vsock.md)
- [11 — Audio, Rosetta & misc](11-audio-rosetta-misc.md)
