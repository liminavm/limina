# limina Roadmap

A milestone-based, bisectable plan for **limina** — a Rust macOS app on top of libkrun
(Hypervisor.framework) to replace Parallels for running Linux guests on Apple Silicon.

Each milestone has: **Goal**, **Key tasks**, **libkrun patches**, **Done test**, **Risks /
spike first**. Milestones are ordered by dependency; ship and tag each one before starting the
next so regressions bisect cleanly.

Grounded in the research under `docs/research/01..11`. All API names verified against the
vendored header `third_party/libkrun/include/libkrun.h` and the Homebrew `libkrun.1.17.4.dylib`
unless noted.

---

## Cross-cutting architecture decisions (apply to all milestones)

These are settled by the research and constrain every milestone:

- **Backend: raw HVF via libkrun, NOT Apple Virtualization.framework.** Vz is closed and forbids
  the custom virtio devices, host-USB passthrough, fine-grained ballooning, and patchable guest
  agents that are limina's differentiators (research 02). Keep libkrun's working HVF backend and
  patch selectively ("Option B").
- **Build our own libkrun** from `third_party/libkrun` with `make GPU=1 INPUT=1 NET=1 BLK=1`
  (add `VHOST_USER=1` only where it compiles — it is `cfg(target_os="linux")`, research 11).
  The brew 1.17.4 bottle works for an initial link spike (gpu/input/display/net symbols confirmed
  via `nm -gU`) but lacks some 1.18 APIs and cannot be patched, so the product builds from source.
- **Dedicated child-process VMM, krunkit-style.** `krun_start_enter` loops `event_manager.run()`
  forever and the whole process is torn down on guest PSCI SYSTEM_OFF/RESET (research 01,
  chain verified: hvf/lib.rs:536-540 -> VcpuExit::Shutdown -> vstate.rs:407-409 ->
  run-loop `self.exit()` -> exit_evt eventfd). The limina GUI must run the VMM in a child process and
  drive it via vsock + the shutdown eventfd. (Optional far-future patch: replace the exit path with
  an `EventManager` break for in-process control — not on the critical path.)
- **Codesigning is a hard gate from M1 on.** The limina host executable (NOT libkrun.dylib) must be
  signed with `com.apple.security.hypervisor` (use `hvf-entitlements.plist`; ad-hoc signing
  suffices, notarization-compatible). Without it `hv_vm_create` returns `Error::VmCreate`.
  `com.apple.vm.networking` (vmnet/bridged) is Apple-gated and deferred — default to user-mode NAT.
- **Page-size reality:** host is 16 KiB pages (Apple Silicon), guest default 4 KiB. This bites the
  balloon (M6) and virtiofs DAX (M5); flagged where relevant.
- **Repo hygiene:** vendor libkrun / libkrunfw / virglrenderer as patched forks under
  `third_party/` with our patches as tracked series so rebases onto upstream stay reviewable.

---

## Testing infrastructure (cross-cutting)

Tests drive the **shipped binaries** the way a user does — `limina` (supervisor) →
`limina-vmm` (worker) → libkrun/HVF — with no shortcuts to libkrun's internal API. The
harness lives in `crates/limina-test` (the [`Guest`] type: boot, await a console marker,
clean teardown that can never leak a live VM). Three layers:

- **L0 — unit.** Pure Rust in each crate (facade `VmSpec → VmResources`, supervisor
  state machine). No HVF; runs anywhere under plain `cargo test`.
- **L1 — fast boot (primary).** Our own tiny direct-boot guest: a static Rust `init`
  (cross-built to `aarch64-unknown-linux-musl` with `rust-lld`) served as the guest root
  over **virtio-fs** (rootfs is just a host dir; no mkfs/image). Boots to userspace and
  powers off cleanly in **~0.3s** (`crates/limina-test/tests/l1_boot.rs`, `guest/limina-init`).
  Implemented via libkrun's `ExternalKernel` (`set_external_kernel`, `KernelFormat::Raw`).
  **Kernel:** our **custom 6.12** Image built by `scripts/build-test-kernel.sh` (config:
  virtio-fs root, vsock, real initramfs, PL011 console); `scripts/build-test-guest.sh`
  uses it when present and falls back to libkrunfw's bundled Image (extracted from the
  dylib) when it isn't — the zero-dependency default. Source is cached as a bare repo so
  rebuilds skip the download; `PAGESIZE=4k|16k` selects the page size (a 16 KiB guest
  validated booting + reporting `pagesize=16384`, relevant to M6's host/guest page menu).
  **vsock agent:** the init now also runs a tiny vsock agent (gated on a
  `limina.agent_port=` cmdline token) so L1 tests make structured host↔guest assertions
  (`tests/l1_vsock.rs`) — the seed of `limina-agent` (D8). Remaining L1: richer agent
  protocol, GPU/virtio-gpu configs, build-artifact (not just source) caching.

  **Linux build environment.** We build the kernel with **Apple `container`** (`brew
  install container` — lightweight Linux VM on Apple Silicon) as a *build tool only*, not
  a deliverable. `build-test-kernel.sh` clones + compiles aarch64 natively inside a
  `fedora:43` container (the host is case-insensitive and has no kernel toolchain) and
  bind-mounts out only the `Image`. `docker/kernel-builder.Containerfile` documents the
  deps. (Note: Apple `container` uses Virtualization.framework — fine for a build tool;
  unrelated to limina's runtime, which deliberately avoids Vz — see architecture D1.)
- **L2 — stock baseline.** The unmodified Fedora `.raw`, opened **read-only**, asserts
  the user-facing chain boots through firmware + bootloader. This is the permanent
  two-tier compatibility floor. (Pristine Fedora has no `console=`, so the kernel goes
  silent after GRUB — userspace/feature assertions are L1's job.) Implemented in
  `crates/limina-test/tests/boot.rs`.

HVF boot tests need codesigning + the entitlement, so they're **gated behind
`LIMINA_HVF_TESTS=1`** (plain `cargo test` skips them) and run via `scripts/test-boot.sh`,
which builds → signs the worker → runs the gate. CI needs a **self-hosted Apple-Silicon
runner** (hosted macOS runners can't do hypervisor); the multi-GB Fedora image is hosted
out-of-repo for L2.

**Rule: fix bugs RED-first.** Every bug fix starts with a failing test that reproduces
it (see CLAUDE.md). L1 is what makes this cheap enough to always do.

---

## Milestone 1 — Boot Fedora-Workstation-43.raw to a serial console

**Status: ✅ done.** Boots the stock distro via EFI to userspace with full dmesg on the serial
console; child-process supervision + codesign shipped (`crates/limina`, `crates/limina-vmm`). The one
caveat is guest-initiated `poweroff` → worker exit 0, which needs the enhanced-tier
power-button/agent path (a stock EFI guest ignores the GPIO power button; baseline uses the
SIGKILL fallback).

**Goal:** `limina run Fedora-Workstation-43.raw` boots the distro's own kernel via EFI and reaches a
login prompt on the host terminal (serial console). No window, no GPU, no real NIC. This is the
smallest end-to-end path that exercises HVF + disk + console + entitlement.

> **The boot path is validated** (`spikes/m1-boot`, link spike against the brew bottle): EDK2 →
> shim/GRUB 2.12 → Fedora `6.19.13-200.fc43.aarch64` → systemd PID 1 → root(btrfs)+/boot(ext4)
> mounted r/w → `getty.target`, with full kernel dmesg captured on serial. Findings below are
> updated from that spike — read `spikes/m1-boot/RESULTS.md` before implementing.

> This stock-distro boot is not just the first milestone — it is the **permanent
> compatibility floor** (see CLAUDE.md "two-tier guarantee"). Every later milestone
> must preserve it: an unmodified Fedora guest on upstream-shaped libkrun must keep
> booting and stay usable, degraded where our enhancements aren't installed. Our
> custom kernel/drivers/agent are the *enhanced* tier layered on top, never a
> precondition for the VM to run.

**Key tasks (concrete):**
1. **Firmware + libkrun.** **Correction (spike):** there is **no** `libkrunfw-efi.dylib` — the EFI
   firmware is **not** linked. `krun_set_firmware(ctx, path)` just `std::fs::read`s a flat EDK2 blob
   into guest RAM (`builder.rs:1568`, `Payload::Firmware`). Use krunkit's blob,
   `/opt/homebrew/Cellar/krunkit/<ver>/share/krunkit/KRUN_EFI.silent.fd` (or build EDK2 ourselves
   later). The brew bottle (1.17.4) is enough to *boot* — it exports every needed symbol — so a
   from-source build is **not** on the M1 critical path. Still build `third_party/libkrun` with
   `make GPU=1 INPUT=1 NET=1 BLK=1` for the product (patches, 1.18 APIs, GPU/input); verify with
   `nm -gU` that it exports `krun_set_firmware`, `krun_add_disk2`, `krun_disable_implicit_console`,
   `krun_add_serial_console_default`.
2. **Minimal `limina-vmm` binary via the INTERNAL Rust API (no C ABI, D2.1).** Depend on the
   vendored `krun-vmm`/`krun-devices`/`krun-polly`/`krun-utils` crates by path; assemble a
   `VmResources` in a `krun/` facade module. **Proven in `spikes/m1-boot-internal`.** Sequence:
   - `let mut vmr = VmResources::default();`
   - `vmr.set_vm_config(&VmConfig { vcpu_count: Some(4), mem_size_mib: Some(4096), .. })`
     (static RAM; dynamic memory is M6).
   - `vmr.set_firmware_config(FirmwareConfig { path })` — boots the distro kernel from the ESP
     (`Payload::Firmware`; reads the flat EDK2 `.fd`, e.g. krunkit's `KRUN_EFI.silent.fd`).
   - `vmr.add_block_device(BlockDeviceConfig { block_id: "root", disk_image_format:
     ImageType::Raw, is_disk_read_only: false, .. })` — confirmed an **MBR** disk presented as
     `vda`: vda1 FAT ESP, vda2 ext4 `/boot` (XBOOTLDR), vda3 btrfs root (`subvol=root`). EFI+disk
     boots the distro's own kernel/initramfs/drivers, no `root_disk_remount`.
   - **Console (spike):** set `vmr.disable_implicit_console = true` and push a
     `SerialConsoleConfig { input_fd, output_fd }` — ours becomes the PL011 EDK2 uses as ConOut
     (the implicit firmware serial is hardcoded output-dropped, `builder.rs:731`, and the
     firmware is silent). `input_fd` must be a kqueue-pollable fd (pipe/FIFO). To see the
     **kernel** dmesg the guest cmdline needs explicit `console=` — libkrun's PL011 is at
     `0x0a001000` → `earlycon=pl011,mmio32,0x0a001000 console=ttyAMA0,115200` (stock Fedora has
     none; serial goes quiet after early boot — full interactive console is the M2 display).
   - **Networking: TSI default** (zero glue); do NOT add a virtio-net device yet (that is M3).
   - `vmm::builder::build_microvm(&vmr, &mut event_manager, shutdown_efd, tx)` then
     `loop { event_manager.run() }` in the child VMM process.
3. **Codesign** the worker with `com.apple.security.hypervisor`. **Done:**
   `crates/limina-vmm/{hvf-entitlements.plist,sign.sh}` (`codesign --entitlements ... -s - --force
   target/<profile>/limina-vmm`). Only the worker (which calls `hv_vm_create`) needs it; the `limina`
   supervisor does not.
4. **Child-process supervision skeleton. Done:** `crates/limina` spawns the signed `limina-vmm` in its
   own process group, forwards a graceful shutdown on SIGINT/SIGTERM (→ worker → libkrun shutdown
   eventfd → guest GPIO power button), escalates to SIGKILL after a grace period, and reports the
   worker's exit as VM-stopped. **Finding:** a *stock* EFI Fedora guest does not honor the GPIO
   power button (KRUN_EFI's ACPI doesn't advertise it), so graceful power-off is an **enhanced-tier**
   feature (our DT / `limina-agent`); the baseline relies on the SIGKILL fallback.

**libkrun patches:** none required for the happy path. Stretch: harden the `panic!` exit paths a
real Fedora boot may hit — `hvf/lib.rs:549` (unknown PSCI), `:595-602` (unknown exit reason),
`:728` (unknown ESR_EL2 EC). Convert panics to logged graceful stops once we see which fire.

**Done test:** From a clean terminal, `limina --firmware <KRUN_EFI.fd> --disk Fedora-Workstation-43.raw
--console <file>` boots the distro: EFI + GRUB + (with `console=ttyAMA0`) the kernel dmesg reach the
serial file, and the guest reaches `getty.target`. Ctrl-C/SIGTERM stops the VM and the supervisor
reports it (clean exit 0 on the enhanced tier; SIGKILL fallback on a stock guest). **Status:**
boot + supervision done (`crates/limina`, `crates/limina-vmm`); guest-initiated `poweroff` → worker
exit 0 awaits the enhanced-tier power-button/agent path.

**Risks / spike first:**
- **Boot path (#1 risk): RESOLVED** by `spikes/m1-boot` — EFI+disk boots to userspace, no
  `root_disk_remount` fallback needed (the distro's own initramfs mounts the btrfs root).
- ~~Confirm `libkrunfw-efi.dylib`~~ — **resolved/wrong:** no such dylib; firmware is a flat `.fd`
  blob read by `krun_set_firmware`. Building EDK2 ourselves is only needed if we want a non-silent /
  patched firmware (e.g. to wire ConIn for interactive GRUB over serial).
- Verify `krun_has_feature(KRUN_FEATURE_BLK)` / `NET` on whichever libkrun we link.

---

## Milestone 2 — Display + input (native Metal window)

**Status: ✅ done.** Real Fedora boots into a native limina window to the GNOME desktop, the host
keyboard + pointer drive it, the present path is flicker-free, and the guest pointer renders as a
crisp hardware cursor. What shipped, and where it differs from the plan below:

- **Display is supervisor-hosted via a shared IOSurface, not an in-process CAMetalLayer.** The
  worker (`limina-vmm --display-window`) runs the native-Rust `krun_display` backend and publishes
  each scanout into a cross-process `IOSurface`; the supervisor (`limina --window`) owns the NSWindow
  and presents it via `CALayer.contents` (the process-topology decision — see the `limina-m2-display`
  note and `spikes/m2-window`). This keeps the AppKit UI in the surviving supervisor, off the VMM
  child. (commits `f6be85e`, `41fe2fa`, `2756830`, `8d346fa`, `fd1d038`)
- **Tier-1 is software-2D — M2 needed *no* M4 GPU machinery.** Our libkrun patch (0001) serves the
  2D scanout straight from host CPU memory and **skips virglrenderer/rutabaga init entirely**, so it
  works on a GL-less host (resolves the "can the gpu device output 2D without VENUS flags?" spike by
  sidestepping the renderer). A headless PNG-capture sink is the test oracle. (`c0c5fc4`, `6e95029`)
- **Input worker builds and runs on macOS arm64** (the top M2 risk — resolved). NSEvent capture →
  virtio-keyboard + absolute pointer, with a kVK→KEY table and device-dependent modifier handling.
  (`d1dfb8f`, `4eac11a`)
- **Stability fixes landed this round:** synchronous software-2D fence retire (GTK4/nautilus hang),
  input modifier desync + main-thread send freeze. (`74c7b9b`, `4eac11a`)
- **Console split out to M2.5.** The interactive console (`--console-pty`, hvc0 + L1 round-trip
  test) started here but is now tracked under M2.5 below.
- **Present path hardened + flicker eliminated.** Rect-limited swizzle into a CPU-side BGRA canvas
  → memcpy into a 3-deep off-screen IOSurface ring (no write-while-composite tear); implicit Core
  Animation actions disabled (`CATransaction`) to kill the per-frame fade. (`3e46c02`, `7cdcde4`,
  `c8f7de3`, `2eaaaab`)
- **Hardware cursor** as a dedicated supervisor overlay sublayer, driven by a now-serviced
  virtio-gpu cursor queue (libkrun patch 0008 + additive `set_cursor`/`move_cursor` ABI). Upstream
  libkrun never implemented the cursor queue, so the guest was compositing its pointer into the
  scanout — the last source of cursor-area flicker. (limina `acf7e1e`, libkrun `0814184`)

**Remaining to formally close M2:** verify the Done-test's *window-close → orderly guest shutdown*
clause (stock guest: SIGKILL fallback; enhanced tier: agent); confirm the mouse-over-menu stall
(diagnosed as a guest-side GNOME hang) is not ours.

**Goal:** A native macOS window shows the guest framebuffer (2D scanout) and a keyboard + pointer
work. Fedora boots to a graphical login (llvmpipe/software GL is fine — 3D is M4).

**Key tasks (original plan, retained for the verified API references):**
1. **Native NSWindow + CAMetalLayer display backend** implementing the verified
   `krun_display_backend` vtable (`libkrun_display.h`): `configure_scanout`, `disable_scanout`,
   `alloc_frame`, `present_frame`. Implemented as the **native Rust** display backend on
   `VmResources.display_backend` (D2.1 — no C `#[repr(C)]` vtable). `alloc_frame` returns a
   shared-storage `MTLBuffer.contents()` pointer with **exactly width*4 bytes-per-row** (libkrun's
   `read_2d_resource` uses a tightly packed `width*BYTES_PER_PIXEL` stride; on M1 UMA this is one
   copy total). `present_frame` publishes; do the actual CAMetalLayer present on the main thread via
   CVDisplayLink/MTKView (thread hop required — vtable calls run on libkrun's single gpu worker
   thread). Format fast-path: values 1/2 (B8G8R8A8/X8) -> `.bgra8Unorm` no swizzle.
   - Pre-boot config: `krun_add_display(ctx, w, h)` (0..15, max 16 displays),
     `krun_display_set_dpi/_physical_size/_refresh_rate` for the generated EDID. HiDPI:
     `contentsScale = backingScaleFactor`, advertise native Retina pixel dims.
   - Mine `examples/krun_gtk_display/src/{display_backend,display_worker,scanout_paintable}.rs` as
     the per-scanout template. Reject GTK/SDL for the product (foreign event loop fights AppKit).
2. **Enable the gpu device for 2D.** `read_2d_resource` goes through `rutabaga.transfer_read`, so
   rutabaga/virgl is in the path even for pure 2D. Determine the minimal
   `krun_set_gpu_options2` flags needed for a plain 2D framebuffer (see spike).
3. **virtio-input via the vtable** `krun_add_input_device(ctx, config_backend, size,
   event_provider_backend, size)`. CRITICAL ABI note: the `void*` args are the Rust `#[repr(C)]`
   `InputConfigBackend` / `InputEventProviderBackend` structs (features+userdata+PhantomData+
   create_fn+vtable, `c_to_rust.rs:188-254`), NOT the header `krun_input_config` structs — use the
   `krun_input` crate's `into_input_config`/`into_input_events` to avoid the layout footgun.
   - Register a keyboard, an **absolute tablet** (EV_ABS ABS_X/Y 0..32767 + INPUT_PROP_DIRECT), and
     a **relative mouse** (EV_REL). Default absolute for desktop; switch to relative on capture.
   - The provider MUST emit explicit `EV_SYN`/`SYN_REPORT` — the worker copies events verbatim with
     no auto-SYN (`worker.rs:175-184`).
   - macOS UI thread -> channel/ring + readable fd (`get_ready_efd`) -> libkrun input worker. Find
     what fd type the worker's epoll-shim wakes on (no `eventfd(2)` on macOS — likely a pipe
     read-end). Window-only NSView events first; host-side kVK_* -> KEY_* table.

**libkrun patches:** likely a Darwin shim fix for the input worker (`utils::epoll` +
`utils::eventfd`) if it does not build/run on macOS arm64 with `--features input` (verify by
building). Possibly surface the statusq LED/key-repeat feedback (currently a no-op,
`worker.rs:238-248`) for CapsLock/NumLock — defer unless needed. No display patch needed for M2.

**Done test:** Fedora boots to GDM in a native limina window; the host keyboard types into the
login field and the pointer moves and clicks. Window close triggers an orderly guest shutdown.

**Risks / spike first:**
- **Does the input worker build/run on macOS arm64?** Build `third_party/libkrun --features input`
  and check the epoll/eventfd shim before designing the provider. This is the top input risk.
- **Can the gpu device output 2D without VENUS/render-server flags?** Spike `krun_set_gpu_options2`
  minimal flags; affects memory footprint and whether M2 needs any M4 machinery.
- Mode-vs-window reconciliation (guest scanout size != window size): letterbox vs scale-blit; how it
  interacts with backingScaleFactor. Latency of the gpu-thread -> main-thread present hop.
- Cursor: UPDATE/MOVE_CURSOR were unimplemented (panic) in upstream — **now serviced** (libkrun
  patch 0008) and presented as a supervisor overlay sublayer, so the guest no longer composites its
  pointer into the scanout.

---

## Milestone 2.5 — Console & serial (full-boot visibility + debug shell)

**Status: 🚧 in progress.** A debuggability *enabler* milestone, inserted before networking
because every later milestone (M3 NAT, M4 3D, M6 balloon) is far easier to diagnose with a real
boot console and an interactive serial shell. Not a new product feature so much as making the boot
chain observable end to end — and finishing the "serial console" promise M1 deferred to "the M2
display."

**Already shipped toward this** (don't redo): hvc0 bidirectional console + the L1 round-trip test
(`l1_console.rs`, libkrun patch 0003 `PortConfig::ConsoleInOut`); the interactive PL011 pty
(`--console-pty`); PL011 drop-on-`WouldBlock` (patch 0002); and **Track A's core: the PL011
serial *tty* now works** — `/dev/ttyAMA0` is a real bidirectional console, proven by `l1_serial.rs`
(patches 0004 HVF halfword-MMIO + 0005 FDT `arm,primecell`). What's left of M2.5 is a real
serial *login/getty* (vs the test's echo mode) and the *visual* boot console (Track B).

**Goal:** (A) an interactive **serial** shell on `/dev/ttyAMA0` (PL011) for both stock and custom
guests — to read logs, kill processes, poke a wedged userspace — and (B) the **full boot process
(EFI → GRUB → early kernel) visible in the limina window**, not just post-DRM frames.

**Key tasks:**

1. **Track A — PL011 serial *tty*. ✅ The "deadlock" is fixed; getty remains.** Exposing
   `/dev/ttyAMA0` needs `arm,primecell` on the FDT serial node
   (`devices/src/fdt/aarch64.rs::create_serial_node`) so the guest's AMBA layer binds `amba-pl011`.
   Adding it *appeared* to deadlock the probe — but re-deriving from a traced boot, the real cause
   was the **HVF MMIO handler `panic!`ing on `len=2`**: the bound driver's first 16-bit `writew`
   (REG_IMSC) killed the vCPU thread, which looked like a guest hang (no PSCI SYSTEM_OFF). The prior
   "IRQs-off spinlock, needs lockdep" diagnosis was inverted — lockdep would have found nothing.
   Fixed by two minimal libkrun patches: **0004** (HVF: handle len=2 halfword writes — the read side
   already did) and **0005** (FDT: `arm,primecell`). The L1 guest now boots fully over
   `console=ttyAMA0` and round-trips input+output through it (`l1_serial.rs`, RED-first verified).
   - **Done:** the tty itself + the `--console`/`--console-input` (and `--console-pty`) input path —
     these were already wired; the len=2 panic was the only blocker.
   - **Remaining:** a real serial **getty/login** on `ttyAMA0` for an actual debug shell (the test
     uses the init's echo mode). For the custom L1 guest, add a getty; for stock Fedora, ensure
     `serial-getty@ttyAMA0` comes up. Then extend the harness toward "type a command, assert output".
   - **Two-tier:** the stock Fedora EFI guest keeps booting (the len=2 fix is global + additive; the
     `arm,primecell` node only affects the direct-kernel path, since the EFI path uses EDK2's own DT).
     L2 `boot` stays green.

2. **Track B — graphical boot console (EDK2 GOP). ✅ Done end-to-end: firmware → GRUB → kernel all
   render on the GOP.** So EFI/GRUB/early-kernel render into the window. We build the `KRUN_EFI` firmware ourselves
   (`scripts/build-krun-efi.sh`, Apple `container`, slp/edk2@krun-support `ArmVirtKrun.dsc`) and add
   `OvmfPkg/VirtioGpuDxe` so the firmware's graphics console paints to our virtio-gpu scanout — which
   the software-2D libkrun patch (0001) already presents. (See `limina-efi-console`: the EFI-path
   cmdline is GRUB-owned and FDT bootargs are ignored, so a *firmware* graphics console — not a
   kernel `console=` — is the lever for the pre-kernel stages.) **Big finding:** the ArmVirtKrun
   firmware *already* wires the whole graphics-console stack (ConSplitter/GraphicsConsole/GOP
   support/FrameBufferBlt) — it's "silent" only because no GOP-*producing* video driver was
   included; adding VirtioGpuDxe is the change. **Done:** Phase 0 (toolchain proven — rebuilt
   firmware boots Fedora end-to-end), Phase 1 (`KRUN_EFI.gop.fd` builds, VirtioGpuDxe loads, the
   firmware sees the virtio-gpu mmio device). **Phase 2 ✅ (the BDS hang is FIXED).** Root cause was
   NOT a virtqueue layout mismatch: VirtioGpuDxe over virtio-mmio marks its control queue ready
   *without* programming QueueNum (reg 0x38), so libkrun's `size`-0 init left `actual_size()`/`pop()`
   ignoring the avail ring (worker kicked, saw no descriptor). Fix = libkrun patch **0006** (snap a
   ready-but-unsized queue to `max_size`, QEMU-compatible). VirtioGpuDxe now produces a **1280×800
   GOP**, GRUB runs, the kernel boots — confirmed both host-side (a frame presents) and from the
   firmware log (`VirtioGpuDriverBindingStart: produced GOP`, `Graphics Console Started`).
   Follow-on **(b) ✅ FIXED (libkrun 0007)**: the guest *kernel*'s re-init of virtio-gpu (EFI→kernel
   hand-off) left the firmware-era worker thread busy-looping on the freed ring, blocking the kernel's
   own frames. The worker now epolls a stop eventfd and the device's `reset()` signals+joins it before
   re-activation. With 0006+0007 the kernel's virtio-gpu driver presents the **live full-color console**
   through software-2D (verified: 157 frames, fbcon render). Follow-on **(a) ✅ FIXED (firmware
   PlatformBm.c patch)**: the *firmware/GRUB* text was never drawn to the GOP. NOT a flush bug —
   VirtioGpuDxe's GOP is Blt-only and flushes correctly per Blt; the cause was that ArmVirt's
   `PlatformBootManagerBeforeConsole` connects only **PCI** displays before populating `ConOut`
   (then `AddOutput`s every GOP handle), so our virtio-**mmio** GOP — produced later, during
   `EfiBootManagerConnectAll` — never entered `ConOut`. Fix: `scripts/build-krun-efi.sh` patches
   PlatformBm.c to `FilterAndProcess(&gVirtioDeviceProtocolGuid, IsVirtioGpu, Connect)` before
   `AddOutput`. Now the **TianoCore logo, BdsDxe messages, and the full GRUB 2.12 menu** render on
   the GOP (verified by capture). Also needed a host-side change: the capture backend (`limina-display`)
   now encodes PNGs on a **background coalescing thread** — the firmware's *synchronous* per-Blt
   `RESOURCE_FLUSH` against a ~20ms PNG-per-frame encoder otherwise stalled the GRUB menu for tens of
   seconds (the real IOSurface window present is cheap; the kernel batches damage so it was never
   affected). With this, a full GOP boot reaches the kernel under capture. **Track B is now
   end-to-end: firmware → GRUB → kernel all render on the GOP.** Remaining: reconcile with the M2
   IOSurface present path and make limina default to the GOP firmware for windowed boots (Phase 3).

3. **Wire both into the harness + window UX.** A serial-console view alongside the display (a
   separate pane/window the user can open), and keep the L1 console round-trip test green as the
   regression oracle; extend it toward "type a command, assert its output" once a shell is present.

**libkrun / firmware patches:** Track A's PL011 fix is shipped (patches 0004 HVF halfword-MMIO +
0005 FDT `arm,primecell`). Track B still needs a `KRUN_EFI` EDK2 rebuild with VirtioGpuDxe + a
graphics console. Both carried as tracked series (`patches/libkrun`, and a firmware build recipe).

**Done test:** (A) the PL011 *tty* round-trips input+output (`l1_serial` ✅); the remaining bar is
`limina ... --console-pty` (or a serial pane) giving an interactive **login shell** on a booted guest
— type `dmesg`/`kill`, see output — on **both** the custom L1 guest and stock Fedora, with the stock
guest still booting unchanged. (B) The limina window shows EFI + GRUB + early-kernel output during
boot, not a black screen until DRM takes over.

**Risks / spike first:**
- ~~The PL011 deadlock may be a non-trivial emulation gap.~~ **Resolved:** it was a one-line HVF
  halfword-MMIO gap (patch 0004), not a kernel spinlock. Lesson logged — the "needs lockdep"
  diagnosis was an inverted finding; re-derive from a traced boot before prescribing the deep dive.
- **EDK2 rebuild friction** (Track B): ArmVirtKrun is built via Fedora rpmbuild; adding VirtioGpuDxe
  + wiring its GOP to libkrun's virtio-gpu may need firmware-side surface plumbing. Spike a minimal
  GOP-renders-to-window proof before committing.
- Keep the **compatibility floor** green throughout (stock Fedora EFI boot) — add a stock-path smoke
  check to the loop so enhanced-tier console work can't silently regress it.

---

## Milestone 3 — Networking (NAT, then bridged)

**Pulled ahead of finishing M4 (2026-06-06):** with the coexist device booting Fedora→GNOME, the
limiting factor for the rest of tier-2 is *guest observability/control* — confirming venus is
selected (`vulkaninfo`), installing Mesa bits, GL→zink config. **SSH into the guest is the right tool
for all of it**, so we do M3 NAT now and return to M4's open items with a real guest shell. (Also
unblocks general guest work far better than a serial getty.)

**Goal:** Real virtio-net NIC with outbound internet and DNS via user-mode NAT; bridged as an
opt-in later sub-step.

**STATUS (2026-06-06): NAT outbound DONE.** `limina --net` spawns + supervises a gvproxy gateway
(`-listen-vfkit unixgram:///abs/socket`) and connects the guest's virtio-net to it via the new
`krun_add_net_unixgram` path (`UnixgramPath(_, vfkit=true)`, `NET_COMPAT_FEATURES`, fixed
locally-administered MAC). No libkrun patch needed. Verified end-to-end against stock Fedora (spike
`spikes/m3-gvproxy` + L2 test `tests/net.rs`): full DHCP Discover→Ack (guest `192.168.127.3`), DNS
resolution, outbound TCP to a real mirror. Gotchas recorded: (a) gvproxy's `-listen-vfkit` URL must
be ABSOLUTE; (b) the guest must reach **userspace** for NetworkManager to DHCP, so net tests boot a
**writable** APFS COW clone (a read-only root never reaches NM). Oracle is host-side gvproxy `-debug`
(`--net-log`) since pristine Fedora is silent on serial after GRUB. Supervisor tears the gateway down
on both exit paths (headless Drop; windowed `gateway::cleanup()` before `process::exit`).

**REMAINING for the SSH goal (next): inbound port-forwarding.** Outbound works, but the guest sits
behind NAT — to `ssh` in we need gvproxy port-forwarding (host `127.0.0.1:2222` → guest
`192.168.127.3:22`) via its REST control endpoint (`-listen unix://api.sock` + `services/forwarder/
expose`), NOT `krun_set_port_map` (TSI-only, EINVALs once a net device exists). Then guest-side:
confirm the Fedora image has a user account + `sshd` enabled (Workstation ships sshd disabled and a
fresh image hasn't run initial-setup) — may need GNOME initial-setup or a console step first.

**Key tasks:**
1. **NAT default via gvproxy** (Homebrew-installed, userspace, no root/entitlement, built-in
   DHCP/DNS + REST port-forward). Use the NEW API directly:
   `krun_add_net_unixgram(c_path, -1, mac, features, NET_FLAG_VFKIT)` — not krunkit's legacy
   `set_gvproxy_path`/`set_net_mac` — so MAC and TSO features pass in one call.
   - Start offloads at `NET_COMPAT_FEATURES` (IPv4-only: CSUM + GUEST/HOST_TSO4 + UFO). Evaluate
     adding `GUEST_TSO6|HOST_TSO6` (reachable only via the new API) once verified non-corrupting.
   - Remember `krun_set_port_map` is TSI-only and EINVALs once a net device is added
     (`net_index != 0`) — configure virtio-net port-forwarding on the gateway (gvproxy REST), not
     `set_port_map`. The guest-side DHCP client is gated by `KRUN_DHCP=1`.
   - **Supervise the gateway process.** libkrun's virtio-net worker logs FATAL and permanently
     disables the NIC on backend HANG_UP with no auto-reconnect (`worker.rs:146`). limina must
     recreate the net path on crash (or patch `worker.rs` to reconnect — see patches).
2. **Bridged (opt-in sub-step):** virtio-net + Apple `vmnet.framework` BRIDGED mode via a privileged
   helper (`vmnet-helper`+unixgram/vfkit OR `socket_vmnet`+unixstream) + the Apple-gated
   `com.apple.vm.networking` entitlement (or a separately-signed root helper). Determine which vmnet
   helper is actually installed.

**libkrun patches:** optional `worker.rs` reconnect-on-HANG_UP patch so a gvproxy restart does not
require a full VM restart. Otherwise none for NAT.

**Done test:** In the guest, `nmcli`/DHCP obtains a lease, `curl https://example.com` succeeds,
and `ping`/DNS resolve (within TSI/gvproxy limits). For bridged: guest gets an IP on the LAN
subnet reachable from another host.

**Risks / spike first:**
- Confirm the installed gvproxy speaks the `VFKT` dgram handshake `unixgram.rs` sends (spike: boot,
  get a lease, curl outbound). Confirm the linked libkrun has the `net` feature built in.
- Does GUEST_TSO6/HOST_TSO6 improve iperf3 without corruption per backend?
- Does vmnet BRIDGED work over en0 Wi-Fi on M1 Max or only wired/USB Ethernet?
- Verify the macOS datagram size limit (`SndBuf = MAX_BUFFER_SIZE - VNET_HDR_LEN`) vs large GSO
  frames. Enumerate TSI gaps (ICMP, mDNS/Bonjour, VPNs, multicast) for product expectations.

---

## Milestone 4 — 3D acceleration (Venus) — a.k.a. "tier-2"

**Goal:** Hardware-accelerated 3D in the guest: GNOME runs on real GPU, GL apps work via Mesa zink.

**Venus-viability spike done (2026-06-06, `spikes/venus-viability`)** — it corrects the flag
guidance below and establishes the architecture. Two findings gate everything:
- **macOS venus flag set is `VENUS | NO_VIRGL` (0xC0)**, optionally `| THREAD_SYNC | ASYNC_FENCE_CB`
  (0x1C2) — **NOT** the in-tree Linux gui_vm `0x343`. Confirmed by sweep + crash report: `USE_EGL`
  (0x1) must be **off** (no EGL on macOS → `virgl_renderer_init` returns -1); `NO_VIRGL` (0x80) must
  be **on** (else virglrenderer runs the GL path `vrend_renderer_init → create_gl_context` which
  **SIGSEGVs** — no GL context on Apple Silicon). `RENDER_SERVER` is unavailable (no
  `virgl_render_server` binary) but harmless unused — venus runs in-process against MoltenVK.
- **Tier-2 is a "coexist" device, not a flag flip.** A `VENUS|NO_VIRGL` rutabaga serves only Vulkan
  3D contexts; it does **not** implement the 2D commands (`RESOURCE_CREATE_2D`/`TRANSFER_TO_HOST_2D`/
  `SET_SCANOUT`/backing) that the firmware GOP, efifb, fbcon, and the scanout *present* all use. Our
  software-2D patch (0001) serves exactly those, but is today **mutually exclusive** with the
  renderer (`software_2d=true ⟹ rutabaga=None`, `virtio_gpu.rs:389`). So booting Fedora with venus
  flags wedges (firmware 2D cmd → ERR_UNSPEC → `ASSERT Gop.c(109)`). **The foundational M4 patch is
  to make software-2D 2D handling and a `VENUS|NO_VIRGL` rutabaga live in one device, routed by
  command/resource type** (2D → software-2D CPU path; 3D ctx/submit/capset → rutabaga/venus). The
  scanout present stays the software-2D CPU path initially; zero-copy present (task 4) layers on top.

**Coexist device IMPLEMENTED (2026-06-06, libkrun patch 0010) — Phase 1 partially proven.** Fedora
boots to the **full GNOME desktop with venus initialized** on the **silent** firmware
(`LIMINA_VIRGL_FLAGS=0xC0`); the captured frame is the live rendered desktop, the guest negotiates 3D
(CTX commands), and the full boot suite stays green (software-2D floor unaffected). The fix was
smaller than expected: switch #1 (decouple) was already done by patch 0001 (`resource_create_2d`
always uses sw2d); the real blocker was **fence routing** (Global-ring 2D fences were sent to the
venus rutabaga, which rejects ctx-0 → the firmware wedge). Patch 0010 routes fences by ring +
degrades gracefully to software-2D on renderer-init failure (no panic). Design:
`docs/design/tier2-coexist-gpu.md`; venus orientation: memory `limina-tier2-venus`.

**Open M4 items (deferred, in priority order):**
1. **Productize:** make coexist the default (graceful degrade makes opt-in pointless) + a
   `--gpu-software-2d` override (capture oracle, local-Terminal GPU-init hang) + split the test
   harness (software-2D for the fast floor tests, a coexist test). Fix `num_capsets` (hardcoded 5 →
   venus's actual count) for clean capset enumeration. The non-fatal `CTX_DETACH_RESOURCE` (0x203 →
   ERR_UNSPEC) dmesg error.
2. **GOP-firmware + venus (the real blocker):** `virgl_renderer_init` is a process-global singleton,
   but patch 0007 stops+re-activates the gpu worker on the EFI→kernel reset → second init hits
   `AlreadyInUse`. So GOP graphical boot console (Track B) and venus don't yet coexist (silent
   firmware avoids it but loses the boot console; GOP firmware degrades the desktop to software-2D).
   Fix = persist the Rutabaga across the worker restart (hoist to the `Gpu` device; design around the
   fence-handler's per-activation queue/interrupt binding) or rework 0007. Currently graceful (no
   panic, degrades).
3. ~~Confirm venus is actually selected~~ **ANSWERED (2026-06-06, via M3 SSH): venus 3D is BROKEN —
   not selected.** SSH'd into the guest (M3 done) and read the worker GPU debug under the default
   coexist GPU. The guest kernel sees venus (capset id 4, size 156; `+context_init`), but **every
   `CtxCreate` fails `ErrRutabaga(ComponentError(22))` (EINVAL)** → no venus context ever exists →
   the desktop runs on software (llvmpipe/lavapipe). Phase 1's "venus inits / negotiates 3D" was
   over-optimistic (host `virgl_renderer_init` ok; guest per-context create fails). Worse,
   `ResourceMapBlob → ErrUnspec` + host `Error removing/adding memory map` — venus blob mapping
   destabilizes the guest (SSH dropped mid-run; the software-2D path is rock-solid). **Prime suspect:
   capset advertisement (item 1's `num_capsets` hardcoded 5 + capset table not derived from the real
   venus rutabaga) → the guest's venus context-create params mismatch → EINVAL. This makes item 1's
   capset fix LOAD-BEARING for venus, not cosmetic.** Re-evaluate keeping coexist as the *default*
   until venus works (graceful degrade covers init failure, not these per-context EINVALs + blob-map
   instability). Details: memory `limina-tier2-venus`. (This finding is exactly why M3 was pulled
   ahead.)

**Key tasks:**
1. **Coexist device: software-2D (2D + present) + `VENUS|NO_VIRGL` rutabaga (3D) in one virtio-gpu.**
   Build libkrun `--features gpu` (already on). Patch the gpu device so `software_2d` no longer means
   "rutabaga = None" — instead always create a `VENUS|NO_VIRGL` rutabaga for 3D context commands
   while routing 2D resource/scanout commands through the software-2D CPU path. Also make the init
   *fallback* land on software-2D (today it falls back to a `NO_VIRGL`-only rutabaga that still can't
   do 2D). Advertise the right feature bits so the guest negotiates 3D/venus (capsets) without losing
   the 2D scanout. **Coexist is the DEFAULT** (not opt-in): venus-init failure degrades gracefully to
   software-2D, so the default path tries venus and you get 3D when it works. Keep a
   `--gpu-software-2d` override for the capture test oracle and the local-Terminal GPU-init hang
   (graceful degradation catches venus *failure*, not the launch-context *hang*). Host virgl **GL**
   is a dead end on Apple Silicon — desktop GL apps go through in-guest Mesa **zink** (GL->VK->Venus).
   Full design: `docs/design/tier2-coexist-gpu.md`.
2. **Verify/patch virglrenderer Apple blob support.** Confirm our virglrenderer carries the Apple
   blob patches (`RUTABAGA_MEM_HANDLE_TYPE_APPLE = 0x0006`, `VIRGL_RENDERER_BLOB_FD_TYPE_APPLE`,
   `virgl_renderer_resource_get_map_ptr`). If Homebrew's lacks them, build the libkrun-flavored fork
   under `third_party/virglrenderer`. macOS MAP_BLOB asks the VMM thread to `hv_vm_map` the host GPU
   pointer into the guest (host-visible/DAX); without these patches Venus host-visible memory breaks.
3. **GPU SHM vRAM window.** Keep the 8 GiB-default guest-physical SHM window (lazy `hv_vm_map`, not
   committed RAM) via `krun_set_gpu_options2`; reconcile its GPA layout with M6 ballooning.
   NB: bulk 3D buffers are already **zero-copy/shared** here (guest reads/writes the renderer's
   own pages via `hv_vm_map`); only the *scanout present* still copies — see task 4.
4. **Zero-copy scanout present (`SET_SCANOUT_BLOB`) — second M4 deliverable.** The M2 present path
   copies the whole framebuffer out of GPU memory every flush (`read_2d_resource` →
   `transfer_read`, `virtio_gpu.rs:491-515`; full-frame, tightly-packed `width*4`); on M1 UMA that
   is one shared-buffer copy, fine for desktop UI but a real cost for full-screen 3D/video at
   Retina. `SET_SCANOUT_BLOB` currently `panic!`s (`worker.rs:334`). Patch libkrun to: (a) accept
   scanout-from-blob; (b) export the scanout blob as an IOSurface/Metal-texture handle (depends on
   the same virglrenderer Apple-blob support as task 2); (c) add a display-vtable **feature bit +
   `present_texture(scanout_id, surface)` callback** (joint change to `libkrun_display.h`, shared
   with doc 09 option C) so the limina backend wraps it straight into a `CAMetalLayer` drawable with
   no readback. This also removes the `read_2d_resource` `.unwrap()` panic on the present path.
   Spike IOSurface↔virglrenderer/MoltenVK interop *before* committing the ABI (see risks).

**libkrun patches:** (1) a virglrenderer fork build (Apple blob patches), shared by tasks 2 & 4;
(2) the `SET_SCANOUT_BLOB` accept path + a new display-vtable surface-export callback for task 4.
Tasks 1–3 land first (Venus + bulk zero-copy); task 4 layers on the M2 CPU-pull present.

**Done test:** (a) In the guest, `glxinfo`/`vulkaninfo` reports the virtio-gpu/Venus renderer (not
llvmpipe), `glmark2` runs on the GPU, and GNOME Shell animations are smooth in the limina window.
(b) With task 4 landed, a full-screen `glmark2`/video shows **no per-frame `read_2d_resource`
readback** in a libkrun trace and present is driven from an IOSurface-backed texture; frame pacing
holds at the display refresh at Retina resolution.

**Risks / spike first:**
- ~~**Spike #1: does Homebrew virglrenderer carry the Apple blob patches + does venus init?**~~
  **Partly answered (`spikes/venus-viability`):** the `slp/krun` bottle carries the Apple-blob API
  (`virgl_renderer_resource_get_map_ptr`, `VIRGL_RENDERER_BLOB_FD_TYPE_APPLE`) and venus is compiled
  in (links MoltenVK); venus **initializes** with `VENUS|NO_VIRGL`. MAP_BLOB host-visible memory at
  runtime is still unproven (no 3D context has run yet).
- **Next spike (guest-side, now the gating unknown): does Fedora 43 Mesa select venus + accelerate?**
  Can't be tested until the coexist device (task 1) exists, since the guest only sees venus once the
  host offers the capset. Cheap pre-check: is the Mesa venus driver (`libvulkan_virtio.so`) even
  present in the `.raw`? If not, it's a guest-side enhanced-tier component to install. Then: does
  zink-on-venus accelerate GNOME/Firefox or fall back to llvmpipe?
- **Spike before committing task 4's ABI:** IOSurface ↔ virglrenderer/MoltenVK interop — can a
  scanout blob be exported as an IOSurface-backed `MTLTexture` the renderer writes into directly?
  If not, task 4's `present_texture` design changes shape. Measure the M2 per-frame
  `read_2d_resource` cost at Retina first to confirm how urgently task 4 is needed (the public 77%
  figure is compute-only and says nothing about present cost).
- MoltenVK feature coverage vs what zink/venus request (tessellation, hostImageCopy, extensions).

---

## Milestone 5 — Clipboard + virtiofs file sharing + guest agent

**Goal:** Bidirectional text clipboard, a host folder shared into the guest, and a versioned
control channel between limina and a guest agent.

**Key tasks:**
1. **Guest agent + vsock control plane.** One multiplexed control connection over a single
   `krun_add_vsock_port(ctx, LIMINA_CTRL_PORT, /run/limina/<vmid>/ctrl.sock)` with the **guest
   connecting out** (default; no listen flag) — mirrors the verified
   `tests/test_cases/src/test_vsock_guest_connect.rs` flow (guest `connect(CID_HOST=2)` <-> host
   `UnixListener`). Coexists with TSI with NO libkrun patch (`device.rs:38-47` keeps
   `unix_ipc_port_map` separate from `tsi_flags`). Wire protocol: 16-byte FrameHeader (magic 'LIMINA',
   version, type, flags, channel, length) + CBOR payloads; HELLO/WELCOME cap negotiation; unknown
   types -> ERROR(UNSUPPORTED), never fatal. First messages: HELLO/WELCOME/HEARTBEAT +
   SHUTDOWN/SHUTDOWN_ACK, with `krun_get_shutdown_eventfd` (verified host->guest orderly shutdown)
   as the forcing fallback.
   - **Agent delivery via virtiofs overlay** (`krun_fs_add_overlay_file`/`krun_fs_add_overlay_dir`)
     + a minimal per-user systemd unit, keeping the user's `.raw` untouched.
   - Reuse libkrun's existing macOS host->guest time sync (DGRAM vsock port 123, `timesync.rs`)
     instead of a custom TIME_SET — just confirm a guest-side consumer exists.
2. **virtiofs file sharing.** `krun_add_virtiofs3` with a DAX/shm window (`VirtioShmRegion` in
   `fs/device.rs`; macOS-capable, not Linux-gated). Confirm shm-window alignment and
   FUSE_SETUPMAPPING/SHMCAP on 16 KiB host pages and that `mount -o dax` works.
3. **Clipboard bridge.** limina-agent (guest) <-> liminad (host) over a full-duplex vsock connection.
   Host side: NSPasteboard `changeCount` polling (no macOS push notification) + static MIME<->UTI
   mapping + promised/lazy data provider. App protocol: length-prefixed binary frames
   (HELLO/OFFER/REQUEST/DATA_HDR/DATA/CLEAR/PING) with monotonic serials + 32-64 KiB chunking on
   vsock credit flow control. Loop-prevention: ignore writes the bridge originated.
   - **M5 = text-only** (optionally shell out to `wl-clipboard`/`xclip` to de-risk Wayland). Native
     libwayland data-control + images is a follow-up; files/primary-selection/HTML deferred.

**libkrun patches:** none for the transport (vsock + virtiofs overlays already exist). Possibly a
small fix if the guest cannot cleanly reconnect a HANG_UP'd port without a VM restart
(`unix.rs:548-562`).

**Done test:** A host folder appears mounted in the guest with read/write; copying text in the guest
pastes on the macOS host and vice-versa; `liminactl status` shows the agent HELLO/WELCOME handshake and
heartbeats.

**Risks / spike first:**
- **#1 blocker: does GNOME/Mutter on Fedora 43 Wayland implement `wlr-data-control-unstable-v1` /
  `ext-data-control-v1`** so an unfocused agent can read/set the clipboard? Spike this before
  building the native bridge; the `wl-clipboard` shell-out is the de-risk fallback.
- Can the NSPasteboard promised-data provider block long enough to round-trip a guest REQUEST/DATA
  without AppKit timing out the paste?
- virtiofs DAX alignment on 16 KiB host pages.
- Validate large chunked vsock transfers respect credit flow control without stalling muxer threads;
  set a max-size cap / temp-file staging.

---

## Milestone 6 — Dynamic memory (balloon, min..max)

**Goal:** The VM is given a `min..max` RAM range; it takes memory under guest pressure and returns
it to macOS when idle, with `phys_footprint` actually dropping.

**Key tasks (in order — the first is cheap and makes the existing path actually work):**
1. **Fix free-page-reporting reclaim.** Replace `libc::MADV_DONTNEED` at balloon `device.rs:100`
   with macOS `MADV_FREE_REUSABLE`/`MADV_FREE_REUSE` so `phys_footprint` drops — **spike-confirmed
   required** (`spikes/balloon-madvise`: DONTNEED returns nothing, REUSABLE reclaims fully even while
   `hv_vm_map`'d). Then enforce 16 KiB host-page alignment/coalescing in `process_frq` for the
   4K-guest/16K-host mismatch (option (a) of the page-size menu — see Risks below and doc 08 §1.2).
   This alone makes the one already-working path (free-page reporting) effective.
2. **Implement the stubbed inflate/deflate handlers** (`event_handler.rs:14-40`, currently log
   "unsupported" and drain the eventfd) and set `num_pages`/`actual` + a config-change interrupt for
   target-driven shrink toward `min`. Advertise `VIRTIO_BALLOON_F_DEFLATE_ON_OOM` (not currently in
   AVAIL_FEATURES, `device.rs:27-30`) as an OOM safety net before driving inflate.
3. **Add a public balloon C API:** `krun_add_balloon(min, max, flags)` /
   `krun_balloon_set_target` / `krun_balloon_get_actual` / `krun_balloon_get_stats`. libkrun stays
   mechanism-only; the PSI/PID policy lives in limina.
4. **PSI autoballoon agent** in the guest over the existing vsock control channel (M5): report
   `/proc/pressure/*` + MemAvailable; limina drives `set_target` between min and max with hysteresis.
   Requires `CONFIG_PSI=y` + `psi=1` in the guest kernel cmdline.

Phase 0 (already done in M1): static `ram_mib` via `krun_set_vm_config` + demand paging. Do not
block boot on this. Defer **virtio-mem** entirely (does not exist in libkrun; large/risky).

**libkrun patches:** the core of this milestone — reclaim fix, 16 KiB alignment, inflate/deflate
handlers, DEFLATE_ON_OOM feature bit, and the new `krun_*balloon*` C API (none exists today).

**Done test:** Start a VM with `--memory 2G..12G`; run a memory-heavy guest workload and watch
`actual` rise toward max; quit the workload and watch `vmmap`/Activity Monitor show limina's
`phys_footprint` drop back toward 2G as pages are madvised back to macOS.

**Risks / spike first:**
- **Spike #1 — RESOLVED** (`spikes/balloon-madvise`, 2026-05-30): `MADV_FREE_REUSABLE` drops
  `phys_footprint` fully on the HVF-mapped MAP_ANON region with **no** `hv_vm_unmap`/`hv_vm_protect`
  first (`hv_vm_map` does not pin pages); `MADV_DONTNEED` returns nothing, `MADV_FREE` is lazy.
  Ballooning is achievable without deeper HVF surgery. Re-confirm on the shipping macOS version.
- **Now the live unknown — the 4K↔16K page-size mismatch.** Both boot paths use 4 KiB guest pages
  (libkrunfw `linux-6.12.87` + Fedora EFI kernel; verified). Because we own the guest kernel this is
  a menu, not a wall: **(a)** host-side coalesce/align in `process_frq` (M6 default — measure waste);
  **(b)** boot a `CONFIG_ARM64_16K_PAGES` guest kernel for 1:1 reclaim + lower stage-2 TLB pressure
  (custom-kernel track); **(c)** patch `mm/page_reporting.c` for host-page-aware, boundary-aligned
  free-page reporting negotiated via a new virtio-balloon feature bit (upstreamable; pursue if (a)'s
  waste is material); **(d)** virtio-mem (later). See doc 08 §1.2. **Spike: measure how much stock
  Fedora reporting actually returns under (a).**
- Re-touch latency/cost of MADV_FREE_REUSE on deflate for an interactive desktop.
- PSI watermark/hysteresis tuning to avoid balloon thrash (build/browser/IDE workloads).

---

## Milestone 7 — USB passthrough

**Goal:** Pass a host USB device (initially libusb-claimable: FTDI/CP210x, YubiKey-class, etc.) into
the guest.

**Key tasks (USB is entirely net-new; the guest kernel has USB compiled OUT today):**
1. **PREREQUISITE — rebuild the libkrunfw guest kernel with USB.** The stock kernel has
   `# CONFIG_USB_SUPPORT is not set` in EVERY arch profile
   (`config-libkrunfw_aarch64:2151`). Enable `CONFIG_USB_SUPPORT=y`, `CONFIG_USB=y`,
   `CONFIG_USBIP_CORE`, `CONFIG_USBIP_VHCI_HCD`, and needed `CONFIG_USB_*` class drivers. This is a
   config-edit + firmware rebuild via libkrunfw's Makefile, a hard prerequisite for ALL USB work.
   (Note: M1 boots the *distro* kernel via EFI, which already has USB — but the rebuilt libkrunfw
   kernel matters wherever limina uses the bundled kernel, and the vhci/usbip plumbing is what we
   target.)
2. **Start with USB/IP, not a native device.** Guest side is 100% upstream (`vhci_hcd` + `usbip`)
   once USB is enabled. Sequence:
   - **C:** prototype USB/IP over virtio-net/TCP (stock `usbip attach -r` works, no guest patching
     beyond the kernel rebuild).
   - **B:** move the transport to libkrun's existing **virtio-vsock** (no VMM device patch for
     transport).
   - **D (mid-term):** native virtio-usb device in libkrun carrying USB/IP PDUs, exposed via a
     `krun_add_usb*` C API modeled on `krun_add_input_device`'s opaque config_backend+size pattern.
3. **Host device claiming.** v1 targets **libusb-claimable** devices (no Apple driver bound).
   Apple-bound interfaces (mass storage, standard HID, recognized audio) need code-signing with a
   device-access entitlement and/or a DriverKit `.dext` — deferred. Isochronous transfers
   (webcams/audio) out of scope for v1.
4. **Cheap early win (can land before the rest):** serial-via-virtio-console for FTDI/CP210x dev
   boards (`krun_add_virtio_console_multiport` / `krun_add_console_port_inout`), independent of the
   USB stack.

**libkrun patches:** libkrunfw kernel config rebuild (prerequisite); later a native virtio-usb
device + `krun_add_usb*` API (Option D). USB/IP-over-vsock needs no VMM transport patch.

**Done test:** Plug a USB-serial adapter or YubiKey into the Mac; `limina usb attach <id>`; the device
node appears in the guest (`lsusb` shows it, `/dev/ttyUSB0` or the YubiKey enumerates) and works.

**Risks / spike first:**
- Does the larger USB-enabled libkrunfw kernel still boot within libkrun's memory/boot constraints?
- Will `vhci_hcd`'s attach accept an AF_VSOCK fd unmodified, or do we need a userspace vsock->vhci
  bridge / small kernel patch?
- Empirical macOS 26 device-claiming matrix (FTDI, YubiKey, mass-storage, webcam, USB keyboard):
  which libusb claims freely vs which Apple blocks; exact entitlements UTM/krunkit use.
- Throughput of USB3 mass storage over USB/IP-vsock vs just using virtiofs (may make storage
  passthrough unnecessary).

---

## Milestone 8 — Audio + x86 emulation + polish

**Goal:** Sound output/input, x86 binary support, and the desktop polish that makes limina a Parallels
replacement: fullscreen, keymap remap, multi-display, system-combo capture, hardware cursor.

**Key tasks:**
1. **Native virtio-snd driving CoreAudio.** Implement a NATIVE in-VMM virtio-snd device in libkrun
   (fill the empty `snd` feature, use reserved id `KRUN_VIRTIO_DEVICE_SND=25`), modeled on the
   in-tree rng/console devices that already work on macOS. Do NOT rely on vhost-user-snd: the entire
   vhost-user path is `cfg(target_os="linux")` + memfd-dependent
   (`builder.rs:976/991/1378/1635/2454`) and vhost-device-sound has no CoreAudio backend. Microphone
   capture adds the rx queue path + macOS mic TCC permission inside the app bundle.
2. **x86 emulation: guest-side, not Rosetta.** Rosetta-for-Linux is bound to
   Virtualization.framework and unavailable to a Hypervisor.framework VMM. Use **FEX-Emu** in the
   guest (primary) + `qemu-user-static` (fallback) via `binfmt_misc` (`CONFIG_BINFMT_MISC=y` — needs
   no libkrun patch; wire via the guest agent / virtiofs overlay).
3. **Polish (each is largely a host-side limina feature unless noted):**
   - **Fullscreen:** NSWindow `toggleFullScreen:` (Spaces, respects the notch) default;
     `CGDisplayCapture` exclusive optional.
   - **Keymap remap + customizable keybindings:** host-side kVK_* -> KEY_* table; Command/Option
     swap is a table edit (guest owns the keyboard layout so dead keys/IME work natively).
   - **System-combo capture (Cmd-Tab/Cmd-Space/Super):** CGEventTap behind an Accessibility/TCC
     toggle; handle `kCGEventTapDisabledByTimeout` + Secure Input gracefully.
   - **Multi-display:** multiplex all displays through the single `krun_set_display_backend` by
     `scanout_id` (up to 16 displays), mapping each to its own NSWindow/CAMetalLayer.
   - **Runtime window-follow resize / EDID hotplug (libkrun patch):** no post-`krun_start_enter`
     entry point changes display size today. Add a C call that raises a virtio-gpu config-change
     interrupt + updates `DisplayInfo` (the virtio-gpu GET_DISPLAY_INFO/GET_EDID + config-change
     capability already exists — plumbing, not new device work). This is the #1 display gap.
   - **Hardware cursor (libkrun patch):** implement UPDATE/MOVE_CURSOR (currently panic) + a
     cursor-queue display callback.
   - **Zero-copy scanout (libkrun + virglrenderer patch, optional):** implement `SET_SCANOUT_BLOB`
     (currently panics) + an IOSurface/Metal-texture display ABI to drop the per-frame CPU readback.
   - **CapsLock/NumLock LED parity (libkrun patch):** surface the statusq LED feedback
     (`worker.rs:238-248` no-op).

**libkrun patches:** native virtio-snd device (largest); runtime display-reconfigure/EDID-hotplug C
call; hardware-cursor callback; optional SET_SCANOUT_BLOB zero-copy; optional LED parity.

**Done test:** Audio plays from a guest app through the Mac speakers (and mic capture works); an x86
Linux binary runs in the arm64 guest via FEX; the limina window goes fullscreen on a Retina display,
Command/Option are swapped per config, Cmd-Tab is captured by the guest when the toggle is on, a
second display attaches at runtime, and resizing the window reflows the guest resolution.

**Risks / spike first:**
- **Spike #1: native virtio-snd.** Can a minimal `snd/` device (modeled on rng/console) enumerate a
  card in the guest and play a tone through a CoreAudio AudioUnit? Measure round-trip latency vs
  Parallels and CoreAudio buffer/clock matching (period/buffer sizes vs render callback) to avoid
  xruns. Confirm `vmm/snd` and the reserved id are truly inert so the native plan starts clean.
- Confirm FEX `binfmt_misc` auto-wiring works under our HVF launch path (vs needing the agent/image
  to set it up).
- IOSurface <-> virglrenderer/MoltenVK interop feasibility before committing to the zero-copy ABI.
- CGEventTap disable frequency / Secure Input handling in practice.

---

## Summary of net-new code vs libkrun patches

| Milestone | Net-new limina code | libkrun (or fw/virgl) patches |
|---|---|---|
| M1 boot ✅ | CLI, internal-API `limina-vmm` (D2.1), child supervisor, codesign | (optional) harden panic exit paths |
| M2 display+input ✅ | supervisor IOSurface window, native-Rust display backend, input provider, kVK->KEY table | software-2D scanout (0001); Darwin input worker ran as-is |
| M2.5 console/serial 🚧 | serial getty/login; serial pane in window; console harness | Track A PL011 tty ✅ (0004 HVF halfword-MMIO + 0005 FDT `arm,primecell`, `l1_serial`); remaining: serial getty; KRUN_EFI EDK2 + VirtioGpuDxe GOP (Track B); already: hvc0 ConsoleInOut (0003), PL011 WouldBlock (0002) |
| M3 networking | gvproxy supervision; bridged helper integration | (optional) worker.rs reconnect-on-HANG_UP |
| M4 3D | virgl flags wiring; IOSurface present-texture backend | virglrenderer Apple-blob fork build; SET_SCANOUT_BLOB accept path + display-vtable surface-export callback (zero-copy scanout) |
| M5 clipboard/fs/agent | guest agent, liminad, NSPasteboard bridge | none for transport (vsock+virtiofs exist) |
| M6 dynamic memory | PSI autoballoon policy | reclaim fix (MADV_FREE_REUSABLE — spike-confirmed) + 16KiB align + inflate/deflate + krun_*balloon* API + DEFLATE_ON_OOM |
| M7 USB | host claim/attach, usbip plumbing | libkrunfw kernel rebuild (USB on); later native virtio-usb + krun_add_usb* |
| M8 audio/x86/polish | fullscreen, keymap, multi-display, FEX wiring | native virtio-snd; runtime resize/EDID; hw cursor; LED parity (zero-copy scanout already landed in M4) |

## First three things to spike (highest uncertainty, gate the most)

1. ~~**M1 boot path:** EFI+disk vs root_disk_remount against the real Fedora `.raw` layout.~~
   **RESOLVED** (`spikes/m1-boot`): EFI+disk boots to userspace, no remount needed.
2. ~~**M2 input worker on macOS:** does `--features input`'s epoll/eventfd shim build/wake on Darwin
   arm64?~~ **RESOLVED**: it does; keyboard + absolute pointer drive the live desktop.
3. **M6 reclaim:** does `MADV_FREE_REUSABLE` actually drop `phys_footprint` on an `hv_vm_map`'d
   region? (Decides whether dynamic memory is feasible at all.) — still the key open spike.

New near-term spike (M2.5): the **PL011 amba-probe deadlock** — lockdep the guest kernel to get the
stuck PC before attempting the libkrun PL011 emulation fix (see `limina-pl011-tty-deadlock`).
