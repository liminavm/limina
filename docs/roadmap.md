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

1. **Productize the enhanced tier.** Deliver what's baked into the dev image (16 KiB kernel, mesa
   zink + venus ICD, patched mutter, `environment.d` policy) via the M5 agent/installer **(still
   TODO — the guest side)**; the **host side is ✅ DONE (2026-06-23):** `scripts/build-app.sh`
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
3. **Run the desktop through the GOP console end-to-end.** The GOP+venus singleton is FIXED
   (libkrun 0022 — a persistent gpu worker owns the renderer across device reset; RED-verified by
   `venus_reset`). Still needs: wire GOP firmware as the windowed-boot default (M2.5 Phase 3, done)
   and make the 16 KiB kernel EFI/BLS-bootable so GRUB selects it (M5 productization — today the EFI
   path boots the stock 4 KiB kernel, on which venus is moot).
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
only the productization track (sysext guest-tools + kernel RPM) and follow-ups remain.

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
`l1_share` (read+write round-trip + `:ro` refusal), `l1_liveness`. Live-verified on the seated
desktop both clipboard directions + shares + window-close → orderly GNOME power-off (~1s, exit 0).

### Remaining: productization (delivery design SETTLED) + follow-ups

The agent is the enhanced-tier configurator. Two carriers:
- **Userspace rides a versioned `systemd-sysext` image (+ confext for `/etc`):**
  `limina-guest-tools-<ver>.sysext.raw` carries the agent + units, the patched mutter (overlayfs
  upper shadows the exact path gnome-shell loads — solves not-parallel-installable; `unmerge` =
  instant rollback), mesa zink + venus ICD under a limina-owned dir selected via ICD JSON/env (do
  NOT shadow stock mesa), and `/usr/lib/environment.d/` policy. Additive, reversible, rpmdb-untouched
  — the package-manager-shaped two-tier tenet. **Distro-agnostic** (plain systemd ≥254);
  `systemd-sysupdate`/`importctl` is the update channel.
- **The kernel goes through the distro's own EFI boot machinery, NOT host direct-boot** (that was
  the dev vehicle; BLS/GRUB fallback, kdump, dracut, the M2.5 boot console all assume the guest boots
  itself): package as a kernel RPM installed via `kernel-install` → BLS entry alongside the stock
  kernels (stock = one GRUB choice away = the degradation path). **Host-built primary** (the
  container pipeline already cross-builds it; needs `CONFIG_EFI_STUB` + dracut-managed rootfs),
  in-guest rebuild as the self-hosting fallback. A sysext can't carry the kernel image (the
  bootloader reads `/boot` before userspace; sysext overlays `/usr` only).
- **Bootstrap is a one-time Parallels-style "install guest tools"** (ISO/attached volume): graceful
  degradation makes this acceptable — the stock guest has a working software-GL desktop before
  enrollment; the installer drops the sysext + kernel package, and the agent owns updates thereafter.

**Consequence:** EFI-booting the enhanced tier needs venus to survive the EFI→kernel reset — that
singleton prerequisite is ✅ DONE (libkrun 0022). What remains is making the 16 KiB kernel
BLS-bootable so GRUB selects it.

**Follow-ups:** DAX/shm window (`VirtioShmRegion` in `fs/device.rs`; confirm shm-window alignment +
FUSE_SETUPMAPPING/SHMCAP on 16 KiB host pages; the enhanced tier already runs a 16k guest so
guest-page = host-page is the default there — test stock-4k DAX separately) + host↔guest uid
mapping; a `liminactl status` consumer; images/files/HTML on the clipboard; reuse libkrun's macOS
host→guest time sync (DGRAM vsock port 123, `timesync.rs`) — confirm a guest-side consumer exists.

**libkrun patches:** none for the transport (vsock + virtiofs overlays already exist). Possibly a
small fix if a HANG_UP'd port can't cleanly reconnect without a VM restart (`unix.rs:548-562`).

**Risks / spike first (delivery-design spikes, both cheap):**
- **SELinux labeling of sysext content** — the overlaid mutter/agent files need correct labels at
  image-build time or stock (Enforcing) Fedora refuses them.
- **EFI-boot the 16 KiB kernel end-to-end** — kernel RPM → `kernel-install`/BLS → GRUB → venus
  desktop (verify `CONFIG_EFI_STUB`, dracut on 16k, BLS default with the stock fallback intact).
  This is also where the GOP+venus singleton fix gets exercised.
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

---

## Milestone 7 — USB passthrough

**Status: 🟡 IN PROGRESS.** Build-by-build plan + as-built log: `docs/design/m7-usb-passthrough.md`.
Shipped & verified: **(1) kernel** — `build-test-kernel.sh` enables USB + `USBIP_VHCI_HCD` + class
drivers + `uinput`; **(2) host `limina-usbip` crate** — the full USB/IP wire protocol (byte-exact to
the kernel source) + a `UsbBackend` trait + a hardware-free **CDC-ACM mock** + a libusb (`rusb`)
backend, 17 unit tests; **(3a) L1 test** (`tests/usb.rs`) — the guest-side USB/IP stack is present on
HVF (GREEN). Remaining: **(3b)** the full mock-attach end-to-end over vsock (a device enumerates in
the guest, no hardware — designed, `vhci_hcd` accepts an `AF_VSOCK` fd so no `usbip` userspace tool
or kernel patch is needed); **(4)** real-device passthrough (libusb claim + the Apple-managed,
restricted `com.apple.vm.device-access` gate — `spikes/usb-probe` is the empirical oracle; a
SoloKeys Solo 2 is the candidate free-to-claim target).

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
   - **Fullscreen:** NSWindow `toggleFullScreen:` default; `CGDisplayCapture` exclusive optional.
   - **Keymap remap + customizable keybindings:** host-side kVK_* → KEY_* table; Command/Option swap
     is a table edit (guest owns the layout so dead keys/IME work natively).
   - **System-combo capture (Cmd-Tab/Cmd-Space/Super):** CGEventTap behind an Accessibility/TCC
     toggle; handle `kCGEventTapDisabledByTimeout` + Secure Input gracefully.
   - **Pointer capture (relative mode):** switch the absolute tablet to the relative mouse on
     capture. **Closes the M2 gap:** the host pointer ignores guest `cursormove` positions, so
     guest-initiated warps aren't reflected host-side — capture mode is where that gets reconciled.
   - **Multi-display:** multiplex all displays through the single `krun_set_display_backend` by
     `scanout_id` (up to 16), each to its own NSWindow/CAMetalLayer.
   - **Runtime window-follow resize / EDID hotplug (libkrun patch):** no post-`krun_start_enter`
     entry changes display size today. Add a C call that raises a virtio-gpu config-change interrupt
     + updates `DisplayInfo` (the GET_DISPLAY_INFO/GET_EDID + config-change capability already
     exists — plumbing, not new device work). The #1 display gap.
   - ~~Hardware cursor~~ — done in M2. ~~Zero-copy scanout~~ — done in M4.
   - **Capability-scope the scanout IOSurfaces (security hardening).** The worker exports each guest
     scanout by its **global `IOSurfaceID`**, mapped with `IOSurfaceLookup` (`window.rs`). That
     namespace is machine-wide and not capability-gated: ids are low/sequential/enumerable, so any
     **non-sandboxed same-user** process can brute-force `1..N` and read the guest screen with no
     screen-recording prompt (`iosdump.swift` is a working PoC). Severity is bounded (local-only,
     same-user; the macOS app sandbox already blocks the App-Store case) — productization hardening,
     not an emergency. Fix: export via `IOSurfaceCreateMachPort` and hand the **port right** to the
     window process (`IOSurfaceLookupFromMachPort`), dropping the global namespace. Cost: a small
     **mach rendezvous** to pass the port to the worker (`SCM_RIGHTS` passes fds, not port rights).
   - **CapsLock/NumLock LED parity (libkrun patch):** surface the statusq LED feedback
     (`worker.rs:238-248` no-op).

**libkrun patches:** native virtio-snd (largest); runtime display-reconfigure/EDID-hotplug C call;
optional LED parity.

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

## First three things to spike

All three founding spikes are **RESOLVED** (M6 reclaim now shipped, 2026-06-26): M1 boot path
(EFI+disk, no remount — `spikes/m1-boot`); M2 input worker on Darwin arm64 (builds + wakes); M6
reclaim (`MADV_FREE_REUSABLE` drops `phys_footprint` on an `hv_vm_map`'d region —
`spikes/balloon-madvise`, re-confirmed on the shipping macOS release).

The standing rule remains: spike the gating unknown before building on it.
