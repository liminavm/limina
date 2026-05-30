# limina Architecture

Status: living design doc. Audience: limina contributors.
Scope: a native macOS (Apple Silicon) app that runs Linux desktop guests on top of a
**patched, vendored** libkrun, replacing Parallels for the author's workflow.

This document is decision-oriented. Every non-obvious claim traces back to the research
docs under `docs/research/` (cited as `[NN]` = `docs/research/NN-*.md`), which in turn cite
libkrun source at `path:line`. When this doc and a research doc disagree, the research doc's
source citation wins — open an issue.

Host baseline: macOS 26.5, Apple M1 Max, 32 GB, arm64, 16 KiB host pages, Rust 1.88
(edition 2024), full Xcode 26.4 / Apple clang 21. No nested virt (M1).

---

## 1. Guiding decisions (the load-bearing ones)

| # | Decision | Why | Source |
|---|----------|-----|--------|
| D1 | **Keep libkrun's HVF backend; patch it selectively (Option B).** Do not adopt Apple Virtualization.framework; do not rewrite the VMM. | Vz is closed/unpatchable and forbids limina's differentiators (custom virtio devices, USB passthrough, fine-grained ballooning, custom guest agent) and *still* needs `com.apple.vm.networking`/root for bridged net. libkrun's HVF run loop, PSCI/SMP bringup, in-kernel `hv_gic`, vtimer, WFI-parking already work. | [02] |
| D2 | **Vendor libkrun + libkrunfw + virglrenderer under `third_party/` and build our own**, applying a maintained patch series. The Homebrew 1.17.4 bottle lacks 1.18 APIs (overlay files, multiport console, vhost-user, `disable_implicit_init`) and we must patch anyway (balloon control, USB kernel, reclaim, runtime resize). | We can't ship features that need patches against a binary dylib. | [01][04] |
| D3 | **Run the VMM in a dedicated child process** ("vmm worker"), not in-process with the Cocoa UI. `krun_start_enter` loops forever and the guest-shutdown path calls `exit()` and tears the process down. | Verified chain: `krun_start_enter` → `loop { event_manager.run() }` (lib.rs:3032-3040); guest PSCI SYSTEM_OFF/RESET → `VcpuExit::Shutdown` → `self.exit(FC_EXIT_CODE_OK)` → `exit_evt` → process teardown. A GUI must not die when the guest powers off. | [01][02] |
| D4 | **The limina host executable carries the `com.apple.security.hypervisor` entitlement** (ad-hoc signable). Specifically, the *vmm worker* binary that calls `hv_vm_create`. Default networking is **gvproxy user-mode NAT** to avoid the Apple-gated `com.apple.vm.networking`. | Without the entitlement `hv_vm_create` → `Error::VmCreate`. Entitlement must be on the executable, not the dylib. | [02][07] |
| D5 | **Milestone-1 boot path: EFI firmware + `krun_add_disk`** of `Fedora-Workstation-43.raw` (60 GiB MBR image w/ EFI partition). Fallback: `krun_add_disk` + `krun_set_root_disk_remount`. | The distro boots its own kernel/drivers; avoids depending on libkrunfw's minimal kernel for a desktop guest. | [01][03] |
| D6 | **Display: native NSWindow + CAMetalLayer presenter implementing the verified `krun_display_backend` vtable** (`configure_scanout`/`disable_scanout`/`alloc_frame`/`present_frame`), in Rust via the in-tree `krun-sys` crate. No GTK/SDL in the product. | The display ABI is real and works; `alloc_frame` hands back a shared-storage `MTLBuffer.contents()` so on M1 UMA there is one copy total. | [03][09] |
| D7 | **Input: vtable path `krun_add_input_device`** (config-backend + event-provider `#[repr(C)]` structs), macOS UI thread → ring + readable fd → libkrun input worker. Guest owns the keyboard layout; host owns the `kVK_*`→`KEY_*` table and Command/Option swap. | The `_fd`/passthrough path is Linux-host only. Translation needs no libkrun patch. | [04] |
| D8 | **One multiplexed control connection over a single `krun_add_vsock_port` with the guest connecting out** (no `listen` flag), TSI left on. 16-byte `FrameHeader` + CBOR. This is the limina-agent ↔ liminad control plane. | IPC ports coexist with TSI with no patch (`unix_ipc_port_map` is separate from `tsi_flags`). | [05][10] |
| D9 | **Dynamic memory requires a libkrun patch.** Phase 0 ships static RAM. The balloon device exists but only free-page-reporting is wired, `num_pages`/`actual` are never set, and reclaim uses Linux `MADV_DONTNEED` which doesn't drop `phys_footprint` on macOS. | No public balloon API in `libkrun.h`. Highest-leverage fix: `MADV_FREE_REUSABLE` + 16 KiB alignment. | [08][10] |

---

## 2. Process & thread model

limina is **two processes** plus the guest:

```
                         macOS user session
  ┌───────────────────────────────────────────────────────────────────────┐
  │  limina (UI process)                     │  limina-vmm (worker process)     │
  │  ───────────────────                   │  ──────────────────────        │
  │  • Cocoa/AppKit main thread (NSApp)    │  • main thread: builds the     │
  │    - NSWindow + CAMetalLayer per       │    libkrun ctx, then calls      │
  │      scanout                           │    krun_start_enter() and       │
  │    - NSEvent capture, CGEventTap       │    BLOCKS forever               │
  │    - menu / prefs / fullscreen         │  • krun spawns:                 │
  │  • present thread (CVDisplayLink)       │    - N × fc_vcpu threads (HVF)  │
  │  • control-plane thread (liminad)         │    - gpu worker thread          │
  │    - vsock AF_UNIX <-> agent            │    - input worker thread        │
  │  • supervisor thread                    │    - virtio-net worker thread   │
  │    - spawns/monitors limina-vmm & gvproxy │    - virtiofs/vsock/balloon...  │
  └───────────────┬──────────────────────┘  └──────────────┬───────────────┘
                  │   AF_UNIX sockets / shared mem / pipes   │
                  └──────────────────────────────────────────┘
```

### 2.1 Why two processes (D3)

`krun_start_enter` never returns under normal operation and the guest power-off path calls
`exit()`. If the VMM ran inside the Cocoa app, a guest shutdown would kill the UI, and we'd
have no clean way to host the AppKit run loop alongside libkrun's blocking enter call.
The **vmm worker** is therefore a separate executable whose `main()` is essentially:

1. parse the resolved VM config (passed via fd/argv/env),
2. build the libkrun context (all `krun_*` device/config calls),
3. hand display/input backend FFI handles to libkrun,
4. call `krun_start_enter()` and block.

When the guest powers off, the worker process exits with `FC_EXIT_CODE_OK`; the UI's
**supervisor thread** observes the exit and transitions the window to a "powered off" state
(offer restart / close). When the user requests shutdown from the UI, liminad sends a
`SHUTDOWN` control message to the agent, with `krun_get_shutdown_eventfd` (host→guest
orderly shutdown signal) as the forcing fallback.

### 2.2 Threads inside the vmm worker (created by libkrun)

- **`fc_vcpu N`** — one host thread per vCPU; `hv_vcpu_create` is called on that thread. The
  HVF loop handles CANCELED/EXCEPTION/VTIMER exits; WFx parks the thread on a crossbeam
  channel (true sleep, no busy spin). **limina patch:** set `QOS_CLASS_USER_INTERACTIVE` on
  these threads (HVF gives no P/E pinning, only QoS hints). Default vCPU count =
  `min(krun_get_max_vcpus(), core_count)` (≤10 on M1 Max). [02]
- **gpu worker** — single thread; control + cursor virtqueues; runs the
  `configure/alloc/present` vtable calls **synchronously**. The present hop to the macOS main
  thread is the one cross-process/cross-thread handoff for frames. [03][09]
- **input worker** — epoll loop on a dedicated thread, copies events verbatim (no SYN
  auto-insert → the provider MUST emit `SYN_REPORT`). [04]
- **virtio-net worker**, **virtiofs worker**, **vsock muxer**, **balloon**, **rng**,
  **console** — per-device threads, all macOS-capable in current source. [07][10][11]

### 2.3 Threads inside the UI process (limina owns)

- **AppKit main thread**: `NSApplication`, all `NSWindow`/`CAMetalLayer`/`NSEvent` work.
  Frame *presentation* happens here (Metal present must be on main thread); a thread hop from
  the gpu worker is required and acceptable since the vtable calls are serialized.
- **present thread** (`CVDisplayLink`/`MTKView`): drives vsync-aligned `present`.
- **liminad control thread**: owns the AF_UNIX endpoint bridged to the guest vsock control
  port; runs the clipboard bridge (NSPasteboard changeCount polling), heartbeat, agent RPC.
- **supervisor thread**: spawns and monitors `limina-vmm` and `gvproxy`; restarts the net path
  on backend HANG_UP (libkrun's net worker permanently disables the NIC otherwise). [07]

### 2.4 Cross-process plumbing

The UI and vmm worker are connected by AF_UNIX sockets:
- **display backend channel**: the worker must call the display vtable, but the *window*
  lives in the UI process. Two viable models (decide during D6 spike):
  - **(A) Frames cross the process boundary** via an IOSurface/shared-memory handle: the vmm
    worker's `alloc_frame` returns a pointer into a shared region the UI process maps and
    presents. This is the long-term zero-copy direction. [09]
  - **(B) Co-locate the display backend in the worker**: run a minimal CAMetalLayer-bearing
    `NSWindow` *in the vmm worker* and let the UI process drive only chrome/prefs via IPC.
    Simpler for milestone-1 (no frame marshalling) but splits AppKit across two processes.
  - **Milestone-1 choice: (B)** — the vmm worker hosts the guest window directly; the UI
    process is thin. Migrate to (A) when zero-copy scanout lands.
- **input channel**: NSEvents captured in whichever process owns the window are translated
  and pushed into libkrun's event-provider ring. With model (B) for M1, input is captured in
  the worker; with model (A), input is captured in the UI and forwarded.
- **control plane**: liminad ⇄ guest agent over vsock (independent of which process owns the
  window).

> Trade-off note: model (B) temporarily puts UI code in the "privileged" (hypervisor-
> entitled) worker. That's acceptable for milestone-1; the long-term split (A) keeps the
> entitled worker headless and is the target once IOSurface zero-copy is in place.

---

## 3. Vendored, patched libkrun (build & patch-series strategy)

### 3.1 Layout

```
third_party/
  libkrun/          # upstream main ~v1.18 (VMM library)            [vendored, patched]
  libkrunfw/        # bundled guest kernel firmware                 [vendored, patched]
  virglrenderer/    # host GL/Vulkan renderer (Apple blob patches)  [vendored, patched]
  krunkit/          # reference only (vfkit-compatible REST front)   [read-only mirror]
```

We do **not** consume Homebrew's libkrun for the product (D2). Homebrew packages remain
useful as a reference oracle and for the very first link-and-smoke spike (the 1.17.4 bottle
*does* export the gpu/input/display symbols, gated by `krun_has_feature`). [01][04]

### 3.2 Build flags

Build libkrun with: `make GPU=1 INPUT=1 NET=1 BLK=1 VHOST_USER=1`.
(`VHOST_USER` is Linux-gated internally but harmless to request; audio will be a *native*
device, not vhost-user — see §8.) virgl flags passed at runtime via
`krun_set_gpu_options2`: `USE_EGL | VENUS | RENDER_SERVER | THREAD_SYNC |
USE_ASYNC_FENCE_CB` (matching in-tree `gui_vm` macOS config; do NOT set `NO_VIRGL`, `DRM`,
or `USE_EXTERNAL_BLOB`). [03]

The macOS→Linux init cross-compile is automatic (libkrun's Makefile downloads a Debian
sysroot and uses `clang -target aarch64-linux-gnu -fuse-ld=lld`); **requires Homebrew
`lld`**. [01]

libkrunfw: rebuild the guest firmware blob from edited kernel `.config` (it's a config-edit
+ firmware rebuild via libkrunfw's Makefile, not a code change). The first required edit is
**enabling USB** (`CONFIG_USB_SUPPORT=y`, `CONFIG_USB=y`, `CONFIG_USBIP_CORE`,
`CONFIG_USBIP_VHCI_HCD`, class drivers) — stock libkrunfw has USB entirely disabled in every
arch profile. [06] Note: for milestone-1 we boot the *Fedora* kernel via EFI (D5), so the
libkrunfw kernel only matters for features that need our firmware (USB, balloon behavior,
PSI, vsock). Audit `.config` for `DRM_VIRTIO_GPU`, `VIRTIO_INPUT`, `VIRTIO_BALLOON`,
`VSOCKETS`, `PSI`. [01][06][08][10]

### 3.3 Patch series

Patches live as an ordered, rebasable series so we can track upstream. **Use a `quilt`- or
`git-format-patch`-style series**, not a long-lived fork branch with merge commits:

```
third_party/libkrun/patches/
  series                              # ordered list, like quilt
  0001-vmm-set-vcpu-qos-interactive.patch
  0002-balloon-madv-free-reusable-16k.patch
  0003-balloon-implement-inflate-deflate.patch
  0004-balloon-public-krun-add-balloon-api.patch
  0005-display-runtime-reconfigure-edid-hotplug.patch
  0006-net-worker-reconnect-on-hangup.patch
  0007-input-statusq-led-feedback.patch
  0008-harden-panic-paths-psci-esr-exit.patch
  0009-snd-native-virtio-snd-coreaudio.patch
  0010-usb-virtio-usb-usbip-transport.patch      # later
third_party/libkrunfw/patches/
  0001-config-enable-usb.patch
  0002-config-enable-psi.patch
```

Conventions:
- Each patch is **single-purpose** and carries a header explaining *what upstream behavior it
  changes and why*, with the `path:line` it touches.
- A `make patch` / `make unpatch` wrapper (or a thin Rust xtask) applies/refreshes the series
  against a pinned upstream commit recorded in `third_party/libkrun/UPSTREAM_REV`.
- **Rebasing on upstream**: bump `UPSTREAM_REV`, `git rebase`/`quilt push -a`, fix
  rejects patch-by-patch, re-run the smoke suite. Patches that get upstreamed are deleted
  from `series`.
- Upstreaming priority (smallest/most-generic first): QoS hint, panic hardening, net
  reconnect, balloon `MADV_FREE_REUSABLE`. The public balloon API, runtime display
  reconfigure, native virtio-snd, and virtio-usb are larger and may stay downstream longer.

### 3.4 Confirmed patches needed (by feature)

| Patch | What | Source |
|-------|------|--------|
| vCPU QoS | `QOS_CLASS_USER_INTERACTIVE` on `fc_vcpu` threads | [02] |
| Balloon reclaim | `MADV_DONTNEED`→`MADV_FREE_REUSABLE`/`REUSE`, 16 KiB align/coalesce in `process_frq` | [08] |
| Balloon control | implement inflate/deflate handlers, set `num_pages`/`actual` + config-change IRQ, advertise `F_DEFLATE_ON_OOM`; add `krun_add_balloon(min,max,flags)`/`set_target`/`get_actual`/`get_stats` | [08][10] |
| Display resize | runtime display-reconfigure/EDID/hotplug C call raising the existing virtio-gpu config-change interrupt (plumbing, not new device) | [09] |
| Net robustness | reconnect (or limina-level restart) on backend HANG_UP | [07] |
| Input LEDs | surface statusq LED/key-repeat for CapsLock/NumLock parity (currently no-op) | [04] |
| Panic hardening | unknown PSCI fn / ESR_EL2 EC / HVF exit reason currently `panic!`; a real desktop guest must not crash the VMM | [02] |
| Native audio | new in-VMM virtio-snd → CoreAudio (fills empty `snd` feature, id 25) | [11] |
| USB | rebuild libkrunfw kernel w/ USB; native virtio-usb carrying USB/IP PDUs, `krun_add_usb*` | [06] |
| Zero-copy scanout (v2) | implement `SET_SCANOUT_BLOB` + IOSurface/Metal-texture export vtable | [03][09] |

---

## 4. limina crate / module layout

A Cargo workspace. Host crates build for macOS arm64; the agent crate builds for
`aarch64-unknown-linux-gnu` (Linux guest).

```
limina/                      (workspace root: Cargo.toml)
├─ third_party/            (vendored libkrun/libkrunfw/virglrenderer + patches)
├─ crates/
│  ├─ krun-sys/            FFI: raw bindings to the patched libkrun C API
│  │                       - generated/maintained extern "C" decls (incl. patched fns:
│  │                         krun_add_balloon, krun_display_reconfigure, krun_add_usb...)
│  │                       - the #[repr(C)] vtable structs: krun_display_backend,
│  │                         InputConfigBackend, InputEventProviderBackend
│  │                       - build.rs links the locally built libkrun.dylib (rpath)
│  │
│  ├─ limina-vmm/            the worker BINARY (entitled w/ com.apple.security.hypervisor)
│  │                       - main(): build ctx -> add devices -> krun_start_enter()
│  │                       - display/  CAMetalLayer presenter (impls display vtable) [M1: here]
│  │                       - input/    NSEvent capture -> event-provider ring
│  │                       - ipc/      AF_UNIX link to limina UI (lifecycle, chrome events)
│  │
│  ├─ limina/                the UI/control BINARY (front-end; user-facing app)
│  │                       - app/      NSApplication, menus, prefs, fullscreen, window mgmt
│  │                       - supervisor/  spawn+monitor limina-vmm & gvproxy; net restart
│  │                       - liminad/    vsock-bridged control plane + clipboard bridge
│  │                       - (display/input live here too once model (A) zero-copy lands)
│  │
│  ├─ limina-display/        shared: Metal presenter, scanout<->NSWindow map, format/HiDPI
│  │                       (used by limina-vmm now, by limina later)
│  ├─ limina-input/          shared: kVK_*->KEY_* keymap, Cmd/Option swap, abs+rel pointer,
│  │                       capture/grab state machine, CGEventTap toggle
│  ├─ limina-proto/          control-plane wire types: FrameHeader + CBOR message enums
│  │                       (shared by liminad host side and the guest agent)
│  ├─ limina-config/         VM config schema (serde), resolution/validation, paths
│  └─ limina-net/            gvproxy lifecycle, unixgram wiring, port-map/forward config
│
└─ agent/
   └─ limina-agent/          GUEST binary (aarch64-linux): vsock client, clipboard,
                           PSI/balloon reporter, time-sync consumer, shutdown handler
```

### 4.1 Module responsibilities (host)

- **`krun-sys`** — the *only* crate that talks `unsafe extern "C"`. It exposes the patched
  superset of libkrun's API. Critical ABI note: the input vtable args to
  `krun_add_input_device` are the Rust `#[repr(C)]` `InputConfigBackend` /
  `InputEventProviderBackend` structs (`features+userdata+PhantomData+create_fn+vtable`), NOT
  the `krun_input_config` header structs the doc comment names. Lay them out exactly, or use
  the upstream `krun_input` crate's `into_input_config`/`into_input_events`. The display
  backend is a zero-initialized `krun_display_backend` vtable passed by value + size to
  `krun_set_display_backend`. [04][09]
- **`limina-display`** — implements `configure_scanout` (reconcile the two sizes it receives:
  display config size from `krun_add_display` vs guest `SET_SCANOUT` resource size —
  letterbox vs scale-blit, interacting with `backingScaleFactor`), `disable_scanout`,
  `alloc_frame` (return a shared-storage `MTLBuffer.contents()` with **exactly width*4**
  bytes-per-row — `read_2d_resource` uses a tightly packed stride), `present_frame` (blit +
  publish; treat damage rect as an upload hint only). Multiplex up to 16 displays by
  `scanout_id` through the single backend, each mapped to its own NSWindow/CAMetalLayer.
  Format fast-path: B8G8R8A8/X8 → `.bgra8Unorm`. [03][09]
- **`limina-input`** — registers **two** pointer devices (absolute tablet
  `EV_ABS`+`INPUT_PROP_DIRECT`, max 32767; relative mouse `EV_REL`) plus a keyboard; default
  absolute for desktop, switch to relative on capture/grab. Host `kVK_*`→`KEY_*` table;
  Command/Option swap and custom keybindings are table edits. Window-only NSView events
  first; `CGEventTap` (Accessibility/TCC) behind a toggle to capture system combos. Provider
  emits explicit `SYN_REPORT`. [04]
- **`limina-net`** — spawns/wires gvproxy via `krun_add_net_unixunixgram(path, -1, mac,
  features, NET_FLAG_VFKIT)` (the new API, not krunkit's legacy `set_gvproxy_path`). Offloads
  start at `NET_COMPAT_FEATURES` (IPv4). Remember `krun_set_port_map` is TSI-only and EINVALs
  once a net device is added — port-forwarding for virtio-net goes through gvproxy's REST.
  [07]
- **`liminad`** (in `limina`) — owns the AF_UNIX endpoint bridged to the guest vsock control
  port; runs clipboard bridge (NSPasteboard changeCount polling + MIME↔UTI + promised data
  provider), heartbeat, and agent RPC dispatch. [05][10]
- **`limina-config`** — serde schema + resolution (see §7).
- **`supervisor`** (in `limina`) — process lifecycle for `limina-vmm` and `gvproxy`; restarts the
  net path on HANG_UP; surfaces guest power-off. [07]

### 4.2 The guest agent (`limina-agent`)

A single static `aarch64-linux` binary, shipped to the guest via a **virtiofs overlay**
(`krun_fs_add_overlay_file`/`_dir`) so the user's `.raw` is never modified, plus a per-user
systemd unit so it has the graphical session's Wayland socket. Responsibilities:
- **Control plane client**: `connect(AF_VSOCK, cid=2 /*host*/, LIMINA_CTRL_PORT)` (guest
  connects out; matches `test_vsock_guest_connect.rs`), HELLO/WELCOME cap negotiation,
  HEARTBEAT, SHUTDOWN/SHUTDOWN_ACK. [10]
- **Clipboard**: bridge Wayland `wlr/ext-data-control` (or XFIXES via XWayland fallback) ↔
  the control protocol; length-prefixed, serial-tagged, chunked (32–64 KiB) over vsock
  credit flow control; loop-prevention via last-value tracking. M1 text-only (may shell out
  to `wl-clipboard` to de-risk Wayland), M2 native data-control + images. [05]
- **Memory pressure reporter**: read `/proc/pressure/*` (needs `CONFIG_PSI=y`, `psi=1`) and
  feed limina's balloon target policy. The balloon *mechanism* is in libkrun (patched); the
  *policy* (min..max, watermarks, hysteresis) lives host-side. [08][10]
- **Time-sync consumer**: libkrun already pushes host→guest time on DGRAM vsock port 123;
  confirm/implement the guest-side consumer (`clock_settime`). [10]
- Later: binfmt_misc wiring for FEX-Emu/qemu-user x86 emulation; USB/IP `usbip attach`. [06][11]

---

## 5. FFI boundary (`krun-sys`)

- `krun-sys` is the single `unsafe` seam. All other host crates call safe Rust wrappers.
- It is built against the **locally built, patched** libkrun, with the rpath pointing at our
  `third_party/libkrun` output, so we never accidentally bind the Homebrew dylib.
- It declares the full patched superset: stock `krun_*` plus our additions
  (`krun_add_balloon`, `krun_balloon_set_target`, `krun_balloon_get_actual`,
  `krun_display_reconfigure`, `krun_add_usb*`, native-snd config). Each addition is gated so a
  build against unpatched upstream fails loudly rather than silently linking a missing symbol.
- libkrun is a **builder over an opaque `uint32 ctx_id`** (global non-recyclable map; panics
  on exhaustion). One ctx per VM; the vmm worker owns exactly one. [01]
- The display/input vtables are passed as `#[repr(C)]` structs; `krun-sys` owns these layouts
  (the highest-risk ABI footgun — see §4.1). [04][09]
- `krun_get_shutdown_eventfd` returns a host→guest eventfd-shim fd; on macOS wait on it via
  kqueue/pipe (no `eventfd(2)`). [10]

---

## 6. Boot flow (milestone-1)

```
limina UI
  └─ spawn limina-vmm (entitled) with resolved config fd
       1. krun_create_ctx
       2. krun_set_vm_config(ctx, vcpus, ram_mib)         # static RAM, Phase 0 [08]
       3. krun_set_firmware(<EFI firmware>)               # gated only by not(tee) [01]
       4. krun_add_disk(ctx, "Fedora-Workstation-43.raw") # EFI boots distro kernel [05]
       5. krun_add_vsock_port(ctx, LIMINA_CTRL_PORT, ctrl.sock)   # control plane [10]
       6. krun_add_net_unixgram(... gvproxy ... NET_FLAG_VFKIT) # NAT [07]
       7. krun_set_gpu_options2(VENUS|...); krun_add_display(...)# GPU + scanout [03]
       8. krun_set_display_backend(<CAMetalLayer vtable>)        # [09]
       9. krun_add_input_device(<kbd>); (...abs tablet); (...rel mouse)  # [04]
      10. krun_fs_add_overlay_* (limina-agent)              # inject agent, .raw untouched [10]
      11. krun_start_enter()   # BLOCKS; exit() on guest power-off
```

**Open boot risks to validate first (from research):** does the Fedora EFI/disk path boot
end-to-end; is the rootfs a plain partition vs btrfs/LVM (breaks the simple
`root_disk_remount` fallback); does Homebrew ship an EFI firmware for libkrun on macOS arm64
or must we build/source EDK2 (`libkrunfw-efi`); does Fedora 43 Mesa 25.2 auto-select Venus
and accelerate GNOME via zink-on-venus (else llvmpipe). [01][03]

---

## 7. VM config schema

`limina-config` defines a serde schema (TOML on disk, one file per VM). Sketch:

```toml
[vm]
name        = "fedora-43"
disk        = "~/Projects/limina/Fedora-Workstation-43.raw"
firmware    = "efi"                 # efi | bundled-kernel
vcpus       = 0                     # 0 = auto: min(krun_get_max_vcpus(), cores)

[memory]
min_mib     = 2048                  # balloon floor (Phase 1+; Phase 0 ignores min/max)
max_mib     = 16384                 # balloon ceiling == initial ram_mib
balloon     = true                  # requires patched libkrun [08]

[display]
scanouts    = 1                     # up to 16
hidpi       = true                  # contentsScale = backingScaleFactor
dpi         = 220
refresh_hz  = 60

[input]
swap_cmd_option = true              # Command<->Option remap [04]
capture_system_combos = false      # CGEventTap toggle (needs Accessibility) [04]
[input.keybindings]                 # custom kVK_* -> KEY_* overrides

[network]
mode        = "nat"                 # nat (gvproxy default) | bridged (vmnet, opt-in) [07]
[[network.port_forward]]            # gvproxy REST forwards (NAT mode)
host = 2222
guest = 22

[clipboard]
enabled       = true
guest_to_host = true
max_bytes     = 67108864

[usb]                               # later milestone [06]
enabled = false
```

Resolution rules: `vcpus=0` → query `krun_get_max_vcpus`; `firmware="efi"` requires a
located EFI firmware; `memory.max_mib` is the libkrun `ram_mib` and the balloon ceiling;
`balloon=true` errors if the linked libkrun lacks `krun_add_balloon` (unpatched build).

---

## 8. Feature → component map

| Target feature | Where it lives | Status / patch | Source |
|----------------|----------------|----------------|--------|
| **Boot Fedora .raw (M1)** | limina-vmm boot flow (EFI + `krun_add_disk`) | no patch | [01][05] |
| **3D acceleration** | virtio-gpu → rutabaga → virglrenderer **Venus** → MoltenVK → Metal; guest Mesa zink for desktop GL | no patch for present; vendored virglrenderer must carry Apple blob patches; v2 zero-copy needs `SET_SCANOUT_BLOB` patch | [03] |
| **Fullscreen** | `limina-display` / app: NSWindow `toggleFullScreen:` (Spaces, respects notch); CGDisplayCapture optional | no patch | [04][09] |
| **Mouse capture** | `limina-input`: abs↔rel switch + grab state machine | no patch | [04] |
| **macOS key-combo capture** | `limina-input`: `CGEventTap` behind a toggle (TCC) | no patch | [04] |
| **Custom keybindings + Cmd/Option swap** | `limina-input` keymap table; `limina-config` | no patch | [04] |
| **Clipboard sharing** | `liminad` (host NSPasteboard) ⇄ `limina-agent` (Wayland data-control) over vsock | no transport patch | [05][10] |
| **USB passthrough** | libkrunfw kernel rebuild (USB) + native virtio-usb / USB-IP transport + `krun_add_usb*`; host libusb claiming | **kernel rebuild + new device patch**; v1 = libusb-claimable devices only | [06] |
| **NAT networking** | `limina-net` + gvproxy (`unixgram`+VFKIT) | no patch (gvproxy is external) | [07] |
| **Bridged networking** | `limina-net` + vmnet helper (BRIDGED) | needs Apple `com.apple.vm.networking` + privileged helper; opt-in later | [07] |
| **Low memory overhead** | static `krun_set_vm_config` + demand paging; `MADV_FREE_REUSABLE` reclaim | reclaim patch | [08] |
| **Dynamic memory (min..max ballooning)** | `limina` balloon policy (PSI) + patched libkrun balloon + `limina-agent` reporter | **patch** (inflate/deflate, public API, 16 KiB align) | [08][10] |
| **Audio** | native in-VMM virtio-snd → CoreAudio | **patch** (fill `snd` feature, id 25) | [11] |
| **x86 emulation** | guest-side FEX-Emu (primary) / qemu-user (fallback), binfmt_misc | no libkrun patch (Rosetta unavailable: it's Vz-only) | [11] |
| **File sharing** | `krun_add_virtiofs3` + DAX/shm window | no patch (macOS-capable) | [11] |
| **Time sync** | libkrun's existing DGRAM port-123 push + guest consumer | confirm guest consumer | [10] |
| **Orderly shutdown** | liminad `SHUTDOWN` msg + `krun_get_shutdown_eventfd` fallback | no patch | [10] |

---

## 9. Build, codesign & entitlement flow

```
┌─ third_party build ─────────────────────────────────────────────────────┐
│  brew deps: lld, cmake, meson, ninja, pkg-config, dtc, glib, pixman,     │
│             molten-vk, vulkan-loader, libepoxy, virglrenderer deps        │
│  1. apply patch series (libkrun, libkrunfw, virglrenderer)               │
│  2. build virglrenderer (Apple blob patches) -> libvirglrenderer.dylib   │
│  3. build libkrunfw (edited .config) -> guest firmware blob              │
│  4. make GPU=1 INPUT=1 NET=1 BLK=1 VHOST_USER=1 -> libkrun.dylib         │
│     (auto-downloads Debian sysroot; cross-compiles Linux init via lld)    │
└─────────────────────────────────────────────────┬───────────────────────┘
                                                   │ (rpath)
┌─ cargo build ───────────────────────────────────▼───────────────────────┐
│  krun-sys links third_party/libkrun/.../libkrun.dylib                    │
│  cargo build -p limina-vmm   (the entitled worker)                         │
│  cargo build -p limina       (UI/control front-end)                        │
│  cross: cargo build -p limina-agent --target aarch64-unknown-linux-gnu     │
└─────────────────────────────────────────────────┬───────────────────────┘
                                                   │
┌─ codesign ──────────────────────────────────────▼───────────────────────┐
│  codesign --entitlements hvf-entitlements.plist --sign - \              │
│           target/.../limina-vmm          # com.apple.security.hypervisor    │
│           ^ MUST be on the executable that calls hv_vm_create (the       │
│             worker), NOT on libkrun.dylib. Ad-hoc (--sign -) suffices    │
│             and is notarization-compatible.                              │
│  limina (UI) needs NO hypervisor entitlement (it never calls HVF).        │
│  Later: bridged net adds com.apple.vm.networking (Apple-managed) to a    │
│         privileged helper, not to limina itself.                          │
└──────────────────────────────────────────────────────────────────────────┘
```

`hvf-entitlements.plist` (minimum):
```xml
<plist><dict>
  <key>com.apple.security.hypervisor</key><true/>
</dict></plist>
```

Without the entitlement on `limina-vmm`, `hv_vm_create` returns `Error::VmCreate`. Default
networking (gvproxy) needs **no** entitlement and **no** root. USB passthrough may later need
a device-access entitlement / DriverKit `.dext` for Apple-claimed interfaces; v1 scopes to
libusb-claimable devices to avoid that. [02][06][07]

---

## 10. Milestone roadmap

- **M1 — Boot.** EFI + `krun_add_disk` of the Fedora `.raw`; CAMetalLayer display backend
  (model B, in-worker window); abs-tablet + keyboard input; gvproxy NAT; vsock control plane
  with HELLO/HEARTBEAT/SHUTDOWN; static RAM. Codesigned `limina-vmm`.
- **M2 — Desktop usable.** Venus 3D verified (zink-on-venus, not llvmpipe); relative-mouse +
  capture/grab; CGEventTap system-combo capture; Cmd/Option swap + custom keybindings;
  text clipboard; fullscreen + HiDPI; net-worker reconnect; panic hardening.
- **M3 — Differentiators.** Dynamic memory (balloon patch + PSI policy + 16 KiB reclaim);
  native virtio-snd→CoreAudio; runtime display resize patch; image clipboard; virtiofs file
  sharing; FEX x86.
- **M4 — Advanced.** USB passthrough (libkrunfw USB rebuild → USB/IP → native virtio-usb);
  bridged networking (vmnet); zero-copy IOSurface scanout (move to display model A).

---

## 11. Top open questions feeding back into design

These gate or reshape decisions above; track in issues:
1. EFI firmware for libkrun on macOS arm64 — Homebrew-provided or must we build EDK2? (gates M1) [01][03]
2. Fedora `.raw` boots end-to-end via EFI/disk; rootfs layout (btrfs/LVM) for the remount fallback. [01][03]
3. Does the input worker (epoll/eventfd shim) build/run on macOS arm64 with `--features input`; what fd does `get_ready_efd` return (pipe read-end?). [04]
4. Display model A vs B long-term: IOSurface ⇄ virglrenderer/MoltenVK interop feasibility before committing to zero-copy export. [03][09]
5. Does `MADV_FREE_REUSABLE` on the HVF-mapped `MAP_ANON` region actually drop `phys_footprint` while it stays `hv_vm_map`'d? (gates dynamic memory) [08]
6. 4 KiB-guest vs 16 KiB-host: does Fedora report free pages in ≥16 KiB-aligned runs, or do we need a 16 KiB guest granule / guest patch? [08]
7. gvproxy speaks the `VFKT` dgram handshake `unixgram.rs` sends; outbound DHCP+curl works. [07]
8. GNOME/Mutter Fedora 43 implements `wlr/ext-data-control` so an unfocused agent can read/set the clipboard. (clipboard #1 blocker) [05]
9. Native virtio-snd spike: card enumerates in guest and plays a tone via a CoreAudio AudioUnit; latency vs Parallels. [11]
10. libkrunfw kernel rebuild with USB still boots within memory/boot constraints; vhci_hcd accepts an AF_VSOCK fd or needs a userspace bridge. [06]

---

### Appendix A — verified libkrun facts this design relies on

- `krun_start_enter` → `loop { event_manager.run() }` (lib.rs:3032-3040); guest power-off →
  `VcpuExit::Shutdown` → `self.exit(...)` → `exit_evt` → process teardown. (D3) [01]
- HVF dlopen'd at runtime; vCPU loop handles CANCELED/EXCEPTION/VTIMER; WFx parks (no busy
  spin); in-kernel `hv_gic` GICv3 is the **default** (not a patch we need). [02]
- virtio-gpu is real; display vtable `configure/disable/alloc/present` confirmed in
  `virtio_gpu.rs:518-536`; CPU pull model, full-frame, width*4 stride; no cursor queue / no
  zero-copy yet. [03][09]
- virtio-input C ABI: `krun_add_input_device(config_backend, size, event_provider, size)`
  with `#[repr(C)]` backend structs; worker copies events verbatim (emit `SYN_REPORT`); `_fd`
  path Linux-only. [04]
- vsock IPC ports (`unix_ipc_port_map`) coexist with TSI; guest connects out to host CID 2.
  [05][10]
- No USB anywhere; libkrunfw kernel has USB fully disabled in every profile. [06]
- Two net architectures (TSI default; virtio-net via unixstream/unixgram/tap); `unixgram`+
  `NET_FLAG_VFKIT` for gvproxy; `set_port_map` is TSI-only. [07]
- Balloon attached unconditionally; only free-page-reporting wired; `num_pages`/`actual`
  never set; reclaim is Linux `MADV_DONTNEED`. No public balloon API. [08][10]
- vhost-user path is `cfg(linux)` + memfd-dependent → audio must be a native virtio-snd
  device; Rosetta is Vz-only → x86 via guest FEX/qemu. [11]
