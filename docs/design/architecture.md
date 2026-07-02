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

> **D0 — Two-tier guarantee (governs everything below).** limina must *always* boot an
> **unmodified stock distro** (Fedora's own kernel, stock Mesa, no limina guest components)
> on **upstream-shaped libkrun** as a usable—if degraded—baseline; missing limina features
> degrade gracefully and never make a stock guest fail. Our custom kernel / drivers /
> `limina-agent` + patched libkrun are an **additive enhanced tier** (full perf+features),
> never a precondition for the VM to run. We are bound to neither Fedora's stock kernel nor
> libkrun's defaults. Every decision below must preserve a stock-guest fallback path. See
> [CLAUDE.md](../../CLAUDE.md) and [00-overview §2](../research/00-overview.md). The M1 boot
> is the permanent compatibility floor; later milestones may not regress it.

| # | Decision | Why | Source |
|---|----------|-----|--------|
| D1 | **Keep libkrun's HVF backend; patch it selectively (Option B).** Do not adopt Apple Virtualization.framework; do not rewrite the VMM. | Vz is closed/unpatchable and forbids limina's differentiators (custom virtio devices, USB passthrough, fine-grained ballooning, custom guest agent) and *still* needs `com.apple.vm.networking`/root for bridged net. libkrun's HVF run loop, PSCI/SMP bringup, in-kernel `hv_gic`, vtimer, WFI-parking already work. | [02] |
| D2 | **Vendor libkrun + libkrunfw + virglrenderer under `third_party/` and build our own**, applying a maintained patch series. The Homebrew 1.17.4 bottle lacks 1.18 APIs (overlay files, multiport console, vhost-user, `disable_implicit_init`) and we must patch anyway (balloon control, USB kernel, reclaim, runtime resize). | We can't ship features that need patches against a binary dylib. | [01][04] |
| D2.1 | **Consume libkrun through its internal Rust crates (`krun-vmm`/`krun-devices`/`krun-polly`/`krun-utils`/`krun-hvf`), NOT the C ABI.** `limina-vmm` builds a `VmResources`, calls `vmm::builder::build_microvm`, and runs its own `EventManager` loop — no `krun-sys`, no FFI. **Spike-validated** (`spikes/m1-boot-internal`): boots Fedora-43 to systemd userspace via pure Rust, at parity with the C-ABI spike. | Compile-time type safety (typed config structs/enums instead of `u32`-over-FFI; display/input become native Rust traits, not `#[repr(C)]` vtables); full control of the run loop and direct device/memory access. Cost: we reimplement libkrun's `ctx_cfg → VmResources` orchestration (becomes our policy, in `limina-vmm`) and track internal-API drift on rebase — judged acceptable since we own the fork and our usage is concentrated. | [01], `spikes/m1-boot-internal` |
| D3 | **Run the VMM in a dedicated child process** ("vmm worker"), not in-process with the Cocoa UI. Even though we now own the event loop (D2.1), the guest-shutdown path still calls `libc::exit()` and tears the process down. | Verified chain: guest PSCI SYSTEM_OFF/RESET → `VcpuExit::Shutdown` → `exit_evt` → `Vmm::process` → **`Vmm::stop` → `libc::exit`** — and that exit lives **inside the `krun-vmm` crate** (`src/vmm/src/lib.rs:361`), not the C entrypoint, so it bites the internal-API path too. A GUI must not die when the guest powers off. In-process control is possible later but requires a deliberate patch to `Vmm::stop`/the exit `Subscriber` — not on the critical path. | [01][02], `spikes/m1-boot-internal` |
| D4 | **The limina host executable carries the `com.apple.security.hypervisor` entitlement** (ad-hoc signable). Specifically, the *vmm worker* binary that calls `hv_vm_create`. Default networking is **gvproxy user-mode NAT** to avoid the Apple-gated `com.apple.vm.networking`. | Without the entitlement `hv_vm_create` → `Error::VmCreate`. Entitlement must be on the executable, not the dylib. | [02][07] |
| D5 | **Milestone-1 boot path: EFI firmware + `krun_add_disk`** of `Fedora-Workstation-43.raw` (60 GiB MBR image w/ EFI partition). Fallback: `krun_add_disk` + `krun_set_root_disk_remount`. | The distro boots its own kernel/drivers; avoids depending on libkrunfw's minimal kernel for a desktop guest. | [01][03] |
| D6 | **Display: native NSWindow + CAMetalLayer presenter implementing libkrun's display backend in native Rust** (`VmResources.display_backend: DisplayBackend`; methods `configure_scanout`/`disable_scanout`/`alloc_frame`/`present_frame`). Per D2.1 we implement the Rust backend type directly — **not** the C `#[repr(C)]` `krun_display_backend` vtable. No GTK/SDL in the product. | The backend works; `alloc_frame` hands back a shared-storage `MTLBuffer.contents()` so on M1 UMA there is one copy total. Implementing in Rust drops the vtable-ABI footgun entirely. | [03][09] |
| D7 | **Input: native Rust backends via `VmResources.input_backends`** (config + event-provider), macOS UI thread → ring + readable fd → libkrun input worker. Per D2.1 these are the Rust backend types, not the C `#[repr(C)]` `InputConfigBackend`/`InputEventProviderBackend` structs. Guest owns the keyboard layout; host owns the `kVK_*`→`KEY_*` table and Command/Option swap. | The `_fd`/passthrough path is Linux-host only. Translation needs no libkrun patch; the Rust path removes the worst documented ABI footgun. | [04] |
| D8 | **One multiplexed control connection over a single `krun_add_vsock_port` with the guest connecting out** (no `listen` flag), TSI left on. 16-byte `FrameHeader` + CBOR. This is the limina-agent ↔ liminad control plane. | IPC ports coexist with TSI with no patch (`unix_ipc_port_map` is separate from `tsi_flags`). | [05][10] |
| D9 | **Dynamic memory requires a libkrun patch — feasibility now proven.** Phase 0 ships static RAM. The balloon device exists but only free-page-reporting is wired, `num_pages`/`actual` are never set, and reclaim uses `MADV_DONTNEED`. **Spike confirmed** (macOS 26.5/M1 Max): `hv_vm_map` does not pin pages — reclaim works on the live mapped region with no unmap — and `MADV_FREE_REUSABLE` drops `phys_footprint` fully (`MADV_DONTNEED` returns nothing). | No public balloon API in `libkrun.h`. Fix: `MADV_FREE_REUSABLE` + 16 KiB align (4K-guest/16K-host page menu), inflate/deflate, public API, PSI policy. | [08][10], `spikes/balloon-madvise` |

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

Our run loop (`loop { event_manager.run() }`, D2.1) blocks forever, and the guest power-off
path calls `libc::exit()` from inside `krun-vmm` (`Vmm::stop`, D3). If the VMM ran inside the
Cocoa app, a guest shutdown would kill the UI, and we'd have no clean way to host the AppKit
run loop alongside libkrun's blocking event loop. The **vmm worker** is therefore a separate
executable whose `main()` is essentially:

1. parse the resolved VM config (passed via fd/argv/env),
2. assemble a `VmResources` (device/config) via the `krun/` facade,
3. install the native Rust display/input backends on the `VmResources`,
4. `vmm::builder::build_microvm(...)` then `loop { event_manager.run() }` and block.

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
  lives in the UI process. Two models were weighed during D6:
  - **(A) Frames cross the process boundary** via an IOSurface handle: the worker allocates
    IOSurface-backed scanouts and the UI process maps and presents them. Zero-copy. [09]
  - **(B) Co-locate the display backend in the worker**: run a minimal CAMetalLayer-bearing
    `NSWindow` *in the vmm worker* and let the UI process drive only chrome/prefs via IPC.
    Simpler bootstrap (no frame marshalling) but splits AppKit across two processes.
  - **As shipped: (A).** Model (B) was only ever a bootstrap crutch; the shipped design is
    the supervisor-owned window (`crates/limina/src/window.rs`, layer-hosting CALayer whose
    `contents` is the guest IOSurface) fed by the worker over a scanout socketpair, with
    non-global IOSurfaces crossed by Mach port (`limina-surfaceport`, 2026-06-23) and
    `shown` acks flowing back on the same fd. The entitled worker is headless.
- **input channel**: NSEvents are captured in the UI process (window + capture CGEventTap)
  and forwarded as evdev triples over three SOCK_DGRAM socketpairs (keyboard, absolute
  pointer, relative pointer) into the worker's virtio-input fd backends.
- **control plane**: supervisor ⇄ guest agent over vsock (independent of which process owns
  the window).

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

> **As-built deltas (2026-07-01).** This section is the founding plan; the shipped layout
> differs in named places — trust the workspace over this tree:
> - No `limina-net` crate: gvproxy lifecycle lives in the supervisor as
>   `crates/limina/src/gateway.rs`. No host `limina-config` crate yet: the worker's own
>   vocabulary is `crates/limina-vmm/src/config.rs`; the per-VM persistent config is designed
>   in `design/vm-definitions.md`.
> - No `liminad` module: the control plane is `crates/limina/src/control.rs` (+
>   `clipboard.rs`, `balloon_policy.rs`).
> - The guest side is `guest/` (its own workspace, `aarch64-unknown-linux-musl`):
>   `limina-agent`, `limina-agent-session` (clipboard), `limina-init` (L1 test init),
>   `limina-mock-mutter`, `limina-config`.
> - Extra shipped crates the plan didn't name: `limina-surfaceport` (Mach-port IOSurface
>   hand-off), `limina-usbip` (M7), `limina-test` (the L1/L2 harness).
> - Display model (A) shipped (supervisor-owned window; see §2.4) — `limina-display` holds
>   the worker-side backends (IOSurface ring + PNG capture oracle), not the presenter.

A Cargo workspace. Host crates build for macOS arm64; the agent crate builds for
`aarch64-unknown-linux-gnu` (Linux guest).

```
limina/                      (workspace root: Cargo.toml)
├─ third_party/            (vendored libkrun/libkrunfw/virglrenderer + patches)
│                          consumed as path-dep crates: krun-vmm, krun-devices,
│                          krun-polly, krun-utils, krun-hvf, krun-display, krun-input
├─ crates/
│  ├─ limina-vmm/            the worker BINARY (entitled w/ com.apple.security.hypervisor)
│  │                       - depends DIRECTLY on the vendored krun-* crates (no C ABI,
│  │                         no krun-sys); see D2.1
│  │                       - main(): build VmResources -> add devices ->
│  │                         vmm::builder::build_microvm -> loop { event_manager.run() }
│  │                       - krun/    thin limina-side facade: VmResources assembly + the
│  │                         ctx-orchestration we reimplement (cmdline, device order,
│  │                         firmware/krunfw, vsock/net heuristics) — our policy layer
│  │                       - display/  CAMetalLayer presenter (impls the Rust display backend) [M1: here]
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

- **libkrun integration (no `krun-sys`)** — `limina-vmm` depends on the vendored `krun-vmm`,
  `krun-devices`, `krun-polly`, `krun-utils`, `krun-hvf` crates by path and calls their Rust
  APIs directly (D2.1). The `krun/` facade module inside `limina-vmm` is the one place that
  assembles a `VmResources` and reimplements the orchestration that lived in libkrun's C
  entrypoint (`src/libkrun/src/lib.rs`): kernel-cmdline assembly, device ordering, firmware/
  krunfw selection, vsock/net heuristics. Input/display backends are the **native Rust**
  backend types (`VmResources.input_backends`, `VmResources.display_backend`) — no
  `#[repr(C)]` vtables, so the documented input-ABI footgun is gone. `unsafe` now lives only
  where it always did (inside libkrun: HVF, `libc`, `mmap`), not at a limina FFI seam. [04][09]
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

## 5. libkrun integration (internal Rust API, D2.1)

- **No FFI seam.** `limina-vmm` depends on the vendored `krun-*` crates by path and calls Rust
  directly; there is no `krun-sys` and no `unsafe extern "C"` in limina. The only `unsafe` is
  the HVF/`libc`/`mmap` code *inside* libkrun, which we already vendor.
- **We construct `VmResources` ourselves** (public `Default` + setters + fields:
  `set_vm_config`, `set_firmware_config`, `add_block_device`, `display_backend`,
  `input_backends`, `serial_consoles`, `disable_implicit_console`, …) and drive
  `vmm::builder::build_microvm(&vmr, &mut event_manager, shutdown_efd, worker_tx)
  -> Arc<Mutex<Vmm>>`, then run our own `polly::EventManager` loop. No opaque `ctx_id` — we
  hold the typed `VmResources`/`Vmm` directly. [01], `spikes/m1-boot-internal`
- **Our patches are now Rust edits to the vendored crates**, not added C functions: balloon
  target/inflate (`krun-devices` + a host policy API in our `krun/` facade), runtime display
  reconfigure, USB, native virtio-snd. Keep them small/upstreamable (mechanism in the crate,
  policy in limina) so rebases stay cheap.
- **Rebase discipline:** internal APIs carry no stability guarantee. Concentrate all calls
  into `limina-vmm`'s `krun/` facade so an upstream signature change touches one module. Pin the
  vendored libkrun commit; bump deliberately.
- Guest-shutdown eventfd: `build_microvm` takes the `shutdown_efd` directly (an `EventFd`); on
  macOS wait on it via kqueue/pipe (no `eventfd(2)`). [10]

---

## 6. Boot flow (milestone-1)

Internal Rust API (D2.1) — all via our `krun/` facade assembling one `VmResources`:

```
limina UI
  └─ spawn limina-vmm (entitled) with resolved config fd
       let mut vmr = VmResources::default();
       1. vmr.set_vm_config(&VmConfig{ vcpu_count, mem_size_mib, .. })  # static RAM, Phase 0 [08]
       2. vmr.set_firmware_config(FirmwareConfig{ path: <EFI fw> })     # Payload::Firmware [01]
       3. vmr.add_block_device(BlockDeviceConfig{ ImageType::Raw, .. }) # EFI boots distro kernel [05]
       4. vmr.set_vsock_device(VsockDeviceConfig{ LIMINA_CTRL_PORT, .. }) # control plane [10]
       5. vmr.add_network_interface(... gvproxy ... VFKIT)             # NAT [07]
       6. vmr.set_gpu_virgl_flags(VENUS|...); vmr.displays.push(..)     # GPU + scanout [03]
       7. vmr.display_backend = Some(DisplayBackend(<CAMetalLayer>))    # native Rust [09]
       8. vmr.input_backends.push(kbd); (abs tablet); (rel mouse)       # native Rust [04]
       9. vmr.add_fs_device(<overlay: limina-agent>)                      # inject agent, .raw untouched [10]
      ── build_microvm(&vmr, &mut em, shutdown_efd, tx); loop { em.run() }  # BLOCKS; libc::exit on power-off
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
│     (NB: we do NOT `make` the libkrun cdylib — cargo compiles krun-vmm    │
│      et al. from source as path-deps; only the C artifacts above are      │
│      prebuilt, linked by krun-rutabaga / loaded at runtime)               │
└─────────────────────────────────────────────────┬───────────────────────┘
                                                   │ (link/load C libs)
┌─ cargo build ───────────────────────────────────▼───────────────────────┐
│  limina-vmm depends on vendored krun-* crates directly (no krun-sys, D2.1)  │
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

The canonical, milestone-by-milestone plan lives in **[../roadmap.md](../roadmap.md)** (M1
boot → M8 polish), with per-milestone goals, libkrun patches, done-tests, and spikes. It is
the single source of truth for sequencing; do not maintain a parallel scheme here.

This doc's job is the *architecture* those milestones build on. The mapping at a glance:

| Roadmap milestone | Architecture pieces it exercises |
|---|---|
| **M1** boot to serial console | child-process `limina-vmm` (§2), EFI+disk boot flow (§6), codesign (§9), internal-API integration (§5, D2.1) |
| **M2** display + input | display model B in-worker window (§2.4), `limina-display`/`limina-input` (§4.1), vsock control plane (D8) |
| **M3** networking | `limina-net` + gvproxy supervision (§4.1), net-worker HANG_UP patch (§3.4) |
| **M4** 3D + zero-copy scanout | virgl flags (§3.2), virglrenderer Apple-blob build, `SET_SCANOUT_BLOB` patch + display model A migration (§2.4) |
| **M5** clipboard + virtiofs + agent | `liminad` bridge (§4.1), `limina-agent` (§4.2), virtiofs-overlay delivery |
| **M6** dynamic memory | balloon patch series (§3.4 D9), PSI policy host-side, page-size menu |
| **M7** USB | libkrunfw USB rebuild + native virtio-usb (§3.4), privileged-helper/entitlement (§9) |
| **M8** audio + x86 + polish | native virtio-snd→CoreAudio (§3.4), FEX wiring (§4.2), runtime display resize, fullscreen/multi-display |

---

## 11. Top open questions feeding back into design

These gate or reshape decisions above; track in issues:
1. EFI firmware for libkrun on macOS arm64 — Homebrew-provided or must we build EDK2? (gates M1) [01][03]
2. Fedora `.raw` boots end-to-end via EFI/disk; rootfs layout (btrfs/LVM) for the remount fallback. [01][03]
3. Does the input worker (epoll/eventfd shim) build/run on macOS arm64 with `--features input`; what fd does `get_ready_efd` return (pipe read-end?). [04]
4. Display model A vs B long-term: IOSurface ⇄ virglrenderer/MoltenVK interop feasibility before committing to zero-copy export. [03][09]
5. **RESOLVED** (`spikes/balloon-madvise`): `MADV_FREE_REUSABLE` drops `phys_footprint` on the live `hv_vm_map`'d region with no unmap; `MADV_DONTNEED` returns nothing. Dynamic memory is feasible on macOS/HVF. [08]
6. 4 KiB-guest vs 16 KiB-host (both boot paths are 4 KiB, verified): how much does stock Fedora reporting actually return under host-side coalescing (menu option a), and is a 16 KiB guest kernel (option b) / a host-page-aware reporting patch (option c) worth it? [08] §1.2
7. gvproxy speaks the `VFKT` dgram handshake `unixgram.rs` sends; outbound DHCP+curl works. [07]
8. GNOME/Mutter Fedora 43 implements `wlr/ext-data-control` so an unfocused agent can read/set the clipboard. (clipboard #1 blocker) [05]
9. Native virtio-snd spike: card enumerates in guest and plays a tone via a CoreAudio AudioUnit; latency vs Parallels. [11]
10. libkrunfw kernel rebuild with USB still boots within memory/boot constraints; vhci_hcd accepts an AF_VSOCK fd or needs a userspace bridge. [06]

---

### Appendix A — verified libkrun facts this design relies on

- The C `krun_start_enter` is just `build_microvm(...)` + `loop { event_manager.run() }`
  (lib.rs:3004-3040) — we replicate it in Rust (D2.1). Guest power-off → `VcpuExit::Shutdown`
  → `exit_evt` → `Vmm::process` → `Vmm::stop` → `libc::exit` **inside `krun-vmm`**
  (src/vmm/src/lib.rs:361), so the child-process model holds for the internal-API path too.
  (D3) [01], `spikes/m1-boot-internal`
- HVF dlopen'd at runtime; vCPU loop handles CANCELED/EXCEPTION/VTIMER; WFx parks (no busy
  spin); in-kernel `hv_gic` GICv3 is the **default** (not a patch we need). [02]
- virtio-gpu is real; display vtable `configure/disable/alloc/present` confirmed in
  `virtio_gpu.rs:518-536`; CPU pull model, full-frame, width*4 stride; no cursor queue / no
  zero-copy yet. [03][09]
- virtio-input: the C ABI is `krun_add_input_device(config_backend, size, event_provider,
  size)` with `#[repr(C)]` backend structs — but on the internal-API path (D2.1, D7) we push
  the native Rust backends onto `VmResources.input_backends` and skip the `#[repr(C)]` layer.
  Worker copies events verbatim (emit `SYN_REPORT`); `_fd` path Linux-only. [04]
- vsock IPC ports (`unix_ipc_port_map`) coexist with TSI; guest connects out to host CID 2.
  [05][10]
- No USB anywhere; libkrunfw kernel has USB fully disabled in every profile. [06]
- Two net architectures (TSI default; virtio-net via unixstream/unixgram/tap); `unixgram`+
  `NET_FLAG_VFKIT` for gvproxy; `set_port_map` is TSI-only. [07]
- Balloon attached unconditionally; only free-page-reporting wired; `num_pages`/`actual`
  never set; reclaim is Linux `MADV_DONTNEED`. No public balloon API. [08][10]
- vhost-user path is `cfg(linux)` + memfd-dependent → audio must be a native virtio-snd
  device; Rosetta is Vz-only → x86 via guest FEX/qemu. [11]
