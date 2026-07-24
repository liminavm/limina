# limina Roadmap

A milestone-based, bisectable plan for **limina** — a Rust macOS app on top of libkrun
(Hypervisor.framework) to replace Parallels for running Linux guests on Apple Silicon.

Each milestone has: **Goal**, **Key tasks**, **libkrun patches**, **Done test**, **Risks /
spike first**. Milestones are ordered by dependency; ship and tag each one before starting the
next so regressions bisect cleanly.

Grounded in the research under `docs/research/01..11`. API names are verified against the
vendored header `third_party/libkrun/include/libkrun.h` unless noted.

---

## Cross-cutting architecture decisions (apply to all milestones)

These are settled and constrain every milestone:

- **Backend: raw HVF via libkrun, NOT Apple Virtualization.framework.** Vz is closed and forbids
  the custom virtio devices, host-USB passthrough, fine-grained ballooning, and patchable guest
  agents that are limina's differentiators (research 02). Keep libkrun's HVF backend, patch
  selectively.
- **Build our own libkrun** from `third_party/libkrun` with `make GPU=1 INPUT=1 NET=1 BLK=1`
  (`VHOST_USER=1` only where it compiles — it is `cfg(target_os="linux")`). The product builds
  from source so we can carry patches and 1.18+ APIs the brew bottle lacks.
- **Dedicated child-process VMM, krunkit-style.** `krun_start_enter` loops forever and the whole
  process is torn down on guest PSCI SYSTEM_OFF/RESET (chain: hvf/lib.rs:536-540 →
  VcpuExit::Shutdown → vstate.rs:407-409 → run-loop exit → exit_evt eventfd). The limina GUI runs
  the VMM in a child process and drives it via vsock + the shutdown eventfd.
  - **Reboot = relaunch the child.** libkrun patch 0023 makes `SYSTEM_RESET` exit with a distinct
    `FC_EXIT_CODE_REBOOT` (125); `supervisor::run` relaunches the worker on it (recycling gvproxy,
    whose vfkit socket is single-connection) with a boot-loop cap, so the supervisor + host-side
    resources survive a guest reboot. Guard: `reboot::guest_reboot_relaunches_the_worker`.
    (Headless path only; the windowed worker↔window socketpair re-wiring is a follow-up.)
- **Codesigning is a hard gate from M1 on.** The limina *worker* (not libkrun.dylib) must be signed
  with `com.apple.security.hypervisor` (`hvf-entitlements.plist`; ad-hoc OK). Without it
  `hv_vm_create` returns `Error::VmCreate`. `com.apple.vm.networking` (vmnet/bridged) is Apple-gated
  and deferred — default to user-mode NAT.
- **Page-size reality:** host is 16 KiB pages (Apple Silicon), guest default 4 KiB. This bites the
  balloon (M6) and virtiofs DAX (M5), and was the gating constraint for venus host-visible blob maps
  (M4) — the enhanced tier runs a 16 KiB-page guest kernel for that reason.
- **Two-tier guarantee (see CLAUDE.md):** an unmodified stock Fedora guest on upstream-shaped
  libkrun must always keep booting and stay usable, degraded where our enhancements aren't installed.
  Our custom kernel/drivers/agent are the *enhanced* tier layered on top, never a precondition for
  the VM to run. Every milestone must preserve this floor; detect enhanced capabilities granularly
  and additively, not as one tier switch.
- **Repo hygiene:** vendor libkrun / libkrunfw / virglrenderer as patched forks under
  `third_party/` with patches as tracked series so rebases onto upstream stay reviewable.

---

## Testing infrastructure (cross-cutting)

Tests drive the **shipped binaries** the way a user does — `limina` (supervisor) →
`limina-vmm` (worker) → libkrun/HVF — with no shortcuts to libkrun's internal API. The
harness lives in `crates/limina-test` (the `Guest` type: boot, await a console marker,
clean teardown that can never leak a live VM). Layers:

- **L0 — unit.** Pure Rust per crate (facade `VmSpec → VmResources`, supervisor state machine).
  No HVF; runs anywhere under plain `cargo test`.
- **L1 — fast boot (primary).** Our own tiny direct-boot guest: a static Rust `init` (cross-built
  to `aarch64-unknown-linux-musl`) served as the guest root over **virtio-fs** (rootfs is a host
  dir; no mkfs/image). Boots to userspace and powers off cleanly in **~0.3s**
  (`tests/l1_boot.rs`, `guest/limina-init`), via libkrun's `ExternalKernel` (`KernelFormat::Raw`).
  **Kernel:** our custom 6.12 Image built by `scripts/build-test-kernel.sh` (virtio-fs root, vsock,
  PL011 console; `PAGESIZE=4k|16k`); falls back to libkrunfw's bundled Image when ours isn't built.
  The init also runs a tiny vsock agent (gated on `limina.agent_port=`) — the seed of `limina-agent`.
  - **Linux build environment.** Kernel built with **Apple `container`** (`fedora:43`) as a build
    tool only (the host is case-insensitive and has no kernel toolchain), bind-mounting out the
    `Image`. `docker/kernel-builder.Containerfile` documents deps. (Apple `container` uses Vz —
    fine for a build tool; unrelated to limina's runtime.)
- **L2 — stock baseline.** The unmodified Fedora `.raw`, opened read-only, asserts the user-facing
  chain boots through firmware + bootloader — the permanent two-tier compatibility floor.
  (Pristine Fedora has no `console=`, so userspace/feature assertions are L1's job.)
  `tests/boot.rs`.

HVF boot tests need codesigning + the entitlement, so they're **gated behind `LIMINA_HVF_TESTS=1`**
(plain `cargo test` skips them) and run via `scripts/test-boot.sh` (builds → signs the worker →
runs the gate). CI needs a **self-hosted Apple-Silicon runner**; the Fedora image is hosted
out-of-repo for L2.

**Trace-replay rendering tests (tier-2 pixels) — all four phases SHIPPED.** Rendering correctness
can't be asserted from exit codes or FPS, so replay tests mechanize the pixel-verify discipline:
capture a workload once, replay it **twice in the same boot** (zink→venus and the software
rasterizer = the reference) and tolerance-compare snapshot frames — no stored golden images, so
intentional mesa/KK changes never invalidate fixtures.
- `venus_replay` — GL via apitrace/eglretrace (Xwayland), guards the backend via GL_RENDERER (also
  the X11-EGL-crash regression test for mesa 0006), compares 47 frame pairs host-side.
- `venus_vk_replay` — native Vulkan via gfxreconstruct (vkcube on venus vs lavapipe; reference leg
  proven via gfxrecon's device-info mismatch warning).
- perf trend ledger (`scripts/perf-ledger.sh` → `perf/ledger.csv`; explicitly NOT a pass/fail gate
  — VM-on-dev-machine variance).
- `venus_shell_replay` — the real seated gnome-shell (mutter), captured via `LD_PRELOAD` egltrace;
  reproduces pixel-exact on venus vs llvmpipe.

NOT covered by replay: the present/scanout path (fence-present, zero-copy) — that stays with the
seated-desktop + iosdump oracles. Capture protocols + divergence notes in
`spikes/trace-replay/RESULTS.md`.

**Rule: fix bugs RED-first.** Every bug fix starts with a failing test that reproduces it (see
CLAUDE.md). L1 is what makes this cheap enough to always do.

**Backlog — programmatic key/input injection into the guest (test + agent tooling).** Driving a
windowed guest today relies on host-side osascript `System Events` keystrokes, which is unreliable:
macOS intercepts function keys (F11 = *Show Desktop* on the host, never reaching the guest), lone
modifiers may not route to the window, and the mapping is opaque (F12 vs F11 keycode confusion cost
a detour during the 2026-07-22 wakeup re-baseline). We already own the guest keyboard/pointer
(`--input-kbd-fd`/`--input-ptr-fd`, evdev). Add a **host→guest synthetic-input path** that writes
evdev events directly into those devices, exposed as (a) a control-plane message and (b) a CLI/test
helper (`limina sendkey <combo>` / a harness API on the `Guest` type), so automation and agent
driving of a windowed VM is deterministic and independent of the macOS event system. Pairs with the
GNOME state problem hit above: kiosk-mode app launch is the workaround; robust key injection is the
fix. Small; unblocks scripted UI tests and reliable agent control of the desktop.

---

## Milestone 1 — Boot Fedora-Workstation-43.raw to a serial console

**Status: ✅ done.** `limina --firmware <KRUN_EFI.fd> --disk Fedora-Workstation-43.raw --console
<file>` boots the stock distro via EFI to userspace with full kernel dmesg on the serial console;
child-process supervision + codesign shipped (`crates/limina`, `crates/limina-vmm`).

**What was built / load-bearing facts:**
- **Firmware is a flat EDK2 `.fd` blob**, NOT a linked dylib — `krun_set_firmware` just `fs::read`s
  it into guest RAM (`builder.rs:1568`, `Payload::Firmware`). Use krunkit's `KRUN_EFI.silent.fd`
  (or build EDK2 ourselves, see M2.5 Track B).
- **Boot path verified** (`spikes/m1-boot`): EDK2 → shim/GRUB → Fedora kernel → systemd → mounted
  btrfs root + ext4 `/boot` → `getty.target`. Disk presents as MBR `vda` (FAT ESP / ext4 `/boot` /
  btrfs root `subvol=root`); the distro's own initramfs mounts root, **no `root_disk_remount`**.
- **`limina-vmm` uses the internal Rust API** (no C ABI): assemble `VmResources` in a `krun/`
  facade — `set_vm_config` (static RAM; dynamic is M6), `set_firmware_config`, `add_block_device`,
  then `build_microvm` + `loop { event_manager.run() }` in the child.
- **Console:** set `disable_implicit_console = true` + push a `SerialConsoleConfig` (input_fd must
  be a kqueue-pollable pipe/FIFO); the implicit firmware serial is output-dropped (`builder.rs:731`).
  To see *kernel* dmesg the guest cmdline needs explicit `console=`; libkrun's PL011 is at
  `0x0a001000` → `earlycon=pl011,mmio32,0x0a001000 console=ttyAMA0,115200`.
- **Networking is TSI default** at this stage (no virtio-net device — that's M3).
- **Supervision:** `crates/limina` spawns the signed worker in its own process group, forwards
  graceful shutdown on SIGINT/SIGTERM, escalates to SIGKILL after a grace period.

**Known caveat:** a *stock* EFI Fedora guest does not honor the GPIO power button (KRUN_EFI's ACPI
doesn't advertise it), so graceful power-off is an **enhanced-tier** feature (our DT / `limina-agent`,
shipped in M5); the baseline relies on the SIGKILL fallback.

**libkrun patches:** none for the happy path. Stretch: convert the `panic!` exit paths a real boot
may hit (`hvf/lib.rs:549` unknown PSCI, `:595-602` unknown exit reason, `:728` unknown ESR_EL2 EC)
to logged graceful stops once we see which fire.

---

## Milestone 2 — Display + input (native Metal window)

**Status: ✅ done.** Real Fedora boots into a native limina window to the GNOME desktop, the host
keyboard + pointer drive it, the present path is flicker-free, and the guest pointer renders as a
crisp hardware cursor.

**What was built / load-bearing decisions:**
- **Display is supervisor-hosted via a shared IOSurface, not an in-process CAMetalLayer.** The
  worker (`limina-vmm --display-window`) runs the native-Rust `krun_display` backend and publishes
  each scanout into a cross-process `IOSurface`; the supervisor (`limina --window`) owns the NSWindow
  and presents it via `CALayer.contents`. Keeps the AppKit UI in the surviving supervisor, off the
  VMM child.
- **Tier-1 is software-2D — M2 needed *no* M4 GPU machinery.** libkrun patch 0001 serves the 2D
  scanout straight from host CPU memory and skips virglrenderer/rutabaga init entirely, so it works
  on a GL-less host. A headless PNG-capture sink is the test oracle.
- **Input worker builds and runs on macOS arm64** (the top M2 risk, resolved). NSEvent capture →
  virtio-keyboard + absolute pointer (+ relative mouse for capture), with a kVK→KEY table. The input
  provider MUST emit explicit `EV_SYN`/`SYN_REPORT` — the worker copies events verbatim with no
  auto-SYN (`worker.rs:175-184`). ABI footgun: `krun_add_input_device`'s `void*` args are the Rust
  `#[repr(C)]` `InputConfigBackend`/`InputEventProviderBackend` (use `krun_input`'s
  `into_input_config`/`into_input_events`), NOT the header structs.
- **Present path hardened + flicker eliminated.** Rect-limited swizzle into a CPU-side BGRA canvas →
  memcpy into a 3-deep off-screen IOSurface ring (no write-while-composite tear); implicit Core
  Animation actions disabled (`CATransaction`) to kill the per-frame fade. `alloc_frame` requires
  exactly `width*4` bytes-per-row (libkrun's `read_2d_resource` uses a tightly-packed stride).
- **Hardware cursor** via a now-serviced virtio-gpu cursor queue (libkrun patch 0008 + additive
  `set_cursor`/`move_cursor` ABI; upstream never implemented it). **Pointer redesign:** the
  worker-published cursor IOSurface becomes the macOS `NSCursor` worn inside the content view
  (one pointer, zero lag, guest-correct shape); pointer events gated to the view. **Known gap:**
  guest-initiated pointer warps (guest `cursormove` positions) are ignored host-side — reconcile
  with pointer capture in M8.
- **Window-close → orderly guest shutdown** is closed via the M5 control plane (window-close sends
  the agent SHUTDOWN, falls back to SIGKILL for stock guests; `l1_shutdown` asserts the ladder).

Pre-boot display config (retained API reference): `krun_add_display(ctx, w, h)` (0..15, max 16),
`krun_display_set_dpi/_physical_size/_refresh_rate` for the generated EDID; HiDPI via
`contentsScale = backingScaleFactor`. Multi-display multiplexes by `scanout_id` (M8).

---

## Milestone 2.5 — Console & serial (full-boot visibility + debug shell)

**Status: ✅ DONE.** Track A (PL011 tty + an interactive command/shell that types-and-asserts on
both the L1 guest and stock Fedora) and Track B (GOP firmware → GRUB → kernel all render in the
window) are both complete. One low-confidence bug — a once-seen windowed-splash hang — remains as a
tracked open lead (deferred, not a blocker: the boot console is verified working on both the capture
and live paths).

A debuggability *enabler* milestone: make the boot chain observable end to end (every later
milestone is easier to diagnose with a real boot console + interactive serial shell).

**Goal:** (A) an interactive **serial** shell on `/dev/ttyAMA0` (PL011) for stock and custom guests;
(B) the **full boot (EFI → GRUB → early kernel) visible in the limina window**, not just post-DRM
frames.

### Track A — PL011 serial *tty* + interactive shell ✅

`/dev/ttyAMA0` is a real bidirectional console, proven by `l1_serial.rs`. Required `arm,primecell`
on the FDT serial node (`devices/src/fdt/aarch64.rs::create_serial_node`) so the guest binds
`amba-pl011` — patch **0005**. The probe blocker was an **HVF MMIO handler panic on `len=2`** (the
driver's first 16-bit `writew` to REG_IMSC), fixed by patch **0004** (handle halfword writes; the
read side already did). Also shipped earlier: hvc0 bidirectional console + L1 round-trip
(`l1_console.rs`, patch 0003 `PortConfig::ConsoleInOut`); PL011 drop-on-`WouldBlock` (patch 0002);
the interactive `--console-pty`.

**Interactive shell ✅ — the harness can now type a command and assert its output over serial.**
On stock Fedora that's a real `serial-getty@ttyAMA0`: it comes up on the EFI path because
`prepare-efi-image.sh` puts `console=ttyAMA0` on the GRUB args, and
`fedora_stock_image_efi_boots_to_userspace` asserts the `login:` prompt on serial; a human attaches
an interactive shell via `--console-pty`/`screen`. The tiny L1 guest has no shell binary (only the
pure-Rust `/init`), so `limina.console_shell` makes the init *be* the shell — a few in-process
built-ins (`echo`, `cat <path>`, `uname`) each framed by a `LIMINA_SHELL_DONE rc=` terminator that
the harness slices on (`Guest::console_command`, robust against interleaved kernel log);
`l1_command.rs` types commands and asserts their output + error recovery. Two-tier: the len=2 fix is
global+additive; the `arm,primecell` node only affects the direct-kernel path (EFI uses EDK2's own
DT). L2 stays green. (Automated *password* login over serial is deliberately not added — SSH is the
automated guest-access path; the serial getty is the human debug shell.)

### Track B — graphical boot console (EDK2 GOP) ✅ end-to-end

We build `KRUN_EFI` ourselves (`scripts/build-krun-efi.sh`, Apple `container`,
slp/edk2@krun-support `ArmVirtKrun.dsc`) and add `OvmfPkg/VirtioGpuDxe` so the firmware's graphics
console paints to our virtio-gpu scanout (which software-2D patch 0001 presents). The EFI-path
cmdline is GRUB-owned and FDT bootargs are ignored, so a *firmware* graphics console — not a kernel
`console=` — is the lever for pre-kernel stages (see `limina-efi-console`).

Three fixes made it work end-to-end:
- **libkrun 0006** — VirtioGpuDxe over virtio-mmio marks its control queue ready *without*
  programming QueueNum, so libkrun's size-0 init ignored the avail ring; snap a ready-but-unsized
  queue to `max_size` (QEMU-compatible). Produces a 1280×800 GOP; GRUB + kernel boot.
- **libkrun 0007 (reworked into 0022)** — the kernel's EFI→kernel re-init of virtio-gpu left the
  firmware-era worker thread busy-looping on the freed ring; the worker now epolls a stop eventfd
  and `reset()` signals+joins it before re-activation.
- **Firmware PlatformBm.c patch** — ArmVirt's `PlatformBootManagerBeforeConsole` connects only
  **PCI** displays before populating `ConOut`, so our virtio-**mmio** GOP never entered ConOut.
  Patch `FilterAndProcess(&gVirtioDeviceProtocolGuid, IsVirtioGpu, Connect)` before `AddOutput`.
  Host-side companion: the capture backend (`limina-display`) encodes PNGs on a background
  coalescing thread — the firmware's synchronous per-Blt `RESOURCE_FLUSH` against a per-frame PNG
  encoder otherwise stalled the GRUB menu (the real IOSurface window present is cheap).

**Phase 3 status (2026-06-14):**
- **✅ Default firmware.** `limina` defaults windowed boots to the GOP firmware when no `--firmware`
  is given — `resolve_windowed_firmware` (`crates/limina/src/main.rs`) tries `$LIMINA_GOP_FIRMWARE`
  → the bundle's `Contents/Resources/KRUN_EFI.gop.fd` → the dev artifact
  `target/krun-efi/KRUN_EFI.gop.fd`, then degrades to krunkit's silent `.fd` with a warning.
  `--firmware` optional under `--window`, still required headless. L0-tested.
- **✅ Present-path reconcile (no code change).** A firmware GOP scanout has no `iosurface_id`, so
  `flush_resource` rides the plain tier-1 software-2D `present_frame`. Verified live: auto-GOP boot
  renders the TianoCore splash → GDM → full GNOME desktop, pixel-confirmed via iosdump.
- **✅ Boot console IS visible.** A dedup frame-timeline proves the window renders the full boot on a
  bare image: firmware → GRUB 2.12 menu → kernel fbcon → GDM → desktop, byte-identical across the
  capture and live paths. The present path is not frozen and needs no fix.
- **❌ OPEN, UNEXPLAINED (DEFERRED) — a windowed boot once sat on the firmware splash and looked
  hung.** Seen once, seemingly on first boot. Cause UNKNOWN; intermittent, not reproduced. Does not
  block M2.5 (the boot console is verified working on both the capture and live paths). Do NOT
  conflate with the SELinux relabel below (that runs after the kernel boots). Tracked as a
  low-confidence race in memory `limina-windowed-reboot-present-race` (candidate causes: first-boot
  firmware/GOP init race, AppKit/control-socket first-frame timing, stale surface cache,
  reboot-relaunch re-wiring). Next step when revisited: a deliberate repro (loop many windowed boots
  with the frame-timeline + serial) before chasing any one theory.
- **SELinux relabel + custom-kernel SELinux (resolved, distinct from the hang).** Dev images
  built under `selinux=0` carried an unlabeled tree + stale `/.autorelabel`, so a stock enforcing
  EFI boot relabeled + rebooted once. Fix: `scripts/build-test-kernel.sh` now compiles SELinux in
  (so the enhanced kernel doesn't diverge from the distro; `selinux=0` still works as the kill
  switch), and a one-time `scripts/prepare-efi-image.sh` per image does a permissive relabel +
  adds `console=ttyAMA0` to GRUB args. Prepared images EFI-boot clean in one boot.
- **Follow-ups ✅ (all three done):** (a) `fedora_stock_image_efi_renders_to_gop` now keeps the
  richest scanout frame seen **before** the serial getty `login:` and asserts it's rich — so it
  asserts the *boot console* (firmware/GRUB/kernel-fbcon) rendered, not just "rich content
  eventually" (which GDM alone satisfied); measured ~70k distinct colors pre-login. (b)
  `run-fedora-window.sh` defaults to the labeled `dev-enh.raw` when present (clean one-boot, no
  relabel+reboot). (c) `prepare-efi-image.sh`'s relabel self-check is now non-fatal (the
  `/.autorelabel`-gone check is authoritative; an already-labeled re-run prints no `Relabeling /`
  line), and a backgrounded `limina` that exits early surfaces its worker-log tail instead of
  reading as a 300s timeout.
- The **GOP+venus singleton blocker is FIXED** (M4 open item; libkrun 0022). Remaining
  productization: ship the GOP firmware inside `limina.app/Contents/Resources/`.

**Compatibility floor (checked on both channels):** `boot::fedora_stock_image_efi_boots_to_userspace`
(silent firmware — asserts firmware → GRUB → kernel → getty `login:` + sshd on serial) and
`boot::fedora_stock_image_efi_renders_to_gop` (GOP firmware — asserts the boot is visually present
in the captured window).

**libkrun / firmware patches:** Track A shipped (0004 + 0005). Track B = KRUN_EFI EDK2 rebuild with
VirtioGpuDxe + the PlatformBm.c patch, plus libkrun 0006/0022; all carried as tracked series.

**Done test:** (A) ✅ the PL011 tty round-trips (`l1_serial`) and the harness types commands +
asserts their output — `l1_command` (L1 in-process shell) and the stock-Fedora serial getty
`login:` (`fedora_stock_image_efi_boots_to_userspace`), with an interactive shell available over
`--console-pty`/serial pane. (B) ✅ the window shows EFI + GRUB + early-kernel output during boot
(`fedora_stock_image_efi_renders_to_gop` now asserts a pre-userspace boot-console frame).

---

## Milestone 3 — Networking (NAT, then bridged)

**Status: ✅ NAT done — outbound + inbound SSH. Bridged remains the opt-in later sub-step.**

**Goal:** real virtio-net NIC with outbound internet + DNS via user-mode NAT; bridged opt-in later.

**What was built / load-bearing facts:**
- **Outbound:** `limina --net` spawns + supervises a gvproxy gateway
  (`-listen-vfkit unixgram:///abs/socket`) and connects the guest's virtio-net via
  `krun_add_net_unixgram` (`UnixgramPath(_, vfkit=true)`, `NET_COMPAT_FEATURES`, fixed MAC). **No
  libkrun patch.** Verified end-to-end (spike `spikes/m3-gvproxy` + `tests/net.rs`): DHCP, DNS,
  outbound TCP. Gotchas: (a) the `-listen-vfkit` URL must be **absolute**; (b) the guest must reach
  **userspace** for NetworkManager to DHCP, so net tests boot a **writable** APFS COW clone (a
  read-only root never reaches NM). Oracle = host-side gvproxy `-debug`. Supervisor tears the
  gateway down on both exit paths (headless Drop; windowed `gateway::cleanup()` before
  `process::exit`).
- **Inbound SSH (no REST forwarding needed):** gvproxy ships a built-in default forward
  `127.0.0.1:<port> → 192.168.127.2:22`, and the guest gets static `.2` when the NIC uses the
  **well-known vfkit MAC `5a:94:ef:e4:0c:ee`** (`crates/limina-vmm/src/krun/mod.rs`). So
  `ssh -p <port> user@127.0.0.1` works with zero forwarding config. The host port **auto-allocates
  from 2222 up** when `--ssh-port` is omitted (so 2+ VMs run concurrently without colliding) or is
  pinned with `--ssh-port <1024-65535>`; either way the supervisor logs the resolved command,
  `guest SSH forward ready: ssh -p N <user>@127.0.0.1`. This is the daily-driver guest-access path —
  runbook in `docs/images.md` (§SSH access) and the `limina-fedora-access` note. (`krun_set_port_map`
  stays TSI-only — EINVALs once a net device exists.)

**Remaining:**
- **Bridged (opt-in):** virtio-net + Apple `vmnet.framework` BRIDGED via a privileged helper
  (`vmnet-helper`+unixgram/vfkit OR `socket_vmnet`+unixstream) + the Apple-gated
  `com.apple.vm.networking` entitlement (or a separately-signed root helper). Determine which vmnet
  helper is installed.
- **Optional libkrun patch:** `worker.rs` reconnect-on-HANG_UP so a gvproxy restart doesn't require
  a full VM restart (today the net worker logs FATAL and permanently disables the NIC,
  `worker.rs:146`; the supervisor recreates the path instead).
- **Offload tuning:** evaluate `GUEST_TSO6|HOST_TSO6` (reachable via the new API) for iperf3 once
  verified non-corrupting; start at `NET_COMPAT_FEATURES` (IPv4 CSUM + TSO4 + UFO).
- **Ergonomic SSH (zero-port, zero-touch keys) — GNOME Boxes prior art.** Replace "read the
  auto-allocated port N from the log" with a stable `ssh limina-<vm>` that needs no forwarding and no
  guest agent. Two independent halves, copied from Boxes (see `docs/research/prior-art-gnome-boxes.md`):
  (A) **inject the host SSH _public_ key as an SMBIOS type-11 OEM string**
  (`io.systemd.credential.binary:ssh.ephemeral-authorized_keys-all=<base64>`) — stock guest **systemd**
  consumes it into sshd with **no cloud-init / no `limina-agent`**, so it's a baseline/bootstrap-tier
  win; needs libkrun to expose SMBIOS OEM strings (small upstreamable patch if it doesn't already).
  (B) a tiny **host-side `ProxyCommand` helper that dials the guest's vsock ssh port** (we own the
  vsock plane already; `systemd-ssh-proxy` is Linux-host-only so we reimplement the dial). Half A is
  independently useful even over the existing NAT forward. **Gating spike:** does libkrun set any
  SMBIOS on aarch64 today, and does stock Fedora systemd light up `ssh.ephemeral-authorized_keys-all`
  from an OEM string under libkrun? (Ties to [[limina-fedora-access]].)

**Risks / spike first:**
- Does vmnet BRIDGED work over en0 Wi-Fi on M1 Max or only wired/USB Ethernet?
- macOS datagram size limit (`SndBuf = MAX_BUFFER_SIZE - VNET_HDR_LEN`) vs large GSO frames.
- Enumerate TSI gaps (ICMP, mDNS/Bonjour, VPNs, multicast) for product expectations.

---

## Milestone 4 — 3D acceleration (Venus) — a.k.a. "tier-2"

**Status: 🟢 GREEN (substantially done).** The seated GNOME desktop runs on venus with
**fence-accurate zero-copy presents** and cross-context buffer sharing; WebGL2 works (we implemented
`VK_EXT_transform_feedback` in KosmicKrisp); the 5000-fish WebGL aquarium runs 60fps vsync-capped
@46% GPU — matching host-native Firefox. **Mutter direct scanout works** (fullscreen client buffers
flip straight to the primary plane). Converged truth + open-threads ledger live in memory
`limina-tier2-venus`; root-cause history in `spikes/venus-draw-probe/RESULTS.md`.

**Goal:** hardware-accelerated 3D in the guest — GNOME on real GPU, GL apps via Mesa zink.

### Load-bearing architecture decisions

- **The GPU flag set is the `GPU_COEXIST_FLAGS` in `crates/limina-vmm/src/krun/mod.rs:102-108`.**
  **⚠️ SUPERSEDED HISTORY:** an earlier note here said `VENUS | NO_VIRGL` (0xC0) with `USE_EGL` off
  and `NO_VIRGL` **on** ("no GL context on Apple Silicon"). That was true *before* zink-on-KK. It is
  now reversed: `NO_VIRGL` is **OFF** and `USE_EGL | USE_GLES | USE_SURFACELESS` are **ON**, because
  zink-on-KK gives virglrenderer a host GL context — so **vrend GL (the virgl tier) is enabled
  alongside venus**. **See `docs/tiers.md` for the authoritative, current tier model** (this bullet
  is kept only to flag the reversal that confused past readers).
- **Tier-2/3 is a "coexist" device, not a flag flip.** The rutabaga serves Vulkan (venus) **and** GL
  (vrend); it does NOT implement the 2D commands the firmware GOP / efifb / fbcon / scanout-present use.
  Our software-2D patch (0001) serves exactly those. **libkrun patch 0010** makes both live in one
  virtio-gpu, routed by ring/command type (2D → software-2D CPU path; 3D ctx/submit/capset →
  rutabaga/venus), and **degrades gracefully to software-2D on renderer-init failure** (no panic).
  **Coexist is the DEFAULT** (`--gpu-software-2d` overrides for the capture oracle). Design:
  `docs/design/tier2-coexist-gpu.md`.
- **The 16 KiB-host / 4 KiB-guest blob map is THE constraint, and the fix is guest-side (enhanced
  tier).** venus RESOURCE_MAP_BLOB → `hv_vm_map` requires host addr, guest addr, AND size to be
  16 KiB-multiples; a stock 4 KiB guest packs host-visible blobs at 4 KiB granularity so two blobs
  share one host page and can't be mapped independently. No host-only fix exists. A **16 KiB-page
  guest kernel** (`build-test-kernel.sh PAGESIZE=16k`) makes venus blobs 16 KiB-sized AND -spaced →
  `hv_vm_map` works with zero host changes; `vulkaninfo` enumerates `Virtio-GPU Venus (Apple M1
  Max)`. On stock 4 KiB Fedora, venus stays unachievable → llvmpipe degraded baseline (floor holds).
  libkrun **0011** logs `hv_vm_map` failures with the alignment breakdown (diagnostic only).
- **Zero-copy present + the B/D insight: IOSurface is the macOS dmabuf.** "Zero-copy end-to-end" has
  four crossings; the desktop needs **B** (rendered image → display) and **D** (mutter can create a
  venus image at all), which are the same missing capability — a *dmabuf-exportable* image. On macOS
  the export currency is **IOSurface**. Making venus "exportable images" resolve host-side to an
  IOSurface-backed `MTLTexture` closes both: mutter's `vkCreateImage(extHandleTypes=…)` succeeds (D)
  and `SET_SCANOUT_BLOB` references that IOSurface for libkrun to hand straight to `CALayer.contents`
  with no readback (B). **Shipped:** `#8` complete — guest kernel fences blob-scanout flushes
  (`patches/linux/0001`) and the host holds them to the true CA latch; enhanced tier =
  `FENCE_PRESENT=1 COPY=0` (0 anomalies), stock-kernel tier keeps `PRESENT_COPY`.
- **Crossing C (`glReadPixels`/screenshots/venus feedback, #28) is separate — coherency, not copy —
  and below B/D.** Host CPU IS coherent with GPU writes; the guest `hv_vm_map` view reads stale
  (GPU write lives in the Apple SLC, beyond the guest mapping's PoC), so guest-invalidate-alone is
  dead. **Zero-copy only — the memcpy/transfer model is rejected.** Remaining candidates: host
  clean-to-PoC + guest invalidate (cheapest, untested); blobs backed from the guest-RAM-coherent
  region; HVF stage-2 cache attrs on `hv_vm_map`.
- **Host Vulkan driver: KosmicKrisp (KK) is the ONE supported venus backend.** Daily driver
  (`boot-seated-kk.sh`); perf knobs default-ON in KK itself, CTS-validated. **MoltenVK retired as a
  venus backend** — it SIGSEGV-loops the gnome-shell compositor (the #28/#32 corruption crashes
  instead of degrading; verified A/B on the same image). Every venus boot path forces the KK ICD and
  **degrades to software-2D (llvmpipe) when KK is absent — never the loader's MoltenVK default**.
  MoltenVK builds/patches archived under `spikes/archive/moltenvk/`.
- **virglrenderer:** our fork carries the Apple blob support (`RUTABAGA_MEM_HANDLE_TYPE_APPLE`,
  `VIRGL_RENDERER_BLOB_FD_TYPE_APPLE`, `virgl_renderer_resource_get_map_ptr`) + IOSurface /
  cross-context patches. Build the libkrun-flavored fork under `third_party/virglrenderer` — do NOT
  link Homebrew's (it silently degrades venus to software-2D; see the `limina-virgl-link-trap` note).

### Open M4 items (the remaining ledger)

1. **Productize the enhanced tier.** The **guest side is ✅ DONE** (2026-06-25 F43, 2026-06-29
   F44, [[limina-enh-delivery]]): the 16 KiB kernel + venus mesa + patched mutter ship as rebuilt
   Fedora RPMs installed by `install-enhanced.sh` from the guest-tools tarball (see item 3 and the
   M5 productization section). What remains is *distribution from the app* (`limina
   install-guest-tools` + version manifest — `docs/design/distribution.md`). The **host side is
   ✅ DONE (2026-06-23):** `scripts/build-app.sh`
   assembles a self-contained `limina.app` — the whole host venus/GL closure (virglrenderer fork,
   epoxy, Vulkan loader, `libvulkan_kosmickrisp.dylib`, the zink-on-KK Mesa stack + LLVM) is vendored
   into `Contents/Frameworks`, relocated to `@rpath`, with a relative-path KK ICD under
   `Contents/Resources/vulkan`; the supervisor sets the bundle-relative venus env (`VK_ICD_FILENAMES`
   + zink-on-KK selectors) when it detects the bundle (`crates/limina/src/venus_env.rs`), so the
   loader sees only our KK ICD (no MoltenVK on the path). Ad-hoc signed for local dev (Developer-ID +
   notarization is the later distribution step). Verified: boots the seated venus desktop from a
   clean environment — the worker loads 11 dylibs from `Contents/Frameworks`, **zero** from
   `/Volumes/mesa-cs` or `third_party/`. Note: bundling only removes *host-driver absence* as a
   fallback trigger — software-2D + in-guest llvmpipe stays a shipping path for guests that can't
   drive venus out of the box (stock 4 KiB kernel / no venus mesa = the floor); the fallback is
   guest-capability-driven, not host-driver-driven. (~266 MB, dominated by the zink→LLVM host-GL
   stack, which the worker genuinely loads — confirmed via `lsof`.)
2. **Upstream patch queue:** mesa zink+venus / KosmicKrisp / virglrenderer / SPIRV-Cross /
   mutter ×2 / kernel 0002+0003 / Fedora-zink backport ask.
3. ~~**Run the desktop through the GOP console end-to-end.**~~ **DONE.** The GOP+venus singleton is
   FIXED (libkrun 0022 — a persistent gpu worker owns the renderer across device reset; RED-verified
   by `venus_reset`); GOP firmware is the windowed-boot default (M2.5 Phase 3); and the **16 KiB kernel
   is EFI/BLS-bootable and GRUB-default** on `enhanced.raw` — the enhanced-delivery work (2026-06-27,
   [[limina-enh-delivery]]) ships it as the `limina-kernel-16k` RPM whose `%post` runs `kernel-install
   add` (dracut initramfs + a BLS entry with distro-standard `root=UUID=`), and `install-enhanced.sh`
   `grubby --set-default`s it. The venus desktop is pixel-verified booting that way. (The earlier "EFI
   boots the stock 4 KiB kernel, venus moot" note is obsolete — that was the pre-RPM L1-direct-boot
   era.)
4. **virtio-gpu flip-completion gap** — event-driven KMS clients hang; #8 gave mutter honest pacing
   but the generic gap remains.
5. **#28 residue policy** — `VN_PERF=no_*_feedback` via agent vs a real fix.
6. **Cosmetic / low-priority:** `num_capsets` hardcoded 5; the non-fatal `CTX_DETACH_RESOURCE`
   (0x203 → ERR_UNSPEC) dmesg line; KK GPU-side per-draw root re-fetch (only if GPU-bound workloads
   reappear); Firefox MSAA cosmetic thread.

**The virgl tier IS shipped (not parked).** GL via virgl → host **vrend** → zink-on-KK runs on a
stock 4k guest (its copy/transfer model is immune to the 16k/4k blob problem) and is **enabled by
default** in the coexist flags — see `docs/tiers.md` (tier 2). What *was* parked is the unrelated
**virgl-over-ANGLE** variant (a separate GL→VK→Metal translation stack); zink-on-KK replaced the need
for it. Native-context (vdrm) backed by Metal is recorded only as a curiosity — venus is the top tier.

**libkrun patches (shipped):** coexist (0010), fence-present series (0017–0022), virglrenderer fork,
KK perf/XFB patches, kernel `patches/linux/0001–0003`, mutter ×2. Remaining: the upstream queue.

**Done test:** in the guest, `vulkaninfo` reports the Venus renderer (not llvmpipe), `glmark2` runs
on the GPU, GNOME Shell animations are smooth; full-screen 3D/video shows no per-frame
`read_2d_resource` readback and present is driven from an IOSurface-backed texture at display refresh.

---

## Milestone 5 — Clipboard + virtiofs file sharing + guest agent

**Status: 🟢 core done.** Control plane, clipboard, virtiofs sharing, and liveness are all live;
the productization track SHIPPED as RPMs (see below); follow-ups remain.

**Goal:** bidirectional text clipboard, a host folder shared into the guest, and a versioned control
channel between limina and a guest agent.

**What was built / load-bearing decisions:**
- **Control plane (`crates/limina-proto`):** 16-byte `LIMINA` frame header + CBOR/minicbor payloads
  (HELLO/WELCOME/HEARTBEAT/SHUTDOWN/SHUTDOWN_ACK/ERROR); **unknown types → `ERR_UNSUPPORTED`, never
  fatal.** One multiplexed vsock control connection with the **guest connecting out** (no listen
  flag; mirrors the verified guest-connect flow), coexisting with TSI with no libkrun patch
  (`device.rs:38-47` keeps `unix_ipc_port_map` separate from `tsi_flags`). The **supervisor owns the
  host side by default** (binds a private socket at `CONTROL_PORT`, serves the handshake, turns
  window-close/SIGTERM into an orderly power-off: SHUTDOWN → 5s agent grace → power-button SIGTERM →
  SIGKILL). Explicit `--vsock-*` still passes raw plumbing for the harness.
- **Multi-peer registry:** the host `ControlPlane` keeps a peer registry (the clipboard needs a
  separate per-session user connection alongside the root agent); SHUTDOWN routes to every
  shutdown-capable peer. Heartbeat liveness: every inbound message is stamped per peer; a monitor
  reports agents silent past `LIMINA_AGENT_SILENT_SECS` (default 5s) and recovery.
- **Guest daemons (baked into the dev-enh golden):** `limina-agent` (musl static + systemd unit:
  reconnect loop, heartbeats, SHUTDOWN → `systemctl poweroff`, also the share auto-mounter) and
  `limina-agent-session` (per-session user helper for clipboard).
- **Text clipboard (live end-to-end):** `CHANNEL_CLIPBOARD` (OFFER/REQUEST/DATA, newest-serial wins
  both ways); the supervisor's NSPasteboard bridge (`crates/limina/src/clipboard.rs` — changeCount
  poller, self-change suppression, `LIMINA_PASTEBOARD` test override). **Two-tier guest mechanism:**
  the enhanced tier implements **ext-data-control-v1** in our carried mutter (`patches/mutter/0003`
  — GNOME refuses it upstream, mutter#524) so the helper is a focusless pure-Rust Wayland client with
  no "screen is being shared" indicator; **stock mutter falls back to the
  `org.gnome.Mutter.RemoteDesktop` D-Bus clipboard API** (no mutter patch, but the orange indicator
  is the documented cosmetic cost). Loop prevention = track our own live source / `is_owner` echo.
  The Wayland foundation is also the stepping stone for future drag-n-drop.
- **virtiofs sharing (shm-less):** `limina --share '[NAME=]PATH[:ro]'` (repeatable) attaches a host
  dir as virtiofs tag `limina-NAME`; the agent auto-mounts every `limina-`-tagged device at
  `/media/NAME`, discovering tags via sysfs (`/sys/fs/virtiofs/<id>/tag` — NOT the virtio-9p
  `mount_tag`, and NOT the cmdline, so it survives EFI boots where GRUB owns the cmdline). A guest
  without the agent degrades to `mount -t virtiofs` by hand. **No DAX/shm window yet** — same
  shm-less shape as the L1 rootfs; DAX is a tracked perf enhancement.

**Tests:** limina-proto L0; `l1_agent`, `l1_shutdown`, `l1_real_agent`, `l1_multi_agent`,
`l1_clipboard` (+ `_oversized_content`), `l1_session_helper` (real helper vs a scripted
`limina-mock-mutter` zbus stand-in on a real musl dbus-daemon — general L1 D-Bus infrastructure),
`l1_share` (read+write round-trip + `:ro` refusal, on libkrunfw's 6.12 kernel), `l1_liveness`.
Live-verified on the seated desktop both clipboard directions + shares + window-close → orderly
GNOME power-off (~1s, exit 0). `l2_share_71` extends the share coverage to a **≥7.1 guest kernel**
(the virtio-fs used-ring-length path, libkrun 0090): Linux ≥7.1's `virtio_fs_verify_response`
rejects a FUSE reply whose used length is 0, which bricked shares on the enhanced tier and escaped
review because no automated test ran a share on ≥7.1 (L1 uses 6.12; the injected L2 kernel is 6.12
too). It boots a distinct ≥7.1 16 KiB test kernel (`Image-16k-71`) and mounts rw + ro shares over
SSH — RED/GREEN-verified against the pre/post-0090 worker.

### Productization: ✅ SHIPPED as RPMs replacing stock at `/usr` (2026-06-25, re-validated on F44 2026-06-29)

The sysext design originally written here was **rejected during implementation** and the section
rewritten after the fact (2026-07-01) — the authoritative rationale lives in `docs/tiers.md`
(§"Why RPM-replace, not a sysext overlay") and `docs/images.md`. Short form:

- **Userspace = rebuilt Fedora SRPMs replacing stock at `/usr`** (mesa pinned to our version +
  dnf-versionlocked; mutter matched to the target distro's version with `patches/mutter` rebased
  per GNOME release). Why not sysext for mesa: our-mesa-vs-stock **libgallium SONAME mismatch**,
  and an overlayfs upper can't *remove* stock files — the resulting ABI blend broke mutter's KMS
  EGL. The retired sysext builders live in `scripts/archive/`.
- **The kernel goes through the distro's own EFI boot machinery** (this part of the original
  design survived): a stock-like kernel RPM whose `%post` runs `kernel-install add` (dracut
  initramfs + BLS entry); stock kernel = one GRUB choice away = the degradation path. venus
  surviving the EFI→kernel reset (libkrun 0022) and the 16 KiB kernel being BLS-bootable are both
  ✅ DONE ([[limina-enh-delivery]]).
- **Bootstrap is a one-time Parallels-style "install guest tools"**: `install-enhanced.sh` + the
  guest-tools tarball, run in the *stock* guest (which has a working software-GL desktop first —
  the two-tier bootstrap floor). Validated end-to-end on pristine F43 (2026-06-25) and F44
  (2026-06-29); component versions in `docs/images.md` §Component versions.
- **Still open in this track:** guest-tools distribution *from the app* (`limina
  install-guest-tools`) + the payload↔guest version-manifest check — designed in
  `docs/design/distribution.md`.

**Follow-ups:** DAX/shm window (`VirtioShmRegion` in `fs/device.rs`; confirm shm-window alignment +
FUSE_SETUPMAPPING/SHMCAP on 16 KiB host pages; the enhanced tier already runs a 16k guest so
guest-page = host-page is the default there — test stock-4k DAX separately) + host↔guest uid
mapping; a `liminactl status` consumer; images/files/HTML on the clipboard; reuse libkrun's macOS
host→guest time sync (DGRAM vsock port 123, `timesync.rs`) — confirm a guest-side consumer exists.

**libkrun patches:** none for the transport (vsock + virtiofs overlays already exist). Possibly a
small fix if a HANG_UP'd port can't cleanly reconnect without a VM restart (`unix.rs:548-562`).

**Risks that remain live (for clipboard files/images):**
- NSPasteboard promised-data provider blocking long enough to round-trip a guest REQUEST/DATA
  without AppKit timing out the paste.
- Large chunked vsock transfers respecting credit flow control without stalling muxer threads
  (max-size cap / temp-file staging).

**Clipboard test-coverage gaps (tracked):** initial-offer-on-connect (live-verified, not in L1);
stale-serial races; multi-peer broadcast + dead-peer pruning; helper resilience (vsock reconnect
after supervisor restart, D-Bus session death → clean exit); the ext-data-control (enhanced) backend
under automation (`l1_session_helper` exercises only the RemoteDesktop fallback; the Wayland backend
is live-verified only — closing it needs a headless ext-data-control stand-in or a real compositor
in the L1 guest).

---

## Milestone 6 — Dynamic memory (balloon, min..max)

**Status: DONE (2026-06-26).** All four tasks landed and are verified end-to-end on stock F44; the
build-by-build plan + as-built log is `docs/design/m6-dynamic-memory.md`. One deliberate deviation
from the task list below: task 3's "public balloon **C API**" was built as the project-standard
**internal Rust API** (`BalloonControlHandle`, mirroring `DisplayResizeHandle`) plus a
`--balloon-control-socket`, not a `krun_*balloon*` C ABI — consistent with the no-C-ABI decision.
Tests: `crates/limina-test/tests/balloon.rs` (FRQ reclaim drops `phys_footprint`),
`balloon_inflate.rs` (target→`actual`→guest), `balloon_psi.rs` (PSI policy inflate/deflate), plus
coalescer + policy + proto unit tests.

> **Post-M6 decision (2026-07-20): drop `DEFLATE_ON_OOM`** (reverses task 2's "advertise it"
> below). The bit is why a freshly booted VM looks out of memory: Linux keeps ballooned pages in
> `MemTotal` (counted as *used*) exactly when the bit is negotiated, and only subtracts them from
> the totals without it — so dropping it makes balloon inflation transparent (`MemTotal` tracks
> effective RAM, usage reads true) on stock and enhanced guests alike, host-side only. Its
> guest-side OOM net is also preempted by systemd-oomd on modern Fedora anyway. Costs and required
> compensations (RED-first allocation-burst test before the drop, possible per-VM escape hatch):
> full analysis in the 2026-07-20 addendum of `docs/design/m6-dynamic-memory.md`.

**Goal:** the VM is given a `min..max` RAM range; it takes memory under guest pressure and returns
it to macOS when idle, with `phys_footprint` actually dropping.

> **The case has numbers (measured 2026-06-11 on the tier-2 desktop):** host RSS is a guest-page
> **high-water mark** — 5.2 GiB at boot-idle → 6.8 GiB after a browsing session, and it never
> returns without a balloon. (Shared mappings: 4 GiB guest RAM + the 8 GiB venus shm window, lazily
> mapped.) Guest idle ~2 GiB is Fedora's own daemons, not VM overhead — so reclaim is the lever,
> not guest slimming. GPU/Metal buffers recycle correctly, so the balloon is the remaining story.

**Key tasks (in order — the first is cheap and makes the existing path actually work):**
1. **Fix free-page-reporting reclaim.** Replace `libc::MADV_DONTNEED` at balloon `device.rs:100`
   with macOS `MADV_FREE_REUSABLE`/`MADV_FREE_REUSE` so `phys_footprint` drops — **spike-confirmed
   required** (`spikes/balloon-madvise`: DONTNEED returns nothing, REUSABLE reclaims fully even while
   `hv_vm_map`'d). Then enforce 16 KiB host-page alignment/coalescing in `process_frq` for the
   4K-guest/16K-host mismatch (page-size menu option (a) — see Risks; doc 08 §1.2). This alone makes
   the one already-working path (free-page reporting) effective.
2. **Implement the stubbed inflate/deflate handlers** (`event_handler.rs:14-40`, currently log
   "unsupported" and drain the eventfd) and set `num_pages`/`actual` + a config-change interrupt for
   target-driven shrink toward `min`. Advertise `VIRTIO_BALLOON_F_DEFLATE_ON_OOM` (not in
   AVAIL_FEATURES today, `device.rs:27-30`) as an OOM safety net before driving inflate.
3. **Add a public balloon C API:** `krun_add_balloon(min, max, flags)` / `krun_balloon_set_target` /
   `krun_balloon_get_actual` / `krun_balloon_get_stats`. libkrun stays mechanism-only; the PSI/PID
   policy lives in limina.
4. **PSI autoballoon agent** in the guest over the M5 vsock control channel: report
   `/proc/pressure/*` + MemAvailable; limina drives `set_target` between min and max with hysteresis.
   Requires `CONFIG_PSI=y` + `psi=1` in the guest kernel cmdline.

Phase 0 (done in M1): static `ram_mib` via `krun_set_vm_config` + demand paging. Defer
**virtio-mem** entirely (does not exist in libkrun; large/risky).

**libkrun patches:** the core of this milestone — reclaim fix, 16 KiB alignment, inflate/deflate
handlers, DEFLATE_ON_OOM feature bit, and the new `krun_*balloon*` C API.

**Done test:** start a VM with `--memory 2G..12G`; run a memory-heavy guest workload and watch
`actual` rise toward max; quit it and watch `vmmap`/Activity Monitor show limina's `phys_footprint`
drop back toward 2G as pages are madvised back to macOS.

**Risks / spike first:**
- **Spike #1 — RESOLVED** (`spikes/balloon-madvise`, 2026-05-30): `MADV_FREE_REUSABLE` drops
  `phys_footprint` fully on the HVF-mapped MAP_ANON region with no `hv_vm_unmap`/`hv_vm_protect`
  first; `MADV_DONTNEED` returns nothing, `MADV_FREE` is lazy. Re-confirm on the shipping macOS
  version.
- **The 4K↔16K page-size mismatch — the menu collapsed in our favor (M4 learning).** The enhanced
  tier already runs a `CONFIG_ARM64_16K_PAGES` kernel (venus requires it), so option (b) is the
  shipped enhanced config: **1:1 reclaim is free on the enhanced tier.** Host-side coalesce/align in
  `process_frq` **(a)** is the *stock-tier* fallback (measure how much stock 4 KiB Fedora reporting
  actually returns — still the spike). Option **(c)** — host-page-aware `mm/page_reporting.c` via a
  feature bit — is now cheap to carry if (a)'s waste is material (the kernel-patch pipeline exists,
  `patches/linux/*.patch`). **(d)** virtio-mem stays later. Doc 08 §1.2.
- Re-touch latency/cost of MADV_FREE_REUSE on deflate for an interactive desktop.
- PSI watermark/hysteresis tuning to avoid balloon thrash (build/browser/IDE workloads).

### Dynamic vCPU hotplug — the CPU sibling of ballooning (planned)

**Status: mechanism half SHIPPED (robustness, task #40, libkrun 0094). Dynamic policy DROPPED
(task #35, 2026-07-22) — the go/no-go gate below failed.** The same `min..max` philosophy as
ballooning, on the CPU axis: dynamically offline idle guest vCPUs to shrink the worker's in-kernel
IPI/timer host-wakeup budget, and re-online them under load. A boot-time A/B against the OLD 18.6k
baseline measured 6→2 vCPUs cutting host wakeups ~3k/s (IPI −67%, timer −56%) at flat throughput
on the GPU-bound blobs workload.

**GATE RESULT (2026-07-22) — #35 DROPPED.** Re-ran the 6-vs-2 A/B on the CURRENT shipped stack
(virgl 0043 + mesa 0017), clean-fullscreen blobs, same method (`spikes/wakeup-probe/RESULTS.md`):
6-vCPU ~8.1k/s → 2-vCPU ~7.3k/s = **only −800/s host wakeups**, below the 1k/s drop bar set in the
caveat. The guest half still moved a lot (IPI1 −75%, arch_timer −35%) — the mechanism works — but
the round-2 fixes made the HOST budget dominated by the ~5.9k/s vkr_ring poll-sleeps, which are
INVARIANT to vCPU count, so the vCPU slice is now a small absolute number. Throughput was already
fine and idle vCPUs are near-free under NO_HZ (~130/s floor). Building the full
signal→policy→mechanism→hysteresis machinery (with real oscillation risk) for <800/s of
nanosleep-class wakeups is not worth it. **Only the #40 robustness fix was worth keeping** (a stock
guest offlining a vCPU no longer wedges the VMM — a two-tier-guarantee fix, shipped). Re-run this
gate ONLY if the post-M13 vkr doorbell-handshake lands and cuts the poll-sleep floor (which would
make the vCPU slice a larger relative fraction again).

**The two halves (mechanism in libkrun, policy in limina):**
1. **Mechanism — libkrun CPU-hotplug (task #40, a robustness fix worth shipping on its own).** A
   guest that offlines a vCPU today (`echo 0 > /sys/devices/system/cpu/cpuN/online`) **wedges the
   whole VMM** — a two-tier stock-guest-guarantee violation. Root cause (de-risk probe
   `vcpu-offline-probe.sh` + opus review, 2026-07-21): libkrun's PSCI
   (`hvf/src/lib.rs handle_psci_request`) models CPU_ON but **not CPU_OFF (0x8400_0002) or
   AFFINITY_INFO (0xc400_0004)** — both return `NOT_SUPPORTED`, so the dying vCPU busy-spins
   (probe saw the worker hit ~546% CPU) and the reaper polls AFFINITY_INFO forever; re-online is
   also broken (the secondary boot channel `boot_receiver.recv()` is one-shot, consumed at boot).
   Fix (SHIPPED, libkrun 0094): model CPU_OFF → park the vCPU thread cleanly (reuse the M9
   `handle_pause` park machinery, zero host CPU/wakeups); model AFFINITY_INFO (OFF for a parked
   vCPU, ON otherwise); make CPU_ON re-deliverable at runtime (durable per-vCPU control channel;
   PC/X0 reset on the owning thread since HVF register access is thread-bound); IRQ/vtimer
   re-affinity on re-online. Guarded by `tests/l2_vcpu_hotplug.rs`.
   **Known limitation → task #41 (DEFERRED, kept tracked):** the guest-visible online state lives
   in `VcpuList` and is NOT in the M9 snapshot, so a snapshot taken while a vCPU is offline restores
   it as online (guest-kernel/host bookkeeping diverges; a later re-online times out gracefully — no
   wedge). Also CPU_ON has no ALREADY_ON idempotency guard. Deferred 2026-07-22: with #35 dropped the
   trigger is manual-only + graceful, not worth the M9 snapshot-format-change risk now; revisit if
   vCPU offlining resurfaces or during an M9 hardening pass.
2. **Policy — limina-agent (task #35, DROPPED 2026-07-22).** The design would have been:
   agent-driven offline/online under a runnable-task-pressure signal (PSI `cpu`/loadavg/`nr_running`)
   with hysteresis + interactivity guardrails (never offline cpu0; step one at a time; fast re-online
   on a load spike; cooldown to avoid oscillation — the balloon-thrash lessons apply directly), host
   config exposing the range (`--cpus MIN..MAX` / `vm.toml [hardware]`), booting MAX, handing the
   range to the agent — mirroring `--memory MIN..MAX`. The full codebase map for this (guest sensor →
   host `VcpuPolicy` mirroring `BalloonPolicy` → cap-gated host→guest control message → agent sysfs
   write; offline/online is entirely guest-driven so it must route through the agent) was captured
   before the gate failed; if the gate is ever re-passed, that shape is the plan. Not built.

**Cost/benefit caveat — SATISFIED, verdict DROP (2026-07-22):** the ~3k/s was measured against the
OLD ~18.6k baseline; the round-2 fixes (libkrun 0091 EVENT_IDX + virgl 0041/0043 ring relax) attacked
the GPU/IO wake chain that *drives* the guest ttwu IPIs. Remeasured on the current stack (see the GATE
RESULT above): the vCPU host-wakeup slice is **~800/s at the 6→2 extreme, below the 1k/s bar → the
dynamic feature is a DROP; only the #40 robustness fix was kept.** Authoritative perf context:
`docs/perf/overhead-inventory.md`.

---

## Milestone 7 — USB passthrough

**Status: 🟡 mock passthrough works end-to-end; real-device capture proven; the helper is DEFERRED.**
Build-by-build plan + as-built log: `docs/design/m7-usb-passthrough.md`. Shipped & verified on HVF:
**(1) kernel** — `build-test-kernel.sh` enables USB + `USBIP_VHCI_HCD` + class drivers + `uinput`;
**(2) host `limina-usbip` crate** — the full USB/IP wire protocol (byte-exact to the kernel source) +
a `UsbBackend` trait + a hardware-free **CDC-ACM mock** + a libusb (`rusb`) backend, 17 unit tests;
**(3a)** the guest-side USB/IP stack is present (`tests/usb.rs`, GREEN); **(3b)** the full
**mock-attach end-to-end** — a CDC-ACM device enumerates in the guest as `/dev/ttyACM0` over vsock
with NO hardware (`vhci_hcd` accepts the `AF_VSOCK` fd directly — no `usbip` userspace tool or kernel
patch), GREEN. **(4) real-device — DEFERRED:** claiming an Apple-bound USB device works **as root with
NO entitlement** (proven on a Solo 2 via `sudo spikes/usb-probe/run.sh`; the
`com.apple.vm.device-access` entitlement is App-Store-only + un-ad-hoc-signable, so a dev key can't
unlock it — root is Apple's documented Developer-ID fallback). The remaining code is the **root USB
capture path, built as the first client of a single shared privileged helper** (`limina-privhelperd`,
which will also broker vmnet networking + future root ops) — see `docs/design/privileged-helper.md`.
Not CI-testable (needs root + the physical device).

**Goal:** pass a host USB device (initially libusb-claimable: FTDI/CP210x, YubiKey-class) into the
guest.

**Key tasks (USB is entirely net-new; the bundled/custom guest kernels have USB compiled OUT today):**
1. **PREREQUISITE — enable USB in OUR kernel config (cheap now).** The enhanced tier standardized on
   our own kernel (`scripts/build-test-kernel.sh`, 16 KiB pages, `patches/linux/` auto-applied), so
   this is a config edit in *our* pipeline, not a libkrunfw rebuild: add `CONFIG_USB_SUPPORT=y`,
   `CONFIG_USB=y`, `CONFIG_USBIP_CORE`, `CONFIG_USBIP_VHCI_HCD`, and needed `CONFIG_USB_*` class
   drivers (while there: `CONFIG_UINPUT` — its absence already bit us; ydotool is unusable without
   it). libkrunfw's bundled kernel has USB off in every arch profile
   (`config-libkrunfw_aarch64:2151`) but only matters where limina still uses it (L1 fallback); the
   EFI distro kernel already has USB.
2. **Start with USB/IP, not a native device.** Guest side is 100% upstream (`vhci_hcd` + `usbip`)
   once USB is enabled:
   - **C:** prototype USB/IP over virtio-net/TCP (stock `usbip attach -r`, no guest patching beyond
     the kernel rebuild).
   - **B:** move the transport to libkrun's existing **virtio-vsock** (no VMM device patch).
   - **D (mid-term):** native virtio-usb device in libkrun carrying USB/IP PDUs, exposed via a
     `krun_add_usb*` C API modeled on `krun_add_input_device`'s opaque config_backend+size pattern.
3. **Host device claiming.** v1 targets **libusb-claimable** devices (no Apple driver bound).
   Apple-bound interfaces (mass storage, standard HID, recognized audio) need a device-access
   entitlement and/or a DriverKit `.dext` — deferred. Isochronous transfers (webcams/audio) out of
   scope for v1.
4. **Cheap early win (can land first):** serial-via-virtio-console for FTDI/CP210x dev boards
   (`krun_add_virtio_console_multiport` / `krun_add_console_port_inout`), independent of the USB
   stack.

**libkrun patches:** our-kernel config edit (prerequisite); later a native virtio-usb device +
`krun_add_usb*` API (Option D). USB/IP-over-vsock needs no VMM transport patch.

**Done test:** plug a USB-serial adapter or YubiKey into the Mac; `limina usb attach <id>`; the
device node appears in the guest (`lsusb` shows it; `/dev/ttyUSB0` or the YubiKey enumerates) and
works.

**Risks / spike first:**
- Does the larger USB-enabled kernel still boot within libkrun's memory/boot constraints?
- Will `vhci_hcd`'s attach accept an AF_VSOCK fd unmodified, or do we need a userspace vsock→vhci
  bridge / small kernel patch?
- Empirical macOS 26 device-claiming matrix (FTDI, YubiKey, mass-storage, webcam, USB keyboard):
  which libusb claims freely vs which Apple blocks; exact entitlements UTM/krunkit use.
- Throughput of USB3 mass storage over USB/IP-vsock vs just using virtiofs (may make storage
  passthrough unnecessary).

---

## Milestone 8 — Audio + x86 emulation + polish

**Goal:** sound output/input, x86 binary support, and the desktop polish that makes limina a
Parallels replacement: fullscreen, keymap remap, multi-display, system-combo capture.

**Key tasks:**
1. **Native virtio-snd driving CoreAudio.** Implement a NATIVE in-VMM virtio-snd device in libkrun
   (fill the empty `snd` feature, reserved id `KRUN_VIRTIO_DEVICE_SND=25`), modeled on the in-tree
   rng/console devices that work on macOS. Do NOT rely on vhost-user-snd: the entire vhost-user path
   is `cfg(target_os="linux")` + memfd-dependent and vhost-device-sound has no CoreAudio backend.
   Mic capture adds the rx queue path + macOS mic TCC permission inside the app bundle.
2. **x86 emulation: guest-side, not Rosetta.** Rosetta-for-Linux is bound to Vz and unavailable to a
   HVF VMM. Use **FEX-Emu** in the guest (primary) + `qemu-user-static` (fallback) via `binfmt_misc`
   (`CONFIG_BINFMT_MISC=y` — no libkrun patch; wire via the guest agent / virtiofs overlay).
3. **Polish (each largely a host-side limina feature unless noted):**
   - ~~**Fullscreen:**~~ **DONE** — NSWindow `FullScreenPrimary` collection behaviour +
     `toggleFullScreen:`, triggered host-side by `Cmd-Ctrl-F` (the macOS-standard combo).
     `CGDisplayCapture` exclusive mode still optional/deferred.
   - ~~**Keymap remap (Command/Option swap):**~~ **DONE** — host-side `KeyRemap` policy over the
     positional kVK_* → KEY_* table; guest owns the layout so dead keys/IME work natively. **Now ON
     by default** (PC-style muscle memory out of the box); `--no-swap-cmd-opt` opts out, the original
     `--swap-cmd-opt` is kept (back-compat, last-wins). Fully customizable keybindings beyond the
     swap still ahead.
   - ~~**System-combo capture (Cmd-Tab/Cmd-Space/Ctrl-arrows):**~~ **DONE (keyboard)** — the capture
     CGEventTap consumes keyDown/keyUp/flagsChanged while captured and forwards them to the guest,
     so system key-combos act in the guest, not the host. Re-enables on `kCGEventTapDisabledByTimeout`.
     **Limitation:** multi-finger trackpad gestures (Mission Control / Spaces swipe) are processed by
     the WindowServer upstream of a session tap and are NOT interceptable (two-finger scroll is).
     Secure Input (password fields) can still suppress the tap — acceptable.
   - ~~**Pointer capture:**~~ **DONE** — `Cmd-Ctrl-G`. A **session-level consuming
     CGEventTap** (needs Accessibility permission) intercepts mouse + keyboard while captured.
     Since 2026-07-23 captured motion integrates the macOS-accelerated deltas into a host-side
     **virtual cursor** (seeded where the pointer was grabbed, clamped to the fit rect) and drives
     the **absolute tablet** — the same device/mapping as uncaptured mode, so movement feels
     exactly like the host cursor and release warps the cursor back to where the virtual cursor
     ended. This retired the old workarounds (`LIMINA_CAPTURE_SENS` host scale + the enhanced-tier
     flat guest pointer profile); the separate relative-mouse virtio-input device now carries
     only the edge-clamped motion overflow as *pressure* (mutter pressure barriers — GNOME's
     hot corner — fire on motion pushed INTO a barrier while pinned, which a pre-clamped
     absolute stream can't express), and seeds a future explicit mouselook/game mode. The
     capture tap only grabs when its window is key (it's a session tap and sees the combo
     system-wide; release stays ungated as the escape hatch). **Ctrl-Opt (press and release
     alone, VMware-style) ungrabs**; any key/click mid-chord cancels it, so guest Ctrl-Alt-*
     combos still work grabbed; capture toggles force-release all guest modifiers so nothing
     wedges across the boundary. **Soft keyboard grab (default ON, `--no-soft-kbd-grab`)**:
     while the VM window is key and not fully grabbed, the tap consumes ALL keyboard input —
     system combos included (Cmd-Tab acts in the guest) — while the mouse stays free; losing
     key status returns the keyboard instantly (click anywhere else), and Ctrl-Opt mutes it
     until the window regains focus. The tap forwards keys through the shared `InputState`
     (one pressed-set/caps-sync bookkeeping for tap + monitor). The guest cursor is
     composited at its reported `cursormove` position (host NSCursor hidden); **closes the M2
     guest-warp gap**. If Accessibility is denied, `CGEventTapCreate` returns NULL and it falls
     back to a leaky local-monitor warp path (same virtual-cursor motion, weaker containment).
   - **Multi-display:** multiplex all displays through the single `krun_set_display_backend` by
     `scanout_id` (up to 16), each to its own NSWindow/CAMetalLayer. **The thinnest plan among the
     named features** — needs a design doc (guest-side mutter multi-monitor via multi-scanout,
     HiDPI, interaction with runtime resize, and the `frame <id>`-carries-no-scanout-id wire gap
     flagged in `docs/reviews/2026-07-01-full-review.md`).
   - ~~**Runtime window-follow resize / EDID hotplug**~~ — **✅ DONE (2026-06-23)**: window resize
     reflows the guest resolution live, no reboot (libkrun 0025/0026, the `--display-control-socket`
     transport, 60Hz-debounced window trigger). Design + as-built:
     `docs/design/runtime-display-resize.md`.
   - ~~Hardware cursor~~ — done in M2. ~~Zero-copy scanout~~ — done in M4.
   - ~~**Capability-scope the scanout IOSurfaces**~~ — **✅ DONE (2026-06-23)**: both display paths
     hand NON-global IOSurfaces to the supervisor by Mach port (`limina-surfaceport`), closing the
     `IOSurfaceLookup` screen-read hole; `iosdump` now needs `LIMINA_GLOBAL_SCANOUT=1`.
   - **CapsLock/NumLock LED parity (libkrun patch):** surface the statusq LED feedback
     (`worker.rs:238-248` no-op).

**libkrun patches:** native virtio-snd (largest); optional LED parity. (The runtime
display-reconfigure call shipped as 0025/0026.)

**Done test:** audio plays from a guest app through the Mac speakers (and mic capture works); an x86
Linux binary runs in the arm64 guest via FEX; the window goes fullscreen on a Retina display,
Command/Option are swapped per config, Cmd-Tab is captured when the toggle is on, a second display
attaches at runtime, and resizing the window reflows the guest resolution.

**Risks / spike first:**
- **Spike #1: native virtio-snd.** Can a minimal `snd/` device (modeled on rng/console) enumerate a
  card and play a tone through a CoreAudio AudioUnit? Measure round-trip latency vs Parallels and
  buffer/clock matching (period/buffer sizes vs render callback) to avoid xruns.
- Confirm FEX `binfmt_misc` auto-wiring works under our HVF launch path (vs needing the agent/image
  to set it up).
- CGEventTap disable frequency / Secure Input handling in practice.

---

## Milestone 9 — Suspend / resume + full VM snapshots (host-side)

**Status: 🚧 M9.1 + M9.2 DONE (headless); M9.3 = GPU, next.** M9.1 (host-side vCPU+GIC+RAM snapshot
mechanism) and **M9.2 (headless device continuity) are shipped and GREEN** — `limina suspend`/`start`
suspends a running managed VM (GPIO suspend button → guest s2idle quiesces virtio to INIT → snapshot →
teardown → next start `--restore`s the same boot_id) with automated happy-path + abort-path L2 guards.
M9.2 is **headless-scoped**: virtio-gpu has no s2idle PM ops, so it's excepted from the quiesce oracle —
suspending a **windowed/GPU** VM is **M9.3**, the next milestone. Full design (decision + rationale, GPU
prior-art, two-tier mapping, M9.0–M9.4 build plan, founding spikes, the demoted guest-side-S4 analysis,
and the **2026-07-18 Fable M9.3 review**) is `docs/design/m9-suspend-resume.md`; M9.2 build detail in
`docs/design/m9.2-quiesced-snapshot.md`. This is the roadmap-shaped digest.

**Follow-up (2026-07-20, LOW priority): guest-kernel virtio-gpu PM ops.** The root of the whole
thaw-reset dance is a *Linux* gap: `virtgpu_drv.c` registers no `.freeze`/`.restore` PM ops, so
every s2idle thaw takes the virtio core's bus fallback (reset → renegotiate → DRIVER_OK with zero
queue re-programming) — broken on any spec-faithful device; libkrun 0072's sticky re-arm and the
defer-and-classify session reset (`docs/design/host-sleep-s2idle.md`) are host-side leniencies for
it. The proper fix is guest-side: carry/refresh the unmerged **Dongwon-Kim drm/virtio
freeze/restore series** in `patches/linux` (enhanced tier) and upstream-report the core gap.
Deliberately low priority: the host-side fixes are sufficient on their own and cover **stock**
guests, which the kernel fix never can; this item's value is upstream hygiene + the clean PM path
for enhanced guests.

**Goal:** Parallels-parity "Suspend" — freeze the running guest to a host-side file in a second or two,
**tear the worker process down** (reclaim all host RAM, the GPU/Metal/IOSurface graph, gvproxy), and a
later "Resume" that restores the *same* desktop (open apps, correct wall clock, working accelerated
display) — **plus** full VM snapshots as a feature (save / restore / clone / roll back named snapshots).
Lifecycle/snapshot is the last uncovered headline Parallels feature (`GAPS-and-verification.md`).

**Decision (pivoted 2026-06-28): host-side VMM snapshot is primary, GPU via Strategy A.** Pause the
vCPUs, quiesce the virtio threads, serialize vCPU + device + GIC state + guest RAM to a file, kill the
worker; restore = relaunch with `--restore`. This is Firecracker/crosVM/Parallels-shaped, it's
**GPU-agnostic**, and it's what unlocks the full-snapshot feature category (guest-side S4 never could).
We pivoted *away* from guest-side Linux S4 as the primary because it has too many distro-coupled failure
points, it can't do snapshots, and spike #1 showed it's blocked by two libkrun HVF gaps anyway — the
host-side path **sidesteps** those gaps (it pauses vCPUs externally and never runs the guest suspend
path). S4 is demoted to Appendix A of the design doc.

**The GPU — Strategy A (quiesce + guest re-init), NOT serialization.** This is the load-bearing call,
backed by a prior-art research pass + the user's primary-source Parallels data:
- **True host-side GPU-state serialization is unprecedented and infeasible for our stack.** QEMU blocks
  it (`migrate_add_blocker("virgl is not yet migratable")` — virgl *and* venus), crosVM/rutabaga's virgl
  `snapshot()`/`restore()` are `Err(Unsupported)` stubs, virglrenderer has zero save/restore entry
  points, Cuttlefish snapshots software-render-only, and **even Apple's `VZVirtualMachine.saveMachineStateTo`
  refuses the virtio GPU**. On Metal there's no vendor save/restore interface, and venus host-readback
  isn't CPU-coherent (#28).
- **The market leaders do A, not B.** Parallels presents **stock virtio-gpu + Mesa virgl** to Linux
  (user-verified: `1af4:1050` + `virgl (Apple M4 Pro)`, Mesa 26.0.8); its suspend kills the host process,
  so the GPU context is reconstructed on resume. VMware Fusion/Workstation suspend virtual-3D too via
  **guest-backed objects** (canonical copy in guest RAM, host object a derived cache) — so **Parallels is
  not unique**. The decisive pattern: suspend-with-3D works iff the resource graph is **guest-backed**
  (VMware MOBs; virtio-gpu `ATTACH_BACKING`/blob = our case), and fails when host-side-only (Hyper-V
  GPU-PV *disables* checkpoints; VirtualBox can't).
- **So A is ours to own, but the guest-side work splits by tier** (sharpened by spike #3 — a *live* guest
  does **not** survive abrupt GPU-device loss, so this work is required, not optional). **Foundation
  (both):** the **kernel virtio-gpu DRM driver** — carry the **Dongwon Kim freeze/restore series** (only
  the kernel owns the guest-id↔host-resource map, so only it can resubmit the resource/context creates).
  **virgl (GL) tier:** that ~suffices (virglrenderer rebuilds from the resubmitted stream + guest-backed
  contents; Mesa virgl ≈ transparent) — the Parallels/VMware-proven baseline. **⚠ this "≈ transparent" is an
  unverified inference** — it never got a source spike like venus did; run one alongside M9.1. **venus
  (Vulkan) tier:** NOT enough — the host render-server's VkObject graph (→ Metal) is gone on a fresh worker,
  so it needs a **Mesa-venus object-graph replay** (`src/virtio/vulkan/`; unsolved upstream — the
  venus-resume spike) **plus a snapshot-time readback of device-local resource *contents*** (textures in
  Metal heaps are captured by *nothing*; replay alone brings objects back **empty** — this is M9's long
  pole) + the host-visible blob copy-back (`TRANSFER_FROM_HOST` at snapshot). Restore uses a **fresh worker**
  (empty renderer) — **no in-process renderer-reset hook needed** (that earlier framing was a
  misdiagnosis). Record/replay B is a later optional upgrade, out of M9 scope.

**Two-tier:** stock guests get suspend/snapshots transparently, but with a **GPU re-init disruption** (no
guest resubmit → spike #3 showed a *live* 3D session crashes/recovers, not a mere flash). Enhanced guests get
seamless re-init: **virgl** via the kernel Dongwon-Kim resubmit; **venus** additionally via the Mesa-venus
replay + device-local content readback + blob copy-back — all triggered by the agent **freeze bracket**
(`m9-freeze-trigger.md`), which also fixes the wall clock. **A degrades gracefully; B wouldn't even start.**

**Reuse (why it's tractable):** rides the existing **reboot=relaunch** spine (`WORKER_EXIT_REBOOT` →
relaunch a fresh worker, host resources survive) — suspend adds a third exit disposition
("snapshotted", code 126) + a `--restore` mode; the windowed fd-swap into a live `WorkerConn`,
surface-port persistence, reconnect-tolerant control plane, gvproxy recycle, and the
**already-long-sleep-aware port-123 timesync** are all in place. New `limina-proto`
`Snapshot`/`Restore`/`TimeSet` messages (update both guest binaries).

**Build plan (bisectable):** M9.0 founding spikes (gate everything) → M9.1 **multi-vCPU** pause + RAM + vCPU
snapshot (no-GPU/sw-2D guest first) → **M9.1.5 spike F: the guest freeze/restore trigger** → M9.2 virtio
device state + GIC → M9.3 GPU via Strategy A: **virgl tier first** (carry Dongwon-Kim kernel resubmit +
snapshot-time GPU quiesce), then the **venus tier** (Mesa-venus object-graph replay + **device-local content
readback** + blob copy-back, gated on the venus-resume spike) → M9.4 full-snapshot feature (save / restore /
clone / roll back) + suspend/resume UX + capability probe.

> **The trigger gap (2026-07-17 review):** an external-pause snapshot never runs the guest's PM path, so the
> Dongwon-Kim resubmit (and venus replay) **never fires** on its own. Decided in `docs/design/m9-freeze-trigger.md`:
> an **agent-coordinated suspend-to-idle bracket** on the enhanced tier (also restores the wall clock,
> dissolves the `ICC_RPR_EL1` edge, drains virtio I/O); the stock tier keeps the raw snapshot with a GPU
> re-init disruption (may cost a live 3D session — recovers). Gated on spike F.

**libkrun patches:** real HVF pause/quiesce (`VcpuEvent::Pause`/`Resume` + `resume_vcpus` are inert today —
*and the pause must wake WFE-parked vCPUs blocked on a channel `recv()`*), vCPU `save_state`/`restore_state`
(wrappers — the `hv_vcpu_get/set_sys_reg` / `set_simd_fp_reg` / `set_vtimer_offset` FFI **already exists**),
in-kernel GIC state get/set (spike #2 proved it round-trips → the userspace `GicV3` fallback is **not
needed**), `CNTVOFF` set on restore, a `--restore` boot mode, a versioned device-state schema (+ mapped-blob
set), virtio freeze/thaw hardening, and a snapshot-time GPU **quiesce** (drain fences before the worker dies
— restore is a fresh worker, so **no in-process renderer reset**). *(The `reset_session` rutabaga-context
drop is **already shipped** — patch 0035 — not M9 work.)* Guest side: carry the `patches/linux` Dongwon-Kim
drm/virtio freeze/restore series (virgl), + Mesa-venus replay + device-local content readback (venus). (The
PSCI `CPU_OFF`/`OSDLR_EL1` gaps that blocked guest-side S4 are **not** on this path.)

**Founding spikes (M9.0):** (1) ✅ **DONE (2026-06-28, `spikes/s4-hibernate/RESULTS.md`)** — guest-side
S4 inside libkrun is correctly wired but blocked by two HVF gaps (PSCI `CPU_OFF`/`AFFINITY_INFO`;
`OSDLR_EL1` debug sysreg); *bearing:* this is why we pivoted host-side (which sidesteps them) and demoted
S4. (2) ✅ **GREEN (2026-07-01, `spikes/m9-hvf-state-roundtrip/RESULTS.md`)** — HVF round-trips the **full**
vCPU + in-kernel-GICv3 state into a fresh VM, guest continues identically (118/120 EL1 sysregs, all accept
`set` post-run; GIC blob byte-identical; timer-across-the-gap fires; `ICC_RPR_EL1` read-only → **quiesce to
no-IRQ-in-service before snapshot**). **M9.1/M9.2 unblocked; the userspace-`GicV3` fallback is not needed;
the vCPU/GIC round-trip is no longer the top risk.** Non-gating deltas left to M9.1: multi-vCPU, MMU-on,
pending SPIs. (3) 🟡 **PARTIAL (2026-06-28, `gpu-reset-live.md`)** — a live `virtio_gpu` unbind/rebind,
three rounds. **Proven:** the host worker is robust (survives resets under any load); the clean path
cold-rebuilds a correct desktop (pixel-verified). **Decisive (round 3, raw unbind, session live): a running
guest session does NOT survive abrupt GPU-device loss** (gnome-shell + glxgears + vkcube crash) — so
guest-side resubmit is *required*, not optional. Root cause in our source: `reset()` keeps the renderer
alive (`device.rs:379`) + the orphaned-context half is **now fixed** (`reset_session` drops rutabaga
contexts via patch 0035, `virtio_gpu.rs:715`). **Corrected:** that collision was a same-worker artifact
(restore = fresh worker), so the real gate is **guest-side** (kernel Dongwon-Kim for virgl; Mesa-venus replay
for venus) —
**not a libkrun renderer-reset hook** (earlier framing was a misdiagnosis). Green light to *start* M9.3
guest-side, not a sign-off. Gates M9.3.

**Done test:** human-verified suspend→resume *and* snapshot→restore→clone on **both** a stock and an
enhanced image; the guest comes back to the same desktop, the clock is correct after a real multi-hour
suspend, venus re-enumerates, and the host RAM was actually freed while suspended.

**Risks / ASSUMED (spike-gated), reframed 2026-07-17:** the top risk is **no longer** the vCPU/GIC
round-trip (spike #2 retired it) — it is **the venus tier's device-local content capture** (`m9-suspend-resume.md`
§4b): object-graph replay re-creates VkObjects *empty*, and device-local textures live in Metal heaps captured
by nothing → they need a snapshot-time venus readback sweep. **Second:** the "virgl ≈ transparent" premise is
an *unverified inference* (unlike venus it got no source spike) → run a virgl-tier source spike alongside M9.1;
if it collapses both tiers become Mesa-replay-shaped. **Third:** the freeze-trigger gap (`m9-freeze-trigger.md`,
decided, spike-F-gated). Still open: re-mmap+`hv_vm_map` at original IPAs behaves like first boot; the stale
host-visible blob mappings must be re-established before vCPUs run (§3); the Dongwon-Kim series applies cleanly
to our kernel (carry out-of-tree). HVF has **no first-class dirty-page log** → a stop-the-world full RAM dump
(multi-second stall) — fine for suspend, a UX note for snapshotting a live VM (`hv_vm_protect` DIY dirty-log is
a future option, out of M9 scope).

---

## Milestone 10 — Additional block devices (multiple disks + ISO/CD-ROM)

Two storage features, surfaced 2026-06-29 during the dogfooding migration (we wanted a rescue path
for offline filesystem repair after a migrated guest's v1-space-cache btrfs wouldn't mount on the
16k kernel — see `docs/hardening-backlog.md` / the migrated-guest diagnosis).

- **Multiple disks (cheap — internals already there).** The worker already models disks as a
  `Vec<DiskSpec>` and loops `add_block_device` over them (`limina-vmm/src/krun/mod.rs:132`); libkrun
  supports several block devices. ONLY the two CLIs hardcode a single `--disk: Option<PathBuf>`.
  Work: make `--disk` repeatable in both `limina` and `limina-vmm` (with a per-disk read-only
  qualifier, e.g. `--disk PATH[:ro]`), give each a unique `block_id` so the guest enumerates
  `vda, vdb, …` in order, and thread the vec through the supervisor→worker arg pass-through. Use
  cases: data/scratch disks, multi-volume guests, and **offline fs repair / rescue** — boot a rescue
  rootfs as `vda`, attach the target disk as `vdb`, then `btrfs check` / `btrfstune` / `fsck` it
  while unmounted.
- **ISO / CD-ROM boot — SHIPPED (Phase 3a, zero code).** It turned out *not* to need a cdrom-class
  device or new firmware work: an ISO is a read-only virtio-blk disk (the guest mounts ISO9660 off
  `/dev/vdX`), and our GOP firmware *already* boots El Torito EFI media (its `PartitionDxe`/FAT stack
  finds the embedded ESP and chainloads `BOOTAA64.EFI`). So `--cdrom file.iso` runs an installer /
  live / rescue ISO directly — verified end to end (firmware → GRUB → installer kernel). The only
  remaining piece is multi-bootable determinism (Phase 3b: host-managed `BootOrder`), still deferred.

Neither is urgent: the btrfs case that motivated them is fixed more simply by adding
`space_cache=v2` to the root cmdline (no rescue needed). These are general capability gaps to close
when convenient.

**Full design: `docs/design/m10-multiple-disks.md`** (2026-06-30). It confirms the whole stack
below the CLI is already multi-disk capable, and that the order of `--disk` options controls disk
order: first `--disk` → `vda`, second → `vdb` (host-side deterministic in source; guest naming
follows under the default synchronous probe). Both shipping tiers (stock + enhanced) boot
BLS `root=UUID=` with a dracut initramfs, so attaching data disks can't shift root; only the
dev/test direct-kernel path uses positional `root=/dev/vda3` (should move to PARTUUID). The two
genuinely-hard parts: *which* disk boots is a firmware BDS decision that attach order does **not**
control (matters once two disks are bootable), and a multi-disk VM needs disk-set identity for M9
snapshots because adding a disk renumbers the trailing vsock+net devices. It also scopes in image
*creation* (`--disk PATH:create=SIZE`) and a concurrent-attach lock, both of which a daily driver
needs.

**Status (2026-06-30):** Phases 0, 1, 3a, 4 + most of 2 shipped + HVF-validated. Repeatable
`--disk PATH[:ro][:create=SIZE]` (attach order = device order, empirically confirmed), sparse image
creation, writable-image `flock`, the discard-truncate durability fix (vendored/patched imago),
**stable identity** (libkrun patch 0038: virtio serial = `block_id` → clone/move-stable
`/dev/disk/by-id/virtio-<id>`), `--cdrom` sugar, **qcow2** data disks (Phase 4 — format
auto-detected by magic, read-write + reboot-stable), and **boot/install from an ISO** (Phase 3a —
an EFI-bootable aarch64 ISO attached as the sole disk wins firmware BDS out of the box: El Torito →
ESP → `BOOTAA64.EFI` → GRUB → installer kernel, **zero code**; the §11 firmware-BDS unknown is
resolved. Spike `spikes/m10-iso-boot/`, guard `tests/disks.rs::boots_efi_iso_to_bootloader`).
Remaining: the multi-disk **snapshot manifest** (deferred to M9, its only consumer) and **Phase 3b**
(host-managed `BootOrder` via a baked EFI varstore — scripted/unattended installs + multi-bootable
determinism; deferred, a productization concern not a boot blocker). The dev-path PARTUUID switch
stays deferred (§11).

---

## Milestone 11 — Perfected productization (build / dev / delivery ergonomics)

**Status: 🟢 the `cargo xtask` command surface is shipped (2026-07-16).** Building, developing, and
running limina is now one obvious command per task instead of a spread of shell scripts you have to
know about. The commands are thin *orchestration over* the existing, tested scripts (which stay the
source of truth), so a fresh clone is trivially buildable and the inner loop is short. What remains
is CI + distribution/notarization (below). It's the "smooth the rough edges we keep re-explaining"
milestone.

**The one-command surface (`cargo xtask <cmd>`, wiring in `xtask/src/main.rs`; `--help` lists it):**
- **`setup`** — fresh-clone bootstrap: `vendor` + `scripts/setup-hooks.sh` (enable the git hooks).
  The single command to run after cloning.
- **`vendor`** — materialize the gitignored `third_party/` trees (clone libkrun + virglrenderer if
  absent, apply each series, vendor+patch imago). Idempotent. Fixed the bootstrap deadlock the imago
  `[patch.crates-io]` introduced (a fresh clone can't `cargo fetch` through the not-yet-vendored
  patch path — the imago script downloads the `.crate` from crates.io directly). Confirms the repo
  model: **patch series committed (`patches/**`), source clones gitignored (`third_party/`).**
- **`build [--release]`** — `cargo build -p limina -p limina-vmm` + codesign the worker
  (`crates/limina-vmm/sign.sh`, hypervisor entitlement) + `check-virgl-link.sh` (the venus link
  guard). The inner-loop "make a runnable worker" step, previously split across cargo + two scripts.
- **`sign [--release]`** — just the worker codesign, when you built via plain `cargo` and only need
  the entitlement.
- **`test [--release] [args…]`** — wraps `scripts/test-boot.sh`: build + codesign + link-check +
  build the L1 guest + trap probe + run the HVF boot tests (`LIMINA_HVF_TESTS=1`). Extra args forward
  to the test run (`--test <name>` filters, a substring after `--`). The canonical "did I break boot".
- **`run --disk <enhanced.raw> [--no-net] [--cpus N] [--ram-mib N] [-- extra…]`** — boot an
  enhanced-tier image to the seated venus desktop in a window (EFI+venus, the documented default
  boot). Builds+signs the debug worker, ensures `/Volumes/mesa-cs` is mounted, then hands off to
  `spikes/venus-draw-probe/boot-enhanced-efi-kk.sh` (which owns the KK/zink env). The disk boots in
  place — clone it first to keep it pristine. Fringe modes (`--kernel-inject`, `--gpu-software-2d`)
  stay as their own scripts, per CLAUDE.md.
- **`app [--release]`** — assemble the full self-contained `target/Limina.app` (the shipping bundle
  with the whole host venus/GL closure), wrapping `scripts/build-app.sh`.
- **`bundle [--release] [--open]`** — a *minimal* `Limina.app` that boots the L1 guest; the
  launch-path smoke test (LaunchServices launch → capture PNG). Distinct from `app`: `bundle` proves
  the normal launch path, `app` is the real deliverable.

**Out of scope for xtask (stay as scripts):** the heavy container-based native builds
(virglrenderer, guest mesa, KRUN_EFI, the `limina-build` image) are Docker/`container`-driven, not
pure orchestration.

**Still open:** CI that runs vendor→build→clippy→a subset of L1; the distribution side of the app
bundle (Developer-ID signing / notarization beyond the local Apple-Development identity). A one-page
onboarding doc shipped: `docs/dev-onboarding.md`.

---

## Milestone 12 — SPICE guest-agent support (baseline-tier clipboard first)

**Status: 📋 planned — not started.** Research done (2026-07-17); no code yet.

**Goal:** light up SPICE's `spice-vdagent` in an **unmodified** guest so a stock Fedora VM gets
integration features **with zero limina guest components installed** — starting with **clipboard
sharing**, then **client→guest file transfer** (drag-a-file-onto-the-window / "send file"). This is a
*baseline-tier compatibility on-ramp*, not a replacement for `limina-agent`: the native control plane
(M5) stays the enhanced-tier path; SPICE is the additive, per-feature story for guests that never
install our tools (per the two-tier guarantee).

**Explicitly out of scope: display resize.** SPICE's `VD_AGENT_MONITORS_CONFIG` is deliberately *not*
pursued — limina already does dynamic guest resolution natively via generated EDID + the display-mode
machinery (M2/M8, [[limina-display-modes]] / [[limina-display-resize]]), which is Wayland-native and
already shipped. SPICE would only duplicate it (worse, over its weaker Wayland path).

**File transfer complements virtiofs, doesn't duplicate it.** M5's virtiofs share is a *persistent
mounted folder*; SPICE file transfer is a one-shot **push of a file into the guest** (client→guest,
drag-and-drop or send-file, landing in the user's Downloads). That's a distinct Parallels-style UX
virtiofs doesn't give — so it's genuinely additive, which is why it's the interesting secondary here.
(Classic SPICE file transfer is client→guest only; guest→host drag-out is not part of the vdagent
file-xfer protocol and stays out of scope.)

**Why this is worth doing (the strategic case):** `spice-vdagent` is **already in the default Fedora
Workstation install** — **hard-verified 2026-07-17** by booting a clone of the stock
`Fedora-Workstation-43.accessible.raw` (kernel `6.17.1-300.fc43`, mesa 25.2.4, unmodified) and querying
the RPM db over SSH: `spice-vdagent-0.23.0-1.fc43.aarch64` is installed, both binaries
(`/usr/bin/spice-vdagent`, `/usr/sbin/spice-vdagentd`) present, and it is **dormant exactly as
predicted** (`/dev/virtio-ports/` holds only the krun console ports, no `com.redhat.spice.0`; daemon
`inactive`). The entire cost is therefore **host-side**; the guest install is $0. A stock guest that has
never seen `install-enhanced.sh` would gain clipboard (and then drag-in file transfer) the moment
limina speaks the vdagent protocol.

### Load-bearing facts (from the 2026-07-17 research)

- **Two guest components, both stock:** `spice-vdagentd` (system daemon, `spice-vdagentd.service`)
  and `spice-vdagent` (per-session, `/etc/xdg/autostart`). They talk to each other over a local Unix
  socket; `vdagentd` talks to the host over a **virtio-serial port**.
- **The trigger is a named virtio-serial port:** the host must expose a **virtio-serial (multiport)
  device with port name `com.redhat.spice.0`** → guest sees `/dev/virtio-ports/com.redhat.spice.0`.
  A stock **udev rule** matching that port name is what wakes `vdagentd`. No named port ⇒ agent stays
  dormant (today's state). libkrun currently gives us virtio-**console** (hvc0), not a SPICE-style
  **named multiport virtserial** — so this is a **new libkrun device/patch** (nearest existing
  plumbing: the console work behind `limina-guest-console`).
  - **HARD-VERIFIED on a stock F43 boot (2026-07-17):** the exact stock rule is
    `/usr/lib/udev/rules.d/70-spice-vdagentd.rules`:
    `ACTION=="add", SUBSYSTEM=="virtio-ports", ENV{DEVLINKS}=="/dev/virtio-ports/com.redhat.spice.0", ENV{SYSTEMD_WANTS}="spice-vdagentd.socket"`.
    So merely exposing the named port host-side pulls `spice-vdagentd.socket` → daemon, with **zero
    guest changes** — the M12 premise, confirmed empirically (`spice-vdagentd.service` is `static`,
    `Requires=spice-vdagentd.socket systemd-logind.service`; `/etc/xdg/autostart/spice-vdagent.desktop`
    present).
- **The protocol is coupled to a SPICE server as message broker.** The vdagent wire format
  (`VDIChunkHeader` [8 B: `port`,`size`] wrapping `VDAgentMessage` [`protocol`,`type`,`opaque`,`size`,
  data], types `VD_AGENT_CLIPBOARD*` / `VD_AGENT_MONITORS_CONFIG` / `VD_AGENT_MOUSE_STATE` /
  `VD_AGENT_ANNOUNCE_CAPABILITIES` / `VD_AGENT_REPLY`) assumes a SPICE server routing chunks between
  guest and client. limina must **implement the host end of that broker itself** and translate to our
  own AppKit/Metal + control-plane primitives — we do **not** run a SPICE server.
- **No reusable Rust crate for our side.** The SPICE Rust ecosystem is all client-display-protocol:
  `spice-client` (pure-Rust *client*, experimental, GPLv3, explicitly **no** vdagent),
  `spice-client-glib` (spice-gtk bindings), `rust-spice`/`rsspice` (bind the C `spice-server` we're
  trying to avoid). The piece we need — a host-side **vdagent-framing broker** — doesn't exist as a
  crate. Good news: the framing is small and stable (~an 8-byte chunk header + 20-byte message header
  + a handful of message types), so hand-rolling it in a `limina-proto`-style crate is easy and keeps
  us off GPLv3 and off the SPICE server entirely.

**Key tasks (clipboard first, in dependency order):**
1. **libkrun: virtio-serial named multiport device exposing `com.redhat.spice.0`.** The gating
   unknown — spike whether we extend the existing virtio-console device to a named multiport or add a
   virtserial device. Verify a stock guest's udev rule fires and `vdagentd` comes up against it (no
   guest changes).
2. **Host vdagent broker (`limina-vdagent`-ish crate).** De-frame `VDIChunkHeader`/`VDAgentMessage`,
   do capability negotiation (`VD_AGENT_ANNOUNCE_CAPABILITIES`), and implement the **clipboard**
   message set (`VD_AGENT_CLIPBOARD_GRAB`/`_REQUEST`/`_RELEASE`/`_CLIPBOARD`) end-to-end. Bridge it to
   the **same NSPasteboard bridge M5 already owns** (`crates/limina/src/clipboard.rs`) — so the host
   clipboard surface is shared between the SPICE path and the native `limina-agent` path, never two
   competing owners. Loop/echo suppression as in M5.
3. **Then file transfer** (`VD_AGENT_FILE_XFER_START`/`_STATUS`/`_DATA`) as the second feature — the
   host initiates a push (from an AppKit drop target / "Send File…" menu), streams file chunks over
   the vdagent channel, and `vdagentd` writes into the guest user's Downloads. Respect the xfer status
   handshake (guest can decline / cancel). Chunk-size + flow-control caution mirrors M5's large-vsock
   transfer risks. After clipboard is proven.
4. **Arbitration with the native path.** When `limina-agent` (enhanced tier) is present, it wins;
   SPICE is the fallback when only stock `spice-vdagent` is there. Detect granularly/additively (per
   the cross-cutting rule) — a guest may have one, both, or neither.

**libkrun patches:** the virtio-serial named-multiport device (task 1) — the one real patch. Broker +
clipboard bridge are pure limina code.

**Done test:** boot an **unmodified** stock Fedora image (no `install-enhanced.sh`, no `limina-agent`)
under limina; `spice-vdagentd` comes up against `/dev/virtio-ports/com.redhat.spice.0`; copy text in
the host and paste it in the guest and vice-versa. Baseline-tier compatibility floor (L2) stays green.

**Risks / spike first:**
- **Spike #1 (gating):** does exposing a named `com.redhat.spice.0` virtio-serial port actually wake
  stock `vdagentd`, and how invasive is the libkrun device change vs the existing virtio-console?
- **Wayland reality on our mutter — largely de-risked, still worth confirming.** `spice-vdagent`'s
  clipboard path is historically X11-centric, but **GNOME Boxes ships spice-vdagent clipboard on
  GNOME/Wayland today** (see `docs/research/prior-art-gnome-boxes.md`), so the old X11-era worry is
  much reduced. Still confirm it works against *our* guest mutter (which we pin/patch) before building
  the broker. Residual-risk fallback unchanged: if stock vdagent's Wayland clipboard somehow fails on
  our mutter, file transfer — compositor-agnostic — becomes the *primary* SPICE win instead.
- ~~**Confirm the default-install assumption on the actual base image.**~~ **DONE 2026-07-17** — a
  stock F43 boot confirmed `spice-vdagent-0.23.0-1.fc43` present + dormant + udev-triggered on the
  named port (see the load-bearing facts above). The "$0 guest install" case holds.
- **Overlap with M5 is intentional but must not double-own the pasteboard** — one host clipboard
  owner, two possible guest transports.

---

## Milestone 13 — Visibility- & power-aware runtime rendering adaptation

**Status: 📋 planned — high-level design only (2026-07-21).** Builds directly on the wakeup-reduction
work (adaptive vkr relax, virgl 0043; `docs/perf/overhead-inventory.md`) and the host power/present
machinery from M8/M9.

**Goal:** dynamically scale the render/present workload to what is actually needed *right now* —
**throttle hard when the guest's output isn't being seen** (window occluded, on another Space/virtual
desktop, or minimized) and **cap/relax when the host is on battery** / in Low Power Mode — to save
battery, host CPU/GPU, and host wakeups, with **zero perceptible cost when the window is live and on
AC**. Resume must be instantaneous and clean. This is the *rendering* sibling of dynamic memory and
dynamic vCPU hotplug (M6): the same "give back what you're not using" philosophy, on the GPU/present
axis, driven by host-observable context instead of a fixed rate.

**Signals (inputs — observed by the AppKit front-end; policy lives in limina, per the tenet):**
- **Visibility:** `NSWindow.occlusionState` (fully occluded behind other windows / minimized) and
  `isOnActiveSpace`. The "behind other windows", "on an inactive Space/desktop", and "minimized" cases
  all collapse into one **"output not visible"** signal (macOS already marks off-Space windows
  non-visible via occlusion).
- **Display context:** fullscreen vs windowed, which display, multi-display — already tracked
  ([[limina-display-modes]], M8).
- **Host power:** AC vs battery, battery level, Low Power Mode. The IOKit power-source *read* already
  exists (`crates/limina-vmm/src/krun/battery.rs`, `IOPSCopyPowerSourcesInfo`) but is **pull-only**
  today — queried lazily to feed the virtio battery mirror (libkrun 0042, [[limina-battery]]). This
  milestone adds a **notification-driven power listener** (modeled on the existing host-sleep listener
  `crates/limina-vmm/src/power.rs` / `IORegisterForSystemPower`, [[limina-host-sleep-s2idle]]) and
  surfaces AC/battery + Low Power Mode (`NSProcessInfo.isLowPowerModeEnabled`, net-new) as a host signal.
- *(future)* thermal pressure.

**Policy — a small hysteresis state machine mapping signals → a target present/render budget**
(configurable via `vm.toml`, sensible defaults, escape hatch to disable):
- **Visible + AC:** full rate (today's behavior).
- **Not visible** (occluded / off-Space / minimized): **hard throttle** — pause or cap presents to a
  few fps; latency is irrelevant when nothing is seen. The biggest win.
- **On battery (visible):** cap to a target (follow display refresh or a configurable cap), bias the
  vkr relax toward deep-idle sooner; optionally honor Low Power Mode.
- **Battery + not visible:** most aggressive.
- **Hysteresis + cooldown** to avoid oscillation on rapid focus/Space flips (the balloon-thrash
  lessons apply directly — see the balloon oscillation ledger, [[limina-balloon-oscillation]]).

**Mechanism — layered, cheapest first, two-tier-friendly (mechanism in libkrun, policy in limina):**
1. **Host present cap / pause (front-end + worker, stock-safe).** Cap the present rate / stop
   compositing to screen when throttled — purely host-side, works for an **unmodified stock guest**
   (degraded tier). Presents today are event-driven present-on-flush with a 60 Hz `NSTimer` fallback
   (`crates/limina/src/window/`) and **no cap** — that is the host throttle point. A host→worker knob
   rides the established per-worker control-socket seam (the display-resize / balloon sockets in
   `crates/limina-vmm/src/krun/mod.rs`). The s2idle GPU-session **park** (libkrun 0089) is related but
   is *not* a general present-pause toggle — add one. Ships a real throttle with no guest cooperation.
2. **Guest backpressure via fence / present-complete feedback.** The fence-accurate present path
   (`LIMINA_FENCE_PRESENT`) already holds the guest's `RESOURCE_FLUSH` fence until the frame is shown
   and releases it on the `shown <id>` ack → `process_retired_presents` (libkrun `virtio_gpu.rs`).
   **Pacing that release** (delaying the ack / completion) slows the guest's own frame loop → throttles
   **guest** CPU/GPU, not just host compositing. Composes with the adaptive relax (occluded → skip the
   warm plateau → straight to deep-idle → fewer wakeups).
3. **Guest-cooperative throttle (enhanced tier, `limina-agent` + control plane).** Host sends the agent
   a target rate; the agent hints mutter to cap the compositor's frame rate. Deepest guest-side saving;
   **degrades gracefully to (1)/(2)** when the agent/enhanced components aren't present.

**Two-tier:** a stock guest gets host present-cap/pause (mechanism 1) with zero guest components; an
enhanced guest layers on backpressure + agent-driven compositor throttle. Detect granularly/additively
(a guest may have none, some, or all of the enhanced pieces).

**Correctness / interactions:**
- **Instant, clean resume** on becoming visible again — no stale frame, no dropped input; reuse the
  s2idle resume path.
- **Never throttle audio, mic, the control plane, or networking** — rendering only.
- Composes with host sleep/s2idle (M9), snapshot, display resize, and dynamic memory/vCPU (M6) as one
  coherent "idle/occluded power posture."
- Detection must be reliable across Spaces, minimize, fullscreen-on-another-display, and multi-display.

**Key tasks (rough dependency order):**
1. **Front-end signal source:** observe `occlusionState` / `isOnActiveSpace` / minimize + the existing
   power-source listener; collapse to a `(visible, power)` state with hysteresis + a debug log.
2. **Host present cap/pause mechanism** (reuse the s2idle pause/resume) and wire the policy to it —
   ships the stock-tier throttle by itself.
3. **`vm.toml` policy config** (a `[power]`/`[render]` section: enable, occluded-fps, battery-fps
   cap, follow-low-power-mode) + defaults + disable switch.
4. **Guest backpressure** via fence-feedback pacing (mechanism knob) + relax bias on occlusion.
   Also make the **vkr relax aggressiveness itself a `(visible, power)` function** — not just
   occluded→deep-idle. Measured 2026-07-22 (`spikes/wakeup-probe/RESULTS.md`): the shipped 0043
   responsive 40 µs warm plateau costs ~5,900 vkr_ring poll-sleeps/s under a *visible* 60 fps blobs
   workload (host wakeups ~8.1k/s) vs ~1,900/s for the old flat-640 cap (~4.2k/s) — the plateau's
   low latency is worth it for focused+AC game-latency, but a 60 fps-capped / battery / background
   workload would prefer the deeper backoff. Fold that knob into this policy (a control-plane or env
   selector for plateau depth) instead of a static retune.
   **This IS the whole of the former "vkr doorbell-handshake" idea** — the doorbell-handshake spike
   (#42, `spikes/venus-ring-doorbell/RESULTS.md`) found the wakeup-suppression handshake already
   exists and is race-free (host publishes IDLE + parks on cnd_wait; guest notifies only on IDLE,
   seq_cst-race-closed), and that a position-threshold EVENT_IDX buys nothing because the host
   bulk-drains per wake. The 5.9k poll-sleeps are the deliberate pre-park poll window; the only knobs
   to cut them are relax plateau depth + `idle_timeout` (bounded by the guest's coupled 1 ms notify
   rate-limit), both latency-vs-wakeup trades — exactly this `(visible, power)` selector. So there is
   **no separate doorbell mechanism to build; it is this task 4 knob.** `idle_timeout` is a possible
   second dimension of the selector (park sooner when occluded), if the plateau-depth knob alone is
   insufficient — but it requires a coordinated guest notify-rate-limit change and adds vmexits, so
   try plateau depth first.
   **Concrete form (from the #42 flush-cadence probes, 2026-07-22 — see
   `spikes/venus-ring-doorbell/RESULTS.md`): adaptive `warm_rungs` (plateau depth) keyed on recent
   inter-flush history.** Per-ring gap histograms showed the residual poll-sleeps are dominated by
   the *plateau-walk*: every gap reaching the 1 ms `idle_timeout` first burns ~17 poll-sleeps
   walking the full warm plateau before parking — even on a ring that's 100% parkable (mutter:
   2 flushes/frame, ~1.9k poll-sleeps/s of pure plateau-walk). ~3.5–4k of ~7.3k poll-sleeps are
   this walk on gaps ≥1 ms, where early-park is SAFE (guest 1 ms notify rate-limit not tripped) and
   adds zero doorbells. So: shorten the plateau for a ring whose recent gaps are long (sparse),
   keep it full during a tight burst. Est. ~halving of poll-sleeps, host-side only, NO mutter patch
   (direct scanout already quiets the compositor ring 4×), NO guest change. Safety boundary: never
   park-early on a gap you can't be sure is ≥1 ms. The `(visible, power)` signal biases the same
   knob. This subsumes the former "doorbell-handshake" and "plateau retune" — one lever.
   **Mechanism BUILT + PROVEN 2026-07-22 (`spikes/venus-ring-doorbell/vkr-adaptive-plateau-depth.patch`):**
   adaptive `warm_rungs` in `vkr_ring_relax`, env-tunable. Forcing the minimal plateau cut
   clean/overview poll-sleeps ~3× (6.8k→2.2k) at a steady 60 fps — the plateau-walk really is most
   of the budget and coarsening it doesn't stop rendering. **Key finding: the driving signal must be
   THIS task's `(visible, vsync-capped, power)` state, NOT a per-ring gap-history heuristic.** The
   expensive plateau-walk is on the *inter-frame idle gap that follows a burst*, which gap-history
   can't predict (the burst resets any "recently long" counter); and globally shortening the warm
   phase re-hurts vkmark (its ~400–640 µs gaps are what the 640 µs plateau protects — the 0043
   trade). A vsync-capped/occluded/battery ring can always coarsen (the ≤640 µs added latency is
   hidden by the frame budget — proven, held 60 fps); an uncapped submit-latency-bound ring (vkmark)
   must keep the full plateau. So M13 selects `warm_rungs` per ring from the visibility/vsync/power
   state. **Ship guardrail: a vkmark A/B confirming the uncapped path keeps the ~2360 score.** The
   gap-history detector stays in the patch as a safe fallback for never-bursting rings only.
   **2026-07-22: a longer-period PROFILE detector was tried as a standalone win — the vkmark ship
   gate FAILED, so it is NOT shippable as-is and M13 remains the path.** Idea: classify the *regime*
   over a ~100 ms window ("has this ring had a long ≥2 ms idle gap recently?" → capped ⇒ coarsen;
   saturated ⇒ full plateau; `vkr_ring_profile_warm_rungs`, env-tunable). The blobs half is real
   (clean-fullscreen poll_sleeps ~5.9k→~1.75k −70%, host ~8.1k→~4.0k −50%, present 60/s). **But a
   clean same-machine A/B (dylib-swap, no build between measurements) shows a −44% vkmark
   regression: pristine 2433/2440/2446 vs adaptive 1374/1362.** The earlier "2289 intact" was a
   2-scene/warm fluke; a cold 3-scene run reproduces ~1370. The tell is `poll_sleeps` during vkmark
   — baseline ~15,200/s (full responsive plateau) vs adaptive ~7,800/s → the classifier **coarsens
   vkmark's ring**, adding ~640 µs pickup latency per submit. **Root cause: the classifier is too
   loose** — vkmark isn't continuously saturated (sporadic ≥2 ms gaps from scene transitions /
   mailbox stalls; parks ~95×/s even at 2440 fps), one gap arms the 100 ms window and coarsening's
   own added latency keeps re-arming it. "Saw one ≥2 ms gap in 100 ms" does not separate a
   *sustained* vsync cap from a *bursty-but-latency-bound* app. vkcube (a true 60 fps cap) coarsened
   correctly and eyeballed SMOOTH — the mechanism is right for its intended target, wrong on
   saturated-but-bursty rings. Open directions: (a) tighten to a duty-cycle / long-idle-*rate*
   signal + re-run both gates; (b) shelve — round-2 (0091+0041) already landed the big wakeup win,
   this was only an increment; (c) ship gated off, enable under this M13 `(visible, occluded,
   power)` policy which already knows the app isn't the focused 3D workload.
   **RESOLVED 2026-07-22 (direction a) — shipped as virgl 0044.** Re-classify by the FRACTION OF
   WALL-CLOCK spent in long (≥2 ms) idle gaps over a sliding window (default 200 ms), coarsen when
   that fraction ≥ 50%. Time-weighting (not gap-count) makes a capped app's one long idle/frame
   dominate (~84%) even with many sub-ms flushes/frame (firefox ~24) while vkmark's sporadic stalls
   stay a tiny fraction. Clean dylib-swap A/B: vkmark 2373/2376/2375 (baseline 2433/2440/2446, ring
   stays responsive — the −44% cliff is gone) and vkcube ~800 poll_sleeps/s (baseline ~2,690, −70%
   — win preserved); robust at coarsen_pct 50–70. Env-tunable LIMINA_RELAX_WARM_MAX/WARM_MIN/
   LONG_IDLE_US/WINDOW_MS/COARSEN_PCT. Non-blocking: eyeball a real capped desktop in coarsen mode
   (vkcube's identical depth eyeballed SMOOTH); confirm defaults scale to 30/120 fps caps.
5. **Enhanced-tier agent throttle:** a control-plane host→guest "target rate" message → `limina-agent`
   → mutter frame-rate hint.

**Done test:** with the window occluded / on another Space, host GPU+CPU and wakeups drop sharply and
recover instantly on focus; on battery with the window live, a measurable framerate cap / wakeup
reduction; a **stock** guest still throttles (host-side) with no guest components; L2 baseline green.

**libkrun patches:** a present pause/cap knob (extends the s2idle path) + a fence-feedback pacing knob;
everything else is limina + control-plane + `limina-agent` code.

**Precursors already in place:** adaptive vkr relax (virgl 0043); the host-sleep listener
(`crates/limina-vmm/src/power.rs`, `IORegisterForSystemPower`) + GPU-session park (libkrun 0089) as the
model for a power listener + quiesce; the IOKit AC/battery read (`krun/battery.rs`) + virtio battery
mirror (libkrun 0042); the fence-accurate present backpressure path (`LIMINA_FENCE_PRESENT`,
`virtio_gpu.rs`); the host→worker control-socket seam (display-resize / balloon) and the host→guest
control plane (`limina-proto` `SHUTDOWN`/`TIME_SYNC`) + `limina-agent` (M5); display modes (M8).

---

## Milestone 14 — Biometric auth: host Touch ID → guest passkeys + fingerprint login

**Status: 🟢 CTAP2 core GREEN end-to-end (2026-07-24, `spikes/touchid-fido/RESULTS.md`). Spikes A
(SEP/Touch ID primitive) + B (uhid↔vsock transport) done, then the real authenticator: hand-rolled
CTAP2 over a Swift CryptoKit SEP shim. On a live F44 enhanced guest, `fido2-cred`/`fido2-assert`
against `/dev/hidraw0` register + assert a passkey with host Touch ID prompts and libfido2
cryptographically verifies both attestation and assertion (ES256, enclave-bound). Two bugs found by
wire-tracing: missing CTAPHID keepalive during the Touch ID wait, and non-canonical getInfo CBOR
(options must be `rk,up,uv,plat`). Remaining = productization (payload/app-bundle delivery of the
SEP dylib + agent, browser/PAM oracles) + the stock-tier xHCI wave.**

**Goal:** the guest uses the Mac's Touch ID as (a) a WebAuthn/passkey authenticator in browsers
(and `sk-*` SSH keys), and (b) fingerprint login/sudo/GDM. **Raw sensor passthrough is impossible
at any privilege level** — the sensor is hardwired to the Secure Enclave; no macOS API or
entitlement (public or grantable) exposes images/templates — so this is an *auth service*, not a
device forwarding.

### Load-bearing decisions (2026-07-24)

1. **Host = a CTAP2 authenticator backed by the Secure Enclave.** `makeCredential` creates a
   SEP P-256 key (ES256 — WebAuthn's mandatory alg); `getAssertion` shows the Touch ID sheet
   (per-RP reason string: "VM 'x' wants to sign in to github.com") and signs via
   `SecKeyCreateSignature`. Key material never exists in the guest; a compromised guest cannot
   sign without a finger on the physical sensor. Credentials are **device-bound** (like a hardware
   key), **namespaced per VM**; attestation = self/none (no cert chain); the restricted
   `com.apple.developer.web-browser.public-key-credential` entitlement is NOT needed (only gates
   iCloud/Apple-Passwords passkeys, which we don't touch).
2. **Spike A facts (all empirical):** LAContext prompt + userPresence-gated SEP ES256 signing work
   from a terminal-launched, Apple-Development-signed CLI with **zero entitlements**. The
   data-protection keychain needs profile-backed entitlements — plain `codesign --entitlements`
   gets the binary **AMFI-SIGKILLed at spawn** (exit 137). Persistence therefore uses **CryptoKit
   `SecureEnclave.P256` `dataRepresentation` blobs** (~284 B, encrypted to this Mac's enclave,
   useless off-machine) stored in per-VM state — no keychain, no provisioning profile, ever.
3. **The CTAP core is transport-agnostic, with two transports:**
   - **Interim (enhanced tier, ship first):** `limina-agent` creates a `/dev/uhid` FIDO HID device
     (usage page 0xF1D0); CTAP HID frames ride a new vsock control-plane channel. Weeks, not
     months; later remains the fallback where USB is off.
   - **FINAL (productized, stock-tier coverage): VMM-emulated xHCI** (`xhci-platform` MMIO — stock
     aarch64 Fedora has the driver built in) presenting **two USB gadget devices**:
     a. an **honest FIDO HID key** — vendor-neutral (browsers/udev detect by usage page, not
        VID/PID); browser passkeys + SSH keys work on an unmodified stock guest, zero config;
     b. an **impersonated, well-supported match-on-chip fingerprint reader** (Route A — decided
        over shipping our own libfprint driver): protocol implemented from the open-source
        libfprint driver of the chosen device (the driver source is the spec), so stock fprintd /
        GNOME Settings / GDM light up natively. **fwupd is neutralized by advertising an
        impossibly high firmware version** so it never offers an update. Match-on-chip, never
        match-on-host: verify = host Touch ID → match/no-match; no synthetic fingerprint images.
        Advertise the minimum enroll-stage count; enrollment stays a manual per-user GNOME
        Settings step (accepted — same UX as macOS itself).
4. **xHCI emulation is shared infrastructure with M7** — real USB passthrough wants a guest-visible
   controller anyway; biometrics adds two device models behind it. Biometrics alone would not
   justify the controller; the combination does.
5. **PAM side on stock:** Fedora's authselect `with-fingerprint` + fprintd take over once a
   "supported reader" exists (verify the F44 default-enabled state during the spike); `pam_u2f`
   becomes optional (FIDO device is then mainly for WebAuthn). `hmac-secret`
   (systemd-cryptenroll) can't live in the SEP — software-key fallback is a separate decision.

**Key tasks:**
1. ✅ **Spike A** — host SEP/LAContext primitive (`spikes/touchid-fido/`).
2. ✅ **Spike B** — agent uhid FIDO device + vsock channel + host CTAPHID; `fido2-token` sees it.
3. ✅ **CTAP2 core** — hand-rolled (ES256-only) over the Swift CryptoKit SEP shim; per-VM store;
   `fido/{ctap2,store}.rs`, `sep.rs`, `swift/fido_sep.swift`. `fido2-cred`/`fido2-assert` verify
   register + assert live with Touch ID. (Chose hand-rolled over `passkey-rs`.)
4. **Productize (next):** wire the store path to the per-VM state dir (currently
   `LIMINA_FIDO_STORE`); deliver the FIDO agent + the SEP-signed supervisor + `liblimina_sep.dylib`
   via the enhanced payload / app-bundle (`build-app.sh` copies the dylib into Frameworks + fixes
   the rpath — dev/test bakes it to OUT_DIR); browser oracle (webauthn.io in guest Firefox).
5. **PAM recipe** + L2 guard (`fido2-assert` round-trip; test-only auto-approve knob for CI since
   the prompt needs a finger).
6. **Stock wave:** xHCI device model + the two gadgets; pick the MOC target from libfprint
   sources (simplest protocol, no pairing/TLS if avoidable); fwupd-neutralization verify.

**Done test:** on a **stock** F44 guest (USB build): Firefox registers + asserts a passkey on
webauthn.io with Touch ID prompts appearing on the host; GNOME Settings shows Fingerprint Login;
enrollment + GDM/sudo fingerprint work via host Touch ID. On the **enhanced** image (uhid build):
the same passkey flow, plus the L2 guard green.

**Risks / spike first:** MOC protocol fidelity (some drivers do pairing/PSK — choose the target
by protocol simplicity); fwupd's actual probe behavior vs the version bluff; enroll-stage UX
(each "touch" = a host prompt); LAContext prompt from all launch modes (terminal proven; Dock
launch to verify — cf. the fd-limit and TCC launch-env traps); multi-VM prompt attribution.

---

## Summary of net-new code vs libkrun patches

| Milestone | Net-new limina code | libkrun (or fw/virgl) patches |
|---|---|---|
| M1 boot ✅ | CLI, internal-API `limina-vmm`, child supervisor, codesign | (optional) harden panic exit paths |
| M2 display+input ✅ | supervisor IOSurface window, native-Rust display backend, input provider, kVK→KEY table | software-2D scanout (0001); hw-cursor queue (0008); Darwin input worker ran as-is |
| M2.5 console/serial ✅ | serial command/getty shell (`l1_command` + stock getty), serial pane in window, boot-console-frame test | PL011 tty (0004 HVF halfword-MMIO + 0005 FDT `arm,primecell`); hvc0 (0003), PL011 WouldBlock (0002); KRUN_EFI EDK2 + VirtioGpuDxe GOP (0006/0022 + PlatformBm.c) |
| M3 networking ✅ (NAT+SSH; bridged deferred) | gvproxy supervision + gateway cleanup; well-known-MAC static lease | none needed (reconnect-on-HANG_UP still optional) |
| M4 3D 🟢 | coexist routing, zero-copy + fence-accurate present path, KK as host driver | coexist (0010), fence-present series (0017–0022), virglrenderer fork, KK perf/XFB, kernel `patches/linux/0001–0003`, mutter ×2; remaining: upstream queue |
| M5 clipboard/fs/agent 🟢 core | guest agent (from L1 vsock seed), NSPasteboard bridge, ext-data-control + RemoteDesktop clipboard clients, virtiofs share + auto-mount, enhanced-tier installer (remaining) | mutter 0003 (ext-data-control); none for transport (vsock+virtiofs exist) |
| M6 dynamic memory ✅ | PSI autoballoon policy + `BalloonControlHandle` / `--memory` / control socket (internal Rust API, not a C ABI) | reclaim fix (MADV_FREE_REUSABLE) + 16 KiB align/coalesce + inflate/deflate handlers + DEFLATE_ON_OOM (0033/0034) |
| M7 USB | host claim/attach, usbip plumbing | our-kernel config edit (USB+uinput); later native virtio-usb + krun_add_usb* |
| M8 audio/x86/polish | fullscreen, keymap, multi-display, pointer capture, IOSurface mach-port scoping, FEX wiring | native virtio-snd; runtime resize/EDID; LED parity |
| M9 suspend/resume + snapshots 📐 designed | host-side VMM snapshot (file format/CRC, `--restore` wiring, device schema + mapped-blob set, named-snapshot manager + clone + APFS `clonefile` disk, agent freeze bracket, proto `Snapshot`/`Restore`/`TimeSet`, capability probe, UX); Mesa-venus object-graph replay + **device-local content readback** + blob copy-back (venus tier) | multi-vCPU HVF pause/quiesce (incl. WFE-parked wakeup) + vCPU save/restore (wrappers, FFI exists) + GIC state (spike #2 green) + `CNTVOFF` set + `--restore` mode + device (de)serialize + virtio freeze/thaw hardening + snapshot-time GPU quiesce (restore = fresh worker, no in-process renderer reset; `reset_session` rutabaga-context fix already shipped, 0035); carry `patches/linux` Dongwon-Kim drm/virtio freeze-restore (virgl) |
| M12 SPICE agent 📋 planned | host vdagent broker (framing + clipboard, then client→guest file transfer), NSPasteboard bridge reuse (M5), native-vs-SPICE arbitration; display-resize deliberately excluded (native EDID already covers it) | virtio-serial named multiport port `com.redhat.spice.0` (wakes stock `spice-vdagentd`); no crate reuse |
| M13 visibility/power render adaptation 📋 planned | front-end occlusion/Space/power signal + hysteresis policy, `vm.toml [power]/[render]` config, host present cap/pause (reuse s2idle), agent frame-rate throttle message | present pause/cap knob (extends s2idle 0089) + fence-feedback pacing knob; relax deep-idle bias on occlusion |
| M14 biometric auth 🟡 spiked | host CTAP2 authenticator (SEP ES256 + LAContext, CryptoKit blob store per VM), agent uhid FIDO bridge + vsock channel, later xHCI + FIDO/MOC-fingerprint gadgets, pam_u2f/authselect recipe | none for uhid transport (vsock exists); stock wave = xHCI controller + gadget device models in libkrun (shared with M7) |

## First three things to spike

All three founding spikes are **RESOLVED** (M6 reclaim now shipped, 2026-06-26): M1 boot path
(EFI+disk, no remount — `spikes/m1-boot`); M2 input worker on Darwin arm64 (builds + wakes); M6
reclaim (`MADV_FREE_REUSABLE` drops `phys_footprint` on an `hv_vm_map`'d region —
`spikes/balloon-madvise`, re-confirmed on the shipping macOS release).

The standing rule remains: spike the gating unknown before building on it.
