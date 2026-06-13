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
  - **Reboot = relaunch the child (done 2026-06-13).** Since the VMM is single-shot, a guest
    *reboot* and *power-off* both exit the worker. libkrun patch 0023 makes `SYSTEM_RESET` exit
    with a distinct `FC_EXIT_CODE_REBOOT` (125), and `supervisor::run` relaunches the worker on it
    (recycling gvproxy, whose vfkit socket is single-connection) instead of treating it as a
    power-off — so a guest reboot restarts the VM in place while the supervisor + its host-side
    resources survive, with a boot-loop cap. Guard: `reboot::guest_reboot_relaunches_the_worker`.
    (Headless path only; the windowed path's worker↔window socketpair re-wiring is a follow-up.)
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

**Trace-replay rendering tests (tier-2 pixels; phase 1 ✅ 2026-06-12).** Rendering
correctness can't be asserted from exit codes or FPS (every historical venus bug — the
all-zero vertex buffer, #32 stencil clip, the u8 sentinel — passed those); the replay
tests mechanize the pixel-verify discipline. Design: capture a GL workload **once** with
apitrace in the seated guest, then on every run replay the trace **twice in the same
boot** — zink→venus and llvmpipe — and tolerance-compare snapshot frames. The software
rasterizer is the reference, so there are no stored golden images and intentional
mesa/KK changes never invalidate fixtures. Implemented: `venus_replay`
(`crates/limina-test/tests/venus_replay.rs`) boots the seated dev-enh golden (KK ICD,
autologin Xwayland session — eglretrace is X11-only), guards the backend via
GL_RENDERER (the env-trap / silent-llvmpipe-fallback check, which doubles as the
X11-EGL-crash regression test for mesa patch 0006), replays
`fixtures/traces/glmark2-build.trace` (gitignored, regenerate via
`spikes/trace-replay/capture-replay.sh`), and compares 47 frame pairs host-side
(≤1% pixels off by >8/255; measured: 0). ~2.5 min; SKIPs cleanly without the
machine-local artifacts. **Phase 2 (same day): `venus_vk_replay` — native Vulkan**
(no zink): gfxreconstruct v1.0.4 (not in Fedora; built via
`scripts/build-gfxreconstruct.sh`, `/opt/gfxreconstruct` baked into dev-enh) captures
vkcube on venus; replay venus (strict) vs lavapipe (`--remove-unsupported` — the venus
capture records instance extensions lavapipe lacks). The reference leg's backend is
PROVEN via gfxrecon's `Replay device info` mismatch warning (FPS can't tell — both
legs vsync-cap ~60 through the session WSI); fixture
`fixtures/traces/vkcube.gfxr`, regenerate via
`spikes/trace-replay/capture-replay-vk.sh`. **Phase 3: the perf trend ledger**
(`scripts/perf-ledger.sh` → git-tracked `perf/ledger.csv`; explicitly NOT a pass/fail
gate — VM-on-dev-machine variance; see perf/README.md). **Phase 4: `venus_shell_replay`
— GNOME SHELL ITSELF.** The real seated gnome-shell (mutter compositing) captured via
an `LD_PRELOAD=egltrace.so` systemd-user drop-in (protocol + the two inherent
divergence classes — startup-fade uninitialized textures, unreplayable client dmabuf
imports — documented in `spikes/trace-replay/RESULTS.md`; fixture regenerated by
`capture-replay-shell.sh`). The shell's own render graph (StWidget shaders, the #32
rounded-corner stencil-clip class, text, overview) reproduces **pixel-exact** on venus
vs llvmpipe. All four phases of the plan are SHIPPED. NOT covered by replay: the
present/scanout path (fence-present, zero-copy) — that stays with the seated-desktop +
iosdump oracles.

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
- **Hardware cursor** driven by a now-serviced virtio-gpu cursor queue (libkrun patch 0008 +
  additive `set_cursor`/`move_cursor` ABI). Upstream libkrun never implemented the cursor queue, so
  the guest was compositing its pointer into the scanout — the last source of cursor-area flicker.
  (limina `acf7e1e`, libkrun `0814184`)
- **Pointer redesign (2026-06-12, `c30df2b`) — host cursor adopts the guest shape.** The first
  presentation (guest cursor as an overlay sublayer positioned from `cursormove` round-trips) gave
  a double pointer, a round-trip of lag, and guest-pointer wander while the host pointer was
  outside the window (AppKit delivers MouseMoved to the key window even then, with *screen*
  coordinates when no window is associated). Now Parallels-style: the worker-published cursor
  IOSurface becomes the macOS `NSCursor` worn inside the content view (one pointer, zero lag,
  guest-correct shape; `cursorhide` → transparent cursor), and pointer events are gated to the
  view (presses/drags/releases follow capture semantics via a forwarded-button mask). Guest
  positions are ignored — guest-initiated warps are the known gap, revisit with pointer capture
  (M8). User-verified.

**Remaining to formally close M2:** ~~verify the Done-test's *window-close → orderly guest
shutdown* clause~~ — implemented 2026-06-12 via the M5 control plane (window-close sends the
agent SHUTDOWN, falls back to SIGKILL for stock guests; `l1_shutdown` asserts the orderly
ladder headlessly). M2 is closed; the windowed variant gets an eyeball once a Fedora guest
runs the real agent.

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
   **The GOP+venus singleton blocker is now FIXED** (M4 open item 2; libkrun 0022, 2026-06-13: the
   renderer persists across the EFI→kernel reset instead of being dropped+re-init'd), so Phase 3 is
   unblocked — what remains is the present-path reconcile + flipping the windowed default to GOP.

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
- Keep the **compatibility floor** green throughout (stock Fedora EFI boot). ✅ The EFI boot is now
  checked on **both** channels (2026-06-13):
  - `boot::fedora_stock_image_efi_boots_to_userspace` — silent firmware; asserts the whole chain on
    **serial** (firmware → GRUB → kernel → getty `login:`) plus **sshd**, on a writable COW clone.
  - `boot::fedora_stock_image_efi_renders_to_gop` — our **GOP firmware**; asserts the boot is
    **visually** present in the captured window (richest frame ~70k distinct colors, no dominant —
    firmware/GRUB/kernel/gdm all render; this path used to freeze on the firmware logo behind the
    reboot loop).
- **SELinux relabel + custom-kernel SELinux (resolved 2026-06-13).** EFI-booting a dev image wedged in
  a reboot loop, which looked like a GOP/code regression but was a *guest-image* artifact: our custom
  16k kernel was built from `arm64 defconfig`, which has **no `CONFIG_SECURITY_SELINUX`**, so nothing
  installed through it ever got labeled. The stock Fedora kernel (EFI path) comes up `enforcing`, sees
  the unlabeled tree + a stale `/.autorelabel`, tries to relabel under enforcing, gets denied
  mid-relabel, and reboots forever. Two-part fix:
  1. **Kernel** — `scripts/build-test-kernel.sh` now compiles SELinux in (`CONFIG_SECURITY_SELINUX`
     + `_BOOTPARAM` + Fedora's `CONFIG_LSM`), so the enhanced tier no longer diverges from the distro;
     `selinux=0` still works as the kill switch for the existing direct-kernel test boots.
  2. **Images** — a one-time `scripts/prepare-efi-image.sh` per image: permissive relabel
     (`fixfiles` labels the tree, converges) **+** `console=ttyAMA0` on the GRUB args (serial console
     + getty on the EFI path). The dev + `.test` images are prepared and EFI-boot clean to userspace.
  (Productization will instead build the kernel the distro's way — enforcing — and use the distro
  config as reference.)

---

## Milestone 3 — Networking (NAT, then bridged)

**Pulled ahead of finishing M4 (2026-06-06):** with the coexist device booting Fedora→GNOME, the
limiting factor for the rest of tier-2 is *guest observability/control* — confirming venus is
selected (`vulkaninfo`), installing Mesa bits, GL→zink config. **SSH into the guest is the right tool
for all of it**, so we do M3 NAT now and return to M4's open items with a real guest shell. (Also
unblocks general guest work far better than a serial getty.)

**Goal:** Real virtio-net NIC with outbound internet and DNS via user-mode NAT; bridged as an
opt-in later sub-step.

**STATUS: ✅ NAT done, outbound + inbound SSH (bridged remains the opt-in later sub-step).**

**Outbound (2026-06-06):** `limina --net` spawns + supervises a gvproxy gateway
(`-listen-vfkit unixgram:///abs/socket`) and connects the guest's virtio-net to it via the new
`krun_add_net_unixgram` path (`UnixgramPath(_, vfkit=true)`, `NET_COMPAT_FEATURES`, fixed
locally-administered MAC). No libkrun patch needed. Verified end-to-end against stock Fedora (spike
`spikes/m3-gvproxy` + L2 test `tests/net.rs`): full DHCP Discover→Ack (guest `192.168.127.3`), DNS
resolution, outbound TCP to a real mirror. Gotchas recorded: (a) gvproxy's `-listen-vfkit` URL must
be ABSOLUTE; (b) the guest must reach **userspace** for NetworkManager to DHCP, so net tests boot a
**writable** APFS COW clone (a read-only root never reaches NM). Oracle is host-side gvproxy `-debug`
(`--net-log`) since pristine Fedora is silent on serial after GRUB. Supervisor tears the gateway down
on both exit paths (headless Drop; windowed `gateway::cleanup()` before `process::exit`).

**Inbound SSH (done; simpler than planned — no REST forwarding needed):** gvproxy ships a built-in
default forward `127.0.0.1:2222 → 192.168.127.2:22`, and the guest gets the static `.2` lease when
the NIC uses the **well-known vfkit MAC `5a:94:ef:e4:0c:ee`** (`crates/limina-vmm/src/krun/mod.rs`).
So `ssh -p 2222 user@127.0.0.1` works with zero forwarding configuration; the REST endpoint
(`services/forwarder/expose`) is only needed for *additional* ports later. (`krun_set_port_map`
stays TSI-only — EINVALs once a net device exists.) Guest side is provisioned (user account + sshd
enabled); asserted by `tests/net.rs`. This is the daily-driver guest-access path — see the
`limina-fedora-access` note.

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

> **STATUS 2026-06-12 — M4 substantially DONE (tier-2 GREEN).** The seated GNOME desktop runs on
> venus with **fence-accurate zero-copy presents** (#8 complete: guest kernel fences blob-scanout
> flushes — `patches/linux/0001` — and the host holds them to the true CA latch; enhanced tier =
> `FENCE_PRESENT=1 COPY=0`, 0 anomalies verified; stock-kernel tier keeps `PRESENT_COPY`) and
> cross-context buffer sharing; WebGL2 works (we implemented VK_EXT_transform_feedback in
> KosmicKrisp); the 5000-fish WebGL aquarium runs 60fps vsync-capped @46% GPU — matching
> host-native Firefox. **Mutter direct scanout works** (fullscreen client buffers flip straight to
> the primary plane: `patches/linux/0002+0003` ARGB-on-primary + LINEAR-modifier advertisement,
> libkrun 0021 wedge-proofing; stock kernels keep composition). Host Vulkan driver: **KosmicKrisp
> (KK), now the ONE supported venus backend** (daily driver, `spikes/venus-draw-probe/boot-seated-kk.sh`;
> perf knobs default-ON in KK itself, CTS-validated). **MoltenVK retired as a venus backend
> (2026-06-13):** it SIGSEGV-loops the gnome-shell compositor (the #28 coherency / #32 stencil class
> corrupts the guest upstream of cogl, so it crashes instead of degrading) — verified A/B on the same
> plain image (MVK: greeter crash-loop; KK: clean). Every venus boot path now forces the KK ICD and
> **degrades to software-2D (llvmpipe) when KK is absent — never the loader's MoltenVK default**
> (`scripts/run-venus-window.sh`, the `kosmickrisp_icd()` gate in `crates/limina-test`, `boot-seated-kk.sh`);
> custom MoltenVK builds/patches archived under `spikes/archive/moltenvk/`. Productization follow-up:
> **bundle KK inside `limina.app`** (loader + `libvulkan_kosmickrisp.dylib` + a relative-`library_path`
> ICD JSON under `Contents/`, worker resolves it relative to its own path) and point the loader at
> only that ICD — so MoltenVK isn't even on the loader's search path and can't load. Same-Team-ID
> codesigning satisfies hardened runtime (no `disable-library-validation` needed).
> Host CPU is attributed and lean (10k-fish aquarium: ~1.9 cores = the guest's own Firefox work,
> ~0.9 core GPU stack; KK dirty-tracking round 28 took the rebind leg +81%, ring thread 38% busy).
> Golden enhanced image: `Fedora-Workstation-43.dev-enh.raw` (16k kernel, zink, patched mutter,
> journal-quiet). Converged truth + the open-threads ledger live in memory `limina-tier2-venus`
> (CURRENT STATE section) and `spikes/venus-draw-probe/RESULTS.md` (rounds 13–29). Everything
> below this box is the historical plan, kept for context.
> **Remaining M4-adjacent work (the open ledger):** the upstream patch queue (mesa
> zink+venus / KosmicKrisp / virglrenderer / SPIRV-Cross / mutter ×2 / kernel 0002+0003 /
> Fedora-zink backport ask — MoltenVK dropped from the queue, it's no longer a backend);
> **productize the enhanced tier** — deliver what today is baked into
> the dev image (kernel, mesa bits, mutter fix, environment.d policy) via the M5 agent/installer,
> and **bundle KK inside `limina.app`** as the host Vulkan driver (loader + KK dylib + relative-path
> ICD JSON; loader pointed only at it) so it's the default *and* MoltenVK isn't on the search path
> to load. Note bundling only removes *host-driver absence* as a fallback trigger — software-2D +
> in-guest llvmpipe stays a shipping-tier path for guests that can't drive venus out of the box
> (stock 4k kernel / no venus mesa = the stock-baseline floor); the fallback is guest-capability-
> driven, not host-driver-driven. The virtio-gpu flip-completion gap
> (event-driven KMS clients hang; #8 gave mutter honest pacing but the generic gap remains);
> ~~GOP-firmware + venus singleton~~ **✅ FIXED (libkrun 0022, 2026-06-13 — renderer persists across
> device reset)**; ~~MVK windowed WSI~~ **moot (MVK retired)**;
> #28 residue policy (`VN_PERF=no_*_feedback` via agent vs real fix); KK GPU-side per-draw
> root re-fetch (only if GPU-bound workloads reappear); Firefox MSAA cosmetic thread.

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
1. **Productize:** ~~make coexist the default~~ ✅ (coexist is the default, with the
   `--gpu-software-2d` override shipped). Remaining: pick **KosmicKrisp as the shipped host-driver
   default in limina proper** (today it's selected by `boot-seated-kk.sh` via `VK_ICD_FILENAMES`)
   and move the enhanced-tier guest config out of the dev image (→ M5 agent/installer). Cosmetic:
   `num_capsets` (hardcoded 5) and the non-fatal `CTX_DETACH_RESOURCE` (0x203 → ERR_UNSPEC) dmesg
   error.
2. **GOP-firmware + venus singleton — ✅ the renderer now survives a device reset (libkrun 0022,
   2026-06-13).** `virgl_renderer_init` is a process-global, init-once, thread-bound singleton, and
   patch 0007 dropped+recreated the renderer on the EFI→kernel reset (and any driver rebind/reboot),
   so the second init hit `AlreadyInUse` → the desktop degraded to software-2D. **Fixed by reworking
   0007 (→ libkrun patch 0022): a persistent gpu worker** spawned once for the whole VMM process owns
   the renderer; `activate()`/`reset()` message it to bind/unbind the per-activation transport (the
   long-lived fence handler reaches the current queue/mem/interrupt via a shared cell). RED-first
   verified by the `venus_reset` test (unbind/rebind virtio-gpu → venus still enumerates) and the full
   boot suite (venus, venus_replay ×3) stays green. This unblocks the prerequisite. **Still remaining
   to actually run the desktop through the GOP console (separate, deferred):** wire GOP firmware as the
   windowed-boot default (M2.5 Phase 3), and make the 16 KiB kernel EFI/BLS-bootable so it's the kernel
   GRUB selects (M5 productization — today the EFI path boots the stock 4 KiB kernel, on which venus is
   moot). The singleton was the shared blocker for both.
3. ~~Confirm venus is actually selected~~ **ANSWERED (2026-06-07, via M3 SSH + empirical diagnostics):
   venus context-create WORKS; the blocker is the 16 KiB-host / 4 KiB-guest blob map.** The
   2026-06-06 "venus BROKEN at CtxCreate" conclusion was a **misread, now disproven**:
   - Logged the actual `context_init`/capset at `rutabaga_core.create_context`. The
     `ComponentError(22)/EINVAL` failures were for **capset_id=2 (VIRGL2 = native OpenGL)** — which
     we intentionally reject under `NO_VIRGL` — while the **venus context (capset_id=4) SUCCEEDS**
     (`create_context OK ... context_init=0x4 capset_id=4`). venus is selected and contexts are
     created. So virglrenderer/venus are NOT at fault (a virglrenderer fork is not needed here), and
     the `num_capsets`-hardcoded-5 suspicion is **cosmetic after all**, not load-bearing.
   - The real failure is the **host-visible blob map**: after the venus ctx, RESOURCE_MAP_BLOB →
     `hv_vm_map(map_ptr, guest_addr, resource.size)` returns **HV_BAD_ARGUMENT (0xfae94003)** →
     guest `OUT_OF_HOST_MEMORY` → `vkCreateInstance` fails → degrade to llvmpipe. `hv_vm_map`
     requires host addr, guest addr, AND size to be 16 KiB-multiples. The first venus blob is
     `0x21000` (size%16k≠0); rounding size up host-side fixes that blob, but the **next** blob then
     lands at guest `base+0x21000` (guest%16k≠0) — **the stock 4 KiB guest packs host-visible blobs
     at 4 KiB granularity, and two blobs sharing one 16 KiB host page cannot be mapped
     independently.** No host-only fix exists (the size round-up was tried and reverted: moot on a
     16k guest, harmful on a 4k guest where it overlaps the neighbor). Kept only libkrun patch
     **0011** (log `hv_vm_map` failures with the alignment breakdown — the diagnostic that found
     this). The guest **stays up and degrades cleanly** (the 2026-06-06 "destabilizes the guest"
     was not reproduced).
   - **THE FIX IS GUEST-SIDE (enhanced tier) — ✅ PROVEN 2026-06-07:** a **16 KiB-page guest kernel**
     makes venus blobs 16 KiB-sized AND 16 KiB-spaced → `hv_vm_map` works with zero host changes.
     Built `Image-16k` (`scripts/build-test-kernel.sh PAGESIZE=16k`; added `BTRFS_FS`+`VIRTIO_NET` so
     it direct-boots Fedora's btrfs root, no initramfs) and booted Fedora 43 on it:
     `limina --kernel Image-16k --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw
     selinux=0 console=ttyAMA0" --disk <cow> --net --display-capture`. Result: **GNOME desktop
     renders; `vulkaninfo` enumerates `Virtio-GPU Venus (Apple M1 Max)` (driver venus, Mesa
     26.1.0-devel); 0 `hv_vm_map` failures.** (Smaller alt not needed now: patch the guest
     virtio-gpu blob allocator to 16 KiB-align while keeping 4 KiB pages.) On **stock 4 KiB Fedora**,
     accelerated venus stays unachievable on a 16k host → llvmpipe degraded baseline (two-tier
     guarantee holds); ANGLE-backed virgl GL is the parked accel idea for that tier (above).
     Remaining for the enhanced tier: WSI/swapchain for accelerated *present* (`vkcube` selected
     venus but lacks `VK_KHR_swapchain`), GL→zink routing, and productizing a 16k boot profile +
     kernel delivery. Details: memory `limina-tier2-venus`. (This is exactly why M3/SSH was pulled
     ahead.)
   - **DEGRADED-TIER ACCELERATION IDEA (future, not blocking): GL via virgl + ANGLE.** virgl (GL,
     capset VIRGL2) uses a **copy/transfer** memory model (guest provides backing pages, host copies
     via `TRANSFER_TO_HOST`) — **no `hv_vm_map` of host memory into the guest**, so it is immune to
     the 16k/4k blob problem and works on a **stock 4 KiB guest**. We can't use virgl's *native* GL
     path on Apple Silicon (no GL → `vrend` SIGSEGVs, hence `NO_VIRGL`), BUT we could back
     virglrenderer's GL with **ANGLE (GL→Vulkan→MoltenVK→Metal)** to give the degraded tier real
     GPU-accelerated **GL** without venus's host-visible-blob requirement. Cost: heavy translation
     stack (GL→virgl→ANGLE→Vulkan→Metal) — much less efficient than zero-copy venus, and GL-only
     (guest Vulkan apps still fall to llvmpipe). Acceptable for the baseline/degraded case. This is
     a real "own the stack" project (virglrenderer fork + ANGLE integration), parked as a potential
     improvement to the stock-4k degraded tier — the enhanced-tier 16 KiB kernel is the primary path.
   - **VERY-LOW-PRIORITY / unlikely: native-context (vdrm) backed by Metal.** mesa probes a native
     context (the guest's own GPU driver forwarding submissions to a host DRM device for the *same*
     GPU — what the `could not connect vdrm` / `Asahi native context` vkcube lines are) before falling
     back to venus. It assumes a **Linux host with the Apple AGX/Asahi kernel driver**; on macOS there
     is no DRM and Metal won't accept raw AGX command buffers, so a Metal-backed vdrm ≈ reimplementing
     the AGX UAPI on Metal — enormous and fighting Metal's abstraction. Recorded only as an eventual
     curiosity; **venus is our path.** Not a real consideration for accelerated present.

**ZERO-COPY END-TO-END PLAN (2026-06-07) — the strategic frame for the remaining M4 work.**
GL now renders on the M1 Max GPU (#21/#27 done: `zink→venus→vkr 1.3.0→MoltenVK→Metal`, glmark2 18/18,
commit 52fad19) — but that is only the *first* of four data crossings. "End-to-end zero-copy"
decomposes into four, and the desktop milestone needs **B+D**, not C:

| # | Crossing | Status | Copy today? |
|---|----------|--------|-------------|
| A | guest app → GPU (uploads, draws) | ✅ works (#21/#27) | zero-copy (blob map) |
| B | rendered image → macOS display (scanout present) | ⚠️ works via **full-framebuffer CPU readback/frame** | **copy** (`flush_resource`→`read_2d_resource`) |
| C | GPU → guest CPU readback (`glReadPixels`, screenshots, **venus feedback**) | ❌ #28 | black-holed |
| D | mutter can run on venus at all (image *creation*) | ❌ #30 | fails before any copy |

**The load-bearing insight: B and D are the same missing capability, and IOSurface is the macOS
dmabuf.** D fails because mutter wants a *dmabuf-exportable* image for KMS scanout and venus/MoltenVK
can't make one (we strip `external_memory_dma_buf`/`drm_format_modifier`). B is slow because the
scanout resource is an ordinary venus image the host must CPU-read every frame. On macOS the export
currency is **IOSurface**, which MoltenVK *does* speak (`VK_EXT_metal_objects`, `MTLTexture` from
`IOSurface`). So one fix closes both: make venus "exportable images" resolve host-side to an
**IOSurface-backed `MTLTexture`** → mutter's `vkCreateImage(extHandleTypes=…)` succeeds (D), and
`SET_SCANOUT_BLOB` references that IOSurface for libkrun to hand straight to `CALayer.contents` /
`CAMetalLayer` with no readback (B becomes zero-copy). This is task 4 below, now also the **#30 unblock**.
We own every layer the seam crosses — venus/vkr, MoltenVK (open), libkrun, the present path.

**#28 (crossing C) is separate and narrower — coherency, not copy — and below B/D for the desktop.**
Proven: host CPU IS coherent with GPU writes (`mtl-shm-coherency`); the guest `hv_vm_map` view reads
stale; guest-side `dc ivac/civac/cvac` (PAN cleared) does NOT reveal the write (`coherency-civac-mod`)
→ **guest-invalidate-alone is dead** (GPU write lives in the Apple SLC, inside the host CPU's domain
but beyond the guest mapping's PoC). C only matters for guest-side readback (glReadPixels/screenshots/
WSI/feedback), NOT the host-side desktop present. ⭐ **ZERO-COPY ONLY — the memcpy/transfer model is
REJECTED.** Remaining zero-copy candidates for C: (b) host clean-to-PoC + guest invalidate (cheapest,
untested — extend `coherency-civac-mod` with a HOST `dc cvac` before the guest invalidate); (d2) back
host-visible blobs from within the guest-RAM-coherent region instead of the shm window above RAM;
(d1) HVF stage-2 cache/shareability attrs on `hv_vm_map` (RWX-only today — needs an HVF capability).

**What we need to LEARN, ranked by leverage:**
1. **(gates everything) MoltenVK IOSurface/`MTLTexture`-backed `VkImage`** — does our MoltenVK 1.4.1
   give vkr an IOSurface-backed texture via `VK_EXT_metal_objects`? Host-only spike (extend the
   `venus-render-server` harness, no VM boot). Decides the entire B+D present architecture — do it
   *before* writing present-path code.
2. **mutter's exact requirement** — which external-memory ext + image props make mutter pick its
   GBM/KMS-scanout path on venus, and whether an IOSurface-backed venus image satisfies it (#30/D).
3. **`SET_SCANOUT_BLOB` → IOSurface → `CALayer.contents`** present path in libkrun (today
   `worker.rs:392` panics) — crossing B, correctness-first then zero-copy.
4. **#28 host-clean-to-PoC spike** (crossing C, cheap) — host `dc cvac` then guest invalidate+read.
5. **Productize the win** (#26): deliver `/opt/mesa-zink` into the guest image (DIAG-free), put apps
   on venus by default; clean perf baseline (#29: `pkill` stale capture VMs first).

Suggested order: finish #26 → spike #1 (MoltenVK-IOSurface, decides architecture) → build B+D present
on its result → #28 host-clean spike in parallel. Memory: `limina-tier2-venus`.

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

**Status: 🟢 core done 2026-06-12 — control plane, clipboard, virtiofs sharing, and liveness
all live; only the productization track (sysext guest-tools + kernel RPM) and follow-ups remain
(see the end of this block).** `crates/limina-proto`
(16-byte `LIMINA` frame header + CBOR/minicbor payloads; HELLO/WELCOME/HEARTBEAT/SHUTDOWN/
SHUTDOWN_ACK/ERROR; unknown types → `ERR_UNSUPPORTED`, never fatal); the L1 guest agent
(`guest/limina-init` `agent` module) speaks it; and the **supervisor owns the host side by
default** (binds a private control socket at the well-known `CONTROL_PORT`, serves the
handshake, and turns **window-close/SIGTERM into an orderly guest power-off**: SHUTDOWN →
5s agent grace → power-button SIGTERM → SIGKILL — closing M2's orderly-shutdown clause for
agent guests). Explicit `--vsock-*` still passes the raw plumbing through (the harness
drives the protocol itself that way). The **product `limina-agent` daemon ships too**
(`guest/limina-agent`, a musl static binary + `limina-agent.service`): reconnect-forever loop,
poll-driven heartbeats, SHUTDOWN → `systemctl poweroff` (raw `reboot(2)` fallback);
installed via `scripts/install-guest-agent.sh` and **baked into the dev-enh golden image**
— window-close on the seated desktop is now a verified orderly GNOME power-off (~1s,
exit 0). Tests: limina-proto L0; `l1_agent` (protocol end-to-end through the shipped
binaries); `l1_shutdown` (the supervisor-owned orderly path, exit 0 unforced in <5s);
`l1_real_agent` (the product daemon, zero-config port + agent-driven power-off);
`l1_multi_agent` (two concurrent agents on the multi-peer registry). **The TEXT CLIPBOARD
is LIVE end-to-end (2026-06-12):** `CHANNEL_CLIPBOARD` (OFFER/REQUEST/DATA, newest-serial
wins both ways), the supervisor's NSPasteboard bridge (`crates/limina/src/clipboard.rs` —
changeCount poller, self-change suppression, `LIMINA_PASTEBOARD` named-pasteboard test
override, `l1_clipboard` proves both directions), and the **`limina-agent-session` user
helper** (`guest/limina-agent-session`; systemd user unit, own vsock connection,
caps=[clipboard]) — live-verified on the seated desktop (guest copy → `pbpaste`;
`pbcopy` → Ctrl+Shift+V in ptyxis) and **baked into the dev-enh golden** (zero-install
on fresh boots; host's current clipboard syncs on connect). **The helper is two-tier
(2026-06-12 redesign):** the original mutter-RemoteDesktop D-Bus backend keeps a remote
session alive forever, which keeps GNOME's orange "screen is being shared" panel
indicator lit for the VM's whole life. The enhanced tier now implements
**ext-data-control-v1** (standardized wayland-protocols staging; KWin/wlroots parity) in
our carried mutter (`patches/mutter/0003` — GNOME refuses the protocol upstream,
mutter#524) and the helper probes for it at startup as a focusless pure-Rust Wayland
client (`wayland_clip.rs`; loop prevention = track our live source, our own set echoes
back `is_owner`); stock mutter falls back to the RemoteDesktop backend, where the
indicator is the documented cosmetic cost. Pixel-verified indicator-free on the seated
desktop with both copy directions live (rdclip oracle + iosdump). The Wayland foundation
(compositor patch + session Wayland client) is also the stepping stone for future
drag-n-drop. **virtiofs FILE
SHARING is LIVE (2026-06-12):** `limina --share '[NAME=]PATH[:ro]'` (repeatable) attaches a
host directory as virtiofs tag `limina-NAME`; the guest agent (and the L1 init seed)
auto-mounts every `limina-`-tagged device at `/media/NAME`, discovering tags via sysfs
(`/sys/fs/virtiofs/<id>/tag` — NOT the virtio-9p `mount_tag` attribute; and NOT the
cmdline, so the mechanism survives EFI boots where GRUB owns the cmdline). A guest
without the agent degrades to `mount -t virtiofs limina-NAME <dir>` by hand (two-tier).
No DAX/shm window yet — same shm-less shape as the proven L1 rootfs; DAX is a tracked
perf enhancement (the 16 KiB-host alignment question moves there). Tested by `l1_share`
(read + write round-trip through the shipped binaries, plus `:ro` write-refusal) and
live-verified on the seated desktop both directions with the share-aware agent **baked
into the dev-enh golden** (zero-install on fresh clones). **Heartbeat-liveness
surfacing is in (2026-06-12):** the control plane stamps every inbound message per
peer; a monitor thread reports (once) any agent silent past `LIMINA_AGENT_SILENT_SECS`
(default 5 s ≈ 5 missed beats) and its recovery — the supervisor log is the status
surface until a CLI/UI consumes it (`l1_liveness` proves silent + recovery on a mute
harness peer while the healthy seed agent stays unreported). **With that, every
core M5 capability is DONE** (control plane + agents, clipboard, virtiofs sharing,
liveness); what remains is the productization track (sysext guest-tools delivery +
kernel RPM, below — the GOP+venus singleton blocker is now cleared, libkrun 0022) and
follow-ups: a `liminactl
status`-style consumer, images/files on the clipboard, DAX, uid mapping for shares,
the clipboard test-gap ledger.

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
   - **Start from the L1 vsock agent seed** — the test guest's init already runs a tiny vsock
     agent (`guest/limina-init`, `tests/l1_vsock.rs`, gated on a `limina.agent_port=` cmdline token);
     grow that into `limina-agent` rather than starting fresh.
   - **Agent delivery via virtiofs overlay** (`krun_fs_add_overlay_file`/`krun_fs_add_overlay_dir`)
     + a minimal per-user systemd unit, keeping the user's `.raw` untouched.
   - **The agent is also the enhanced-tier configurator (learned from M4), and the delivery
     design is SETTLED (2026-06-12):** tier-2 today depends on guest config hand-baked into the
     dev image (`environment.d` policy, `/opt/mesa-zink` + patched venus ICD + patched mutter,
     the 16 KiB kernel); productizing moves all of it to two carriers:
     - **Userspace rides a versioned `systemd-sysext` image (+ confext for `/etc`):**
       `limina-guest-tools-<ver>.sysext.raw` carries the agent + units, the patched mutter
       (overlayfs upper shadows the exact path gnome-shell loads — solves the
       not-parallel-installable problem, `unmerge` = instant rollback), mesa zink + venus ICD
       under a limina-owned dir selected via ICD JSON/env (do NOT shadow stock mesa — explicit
       override mechanisms exist), and `/usr/lib/environment.d/` policy. Additive, reversible,
       rpmdb-untouched — the package-manager-shaped form of the two-tier tenet. **Bonus:
       distro-agnostic** — sysext is plain systemd (≥254), so the same image format serves
       Debian-class guests later with only per-distro `extension-release` matching.
       `systemd-sysupdate`/`importctl` (256+) is the ready-made update channel.
     - **The kernel goes through the distro's own EFI boot machinery, NOT host direct-boot**
       (that was the dev vehicle; BLS/GRUB fallback, kdump, dracut, the M2.5 boot console all
       assume the guest boots itself): package as a kernel RPM (deb later) installed via
       `kernel-install` → BLS entry alongside the stock kernels (stock = one GRUB choice away =
       the degradation path). **Host-built primary** (our container pipeline already
       cross-builds it; needs `CONFIG_EFI_STUB` + dracut-managed rootfs), in-guest rebuild as
       the self-hosting fallback. A sysext cannot carry the kernel image (the bootloader reads
       `/boot` before userspace exists; sysext overlays `/usr` only) — module trees could ride
       one, but they ride the RPM anyway.
     - **Bootstrap is a one-time Parallels-style "install guest tools"** (ISO or attached
       volume): graceful degradation is exactly what makes this acceptable — the stock guest
       has a working (software-GL) desktop before enrollment; the installer drops the sysext +
       kernel package, and from then on the agent owns updates.
     - **Consequence (priority change):** EFI-booting the enhanced tier needs venus to survive the
       EFI→kernel reset — **that singleton prerequisite is now ✅ DONE** (M4 open item 2; libkrun
       0022, 2026-06-13: the renderer persists across a device reset). What remains for the EFI path
       is making the 16 KiB kernel BLS-bootable (this delivery work) so GRUB selects it.
   - Reuse libkrun's existing macOS host->guest time sync (DGRAM vsock port 123, `timesync.rs`)
     instead of a custom TIME_SET — just confirm a guest-side consumer exists.
2. ~~**virtiofs file sharing.**~~ **DONE (2026-06-12, shm-less).** `limina --share` →
   worker `--share TAG=PATH[:ro]` → the existing `add_fs_device` plumbing (no DAX/shm
   window — the L1-rootfs-proven shape); the agent auto-mounts `limina-*` tags at
   `/media/<name>` via sysfs discovery. See the status block above. **Deferred to a perf
   follow-up:** the DAX/shm window (`VirtioShmRegion` in `fs/device.rs`; confirm
   shm-window alignment and FUSE_SETUPMAPPING/SHMCAP on 16 KiB host pages and that
   `mount -o dax` works) and host↔guest uid mapping.
3. **Clipboard bridge.** limina-agent (guest) <-> liminad (host) over a full-duplex vsock connection.
   Host side: NSPasteboard `changeCount` polling (no macOS push notification) + static MIME<->UTI
   mapping + promised/lazy data provider. App protocol: length-prefixed binary frames
   (HELLO/OFFER/REQUEST/DATA_HDR/DATA/CLEAR/PING) with monotonic serials + 32-64 KiB chunking on
   vsock credit flow control. Loop-prevention: ignore writes the bridge originated.
   - **Guest-side mechanism (RESOLVED 2026-06-12 by reading our vendored mutter — see Risks):
     the `org.gnome.Mutter.RemoteDesktop` D-Bus clipboard API** (`EnableClipboard`/`SetSelection`,
     session bus — what gnome-remote-desktop uses), NOT a Wayland data-control protocol: mutter
     49.5 implements neither `wlr-data-control-unstable-v1` nor `ext-data-control-v1`, which also
     rules out the `wl-clipboard` shell-out for a background agent on stock GNOME. Works on the
     **stock** tier (no mutter patch needed) — **spike-verified end-to-end 2026-06-12, both
     directions, real apps** (`spikes/clipboard-remotedesktop/RESULTS.md`). Keep data-control as
     the later non-GNOME-guest path.
   - **Design consequence (from the spike):** the clipboard lives in a per-session **user**
     helper (`limina-agent-session` + systemd user unit; the RemoteDesktop session needs the
     user's bus and must stay resident to service SelectionTransfer), so the control plane
     must accept multiple concurrent guest connections (root agent + session helper; vsock
     connect needs no root). **DONE (2026-06-12):** host `ControlPlane` keeps a peer
     registry (serve thread + HELLO/caps per connection); SHUTDOWN routes to every
     `shutdown`-capable peer. Covered by `l1_multi_agent` (init seed + real limina-agent
     connected concurrently).
   - **M5 = text-only.** Images is a follow-up; files/primary-selection/HTML deferred.

**libkrun patches:** none for the transport (vsock + virtiofs overlays already exist). Possibly a
small fix if the guest cannot cleanly reconnect a HANG_UP'd port without a VM restart
(`unix.rs:548-562`).

**Done test:** A host folder appears mounted in the guest with read/write; copying text in the guest
pastes on the macOS host and vice-versa; `liminactl status` shows the agent HELLO/WELCOME handshake and
heartbeats.

**Risks / spike first:**
- ~~**#1 blocker: does GNOME/Mutter on Fedora 43 Wayland implement `wlr-data-control-unstable-v1`
  / `ext-data-control-v1`?**~~ **ANSWERED (2026-06-12, by grepping `third_party/mutter` 49.5 — no
  runtime spike needed; we vendor the compositor):** it implements **neither**, so no data-control
  client (including `wl-clipboard`) can serve an unfocused agent on stock GNOME. The sanctioned
  channel is mutter's **RemoteDesktop D-Bus clipboard API** (see task 3). ~~Residual spike:
  confirm a non-gnome-remote-desktop session client may create a RemoteDesktop session
  (permissions/portal), and prototype get/set from a systemd user unit.~~ **DONE (2026-06-12,
  `spikes/clipboard-remotedesktop/`): YES on all counts** — no access control on CreateSession
  (verified in vendored source + live), and a session-bus-only background client (the systemd
  user-unit shape) set + read the selection AND round-tripped through real apps both ways
  (paste into ptyxis → file; copy from gnome-text-editor → SelectionRead). Stock guest, zero
  modifications. Operational traps (stuck-modifier keysym injection — use keycodes; resident
  owner required; O_NONBLOCK fds; overview swallows input) in the spike RESULTS.md.
- Can the NSPasteboard promised-data provider block long enough to round-trip a guest REQUEST/DATA
  without AppKit timing out the paste?
- virtiofs DAX alignment on 16 KiB host pages. (Less scary than when written: the enhanced tier
  already runs a 16 KiB-page guest kernel — venus requires it — so guest-page = host-page is the
  default enhanced configuration; test stock-4k DAX separately.)
- Validate large chunked vsock transfers respect credit flow control without stalling muxer threads;
  set a max-size cap / temp-file staging.
- **Delivery-design spikes (both cheap):** (a) **SELinux labeling of sysext content** — the dev
  guest runs `selinux=0` but stock Fedora is Enforcing; the overlaid mutter/agent files need
  correct labels at image-build time or gnome-shell/systemd will refuse them. (b) **EFI-boot the
  16 KiB kernel end-to-end** — kernel RPM → `kernel-install`/BLS → GRUB → venus desktop (verify
  `CONFIG_EFI_STUB`, dracut initramfs on 16k, and BLS default selection with the stock fallback
  entry intact). (b) is also where the GOP+venus singleton fix gets exercised.

**Clipboard test-coverage gaps (tracked 2026-06-12; ~~oversized-content~~ fixed same day with
`l1_clipboard_oversized_content_keeps_channel_alive`):**
- ~~**`limina-agent-session` under automation**~~ **DONE (same day, `l1_session_helper`):** the
  L1 guest now carries a real session bus (`scripts/build-dbus-guest.sh` — Alpine's musl
  dbus-daemon + 4-file closure; the rootfs is a host dir over virtio-fs so growth is free) and
  **`limina-mock-mutter`** (a scripted zbus stand-in claiming `org.gnome.Mutter.RemoteDesktop`,
  driven/observed through host-visible rootfs files). The REAL helper bridges both directions
  against the REAL supervisor + named pasteboard in ~3.5s. The dbus stack (`limina.dbus` cmdline)
  is general L1 infrastructure for future D-Bus-needing tests.
- **Initial-offer-on-connect** (host pushes its current clipboard to a late joiner) — live-verified;
  the L1 test boots with an empty pasteboard so the path never fires there.
- **Stale-serial races** — newest-wins exists on both sides; no test provokes an out-of-order
  exchange yet.
- **Multi-peer clipboard broadcast** + dead-peer pruning under broadcast failure.
- **Helper resilience** — vsock reconnect after a supervisor restart; D-Bus session death → clean
  exit (systemd restart into the new session).
- **The ext-data-control (enhanced) backend under automation** — `l1_session_helper` exercises
  the RemoteDesktop fallback (the L1 guest has no compositor); the Wayland backend is
  live-verified only. Closing this means a headless ext-data-control server stand-in (or a real
  compositor in the L1 guest) — weigh cost vs. the live coverage.

---

## Milestone 6 — Dynamic memory (balloon, min..max)

**Goal:** The VM is given a `min..max` RAM range; it takes memory under guest pressure and returns
it to macOS when idle, with `phys_footprint` actually dropping.

> **The case now has numbers (measured 2026-06-11 on the tier-2 desktop):** host RSS is a
> guest-page **high-water mark** — 5.2 GiB at boot-idle → 6.8 GiB after a browsing session, and it
> *never returns* without a balloon. (Shared mappings: 4 GiB guest RAM + the 8 GiB venus shm
> window, lazily mapped.) Guest idle usage ~2 GiB is Fedora's own daemons, not VM overhead — so
> reclaim is the lever, not guest slimming. Also: GPU/Metal buffers recycle correctly (the one
> IOSurface leak was found and fixed), so the balloon is the remaining memory story.

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
- **The 4K↔16K page-size mismatch — the menu has since collapsed in our favor (M4 learning).**
  The enhanced tier **already runs a `CONFIG_ARM64_16K_PAGES` guest kernel** — venus host-visible
  blob maps *require* it (`hv_vm_map` 16 KiB alignment), so option (b) is no longer a speculative
  custom-kernel track, it is the shipped enhanced configuration: **1:1 reclaim is free on the
  enhanced tier.** Host-side coalesce/align in `process_frq` **(a)** is therefore the *stock-tier*
  fallback (measure how much stock 4 KiB Fedora reporting actually returns — still the spike).
  Option **(c)** — host-page-aware `mm/page_reporting.c` negotiated via a feature bit — is now
  cheap to carry if (a)'s waste is material: the kernel-patch pipeline exists
  (`patches/linux/*.patch`, auto-applied by `scripts/build-test-kernel.sh`, three patches carried
  today). **(d)** virtio-mem stays later. See doc 08 §1.2.
- Re-touch latency/cost of MADV_FREE_REUSE on deflate for an interactive desktop.
- PSI watermark/hysteresis tuning to avoid balloon thrash (build/browser/IDE workloads).

---

## Milestone 7 — USB passthrough

**Goal:** Pass a host USB device (initially libusb-claimable: FTDI/CP210x, YubiKey-class, etc.) into
the guest.

**Key tasks (USB is entirely net-new; the bundled/custom guest kernels have USB compiled OUT today):**
1. **PREREQUISITE — enable USB in OUR kernel config (cheap now).** Since this was written, the
   enhanced tier standardized on **our own kernel** built by `scripts/build-test-kernel.sh`
   (16 KiB pages, `patches/linux/` auto-applied) — so the prerequisite is a config edit in *our*
   pipeline, not a libkrunfw rebuild: add `CONFIG_USB_SUPPORT=y`, `CONFIG_USB=y`,
   `CONFIG_USBIP_CORE`, `CONFIG_USBIP_VHCI_HCD`, and needed `CONFIG_USB_*` class drivers (while
   there: `CONFIG_UINPUT` — its absence already bit us, ydotool is unusable in the guest).
   libkrunfw's bundled kernel (`# CONFIG_USB_SUPPORT is not set` in every arch profile,
   `config-libkrunfw_aarch64:2151`) only matters where limina still uses it (L1 fallback). The EFI
   distro kernel already has USB.
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
   - **Pointer capture (relative mode):** switch the absolute tablet to the relative mouse on
     capture (games, virt-viewer-style use). Known gap to close here (from the M2 pointer
     redesign): the host pointer ignores guest `cursormove` positions, so **guest-initiated
     pointer warps** aren't reflected host-side — capture mode is where that gets reconciled.
   - **Multi-display:** multiplex all displays through the single `krun_set_display_backend` by
     `scanout_id` (up to 16 displays), mapping each to its own NSWindow/CAMetalLayer.
   - **Runtime window-follow resize / EDID hotplug (libkrun patch):** no post-`krun_start_enter`
     entry point changes display size today. Add a C call that raises a virtio-gpu config-change
     interrupt + updates `DisplayInfo` (the virtio-gpu GET_DISPLAY_INFO/GET_EDID + config-change
     capability already exists — plumbing, not new device work). This is the #1 display gap.
   - ~~**Hardware cursor**~~ — done in M2 (libkrun 0008 + the host-cursor adoption redesign).
   - ~~**Zero-copy scanout**~~ — done in M4 (`SET_SCANOUT_BLOB` + IOSurface present, fence-accurate).
   - **Capability-scope the scanout IOSurfaces (security hardening).** Today the worker exports each
     guest scanout by its **global `IOSurfaceID`**, and the window process maps it with
     `IOSurfaceLookup` (`window.rs`). That namespace is machine-wide and *not* capability-gated: the
     IDs are low, sequential, and enumerable, so any **non-sandboxed same-user** process can brute-
     force `1..N` and `IOSurfaceLookup` the guest's rendered frames — i.e. silently read the guest
     screen with no screen-recording prompt and no window in front. `spikes/venus-draw-probe/iosdump.swift`
     is a working proof of concept (it maps any global IOSurface id cross-process). Severity is
     bounded — local-only, same-user (a process at that privilege can already screen-record / read our
     memory), and the macOS app **sandbox already blocks** the App-Store-app case — so it's
     productization hardening, not an emergency. Fix: have the worker export via
     `IOSurfaceCreateMachPort` and hand the **port right** (a capability) to the window process instead
     of a global id, mapping with `IOSurfaceLookupFromMachPort`; only our process can then map the
     surface, and the global namespace is dropped entirely. The cost is a small **mach rendezvous** to
     pass the port to the worker we spawn (`SCM_RIGHTS` passes fds, not port rights) — an initial mach
     channel or a bootstrap-registered name. This is the concrete case where the host↔worker Mach-port
     option earns its keep.
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
| M3 networking ✅ (NAT+SSH; bridged deferred) | gvproxy supervision + gateway cleanup; well-known-MAC static lease | none needed (reconnect-on-HANG_UP still optional) |
| M4 3D 🟢 (substantially done) | coexist routing, zero-copy + fence-accurate present path, KK as host driver | shipped: coexist (0010), fence-present series (0017–0021), virglrenderer fork (blob/IOSurface/cross-context), KK perf/XFB patches, kernel `patches/linux/0001–0003`, mutter ×2; remaining: the upstream queue |
| M5 clipboard/fs/agent | guest agent (from the L1 vsock seed), liminad, NSPasteboard bridge, mutter RemoteDesktop clipboard client, enhanced-tier installer | none for transport (vsock+virtiofs exist) |
| M6 dynamic memory | PSI autoballoon policy | reclaim fix (MADV_FREE_REUSABLE — spike-confirmed) + 16KiB align + inflate/deflate + krun_*balloon* API + DEFLATE_ON_OOM |
| M7 USB | host claim/attach, usbip plumbing | our-kernel config edit (USB+uinput on); later native virtio-usb + krun_add_usb* |
| M8 audio/x86/polish | fullscreen, keymap, multi-display, pointer capture, FEX wiring | native virtio-snd; runtime resize/EDID; LED parity (hw cursor + zero-copy scanout already landed) |

## First three things to spike (highest uncertainty, gate the most)

1. ~~**M1 boot path:** EFI+disk vs root_disk_remount against the real Fedora `.raw` layout.~~
   **RESOLVED** (`spikes/m1-boot`): EFI+disk boots to userspace, no remount needed.
2. ~~**M2 input worker on macOS:** does `--features input`'s epoll/eventfd shim build/wake on Darwin
   arm64?~~ **RESOLVED**: it does; keyboard + absolute pointer drive the live desktop.
3. ~~**M6 reclaim:** does `MADV_FREE_REUSABLE` actually drop `phys_footprint` on an `hv_vm_map`'d
   region?~~ **RESOLVED** (`spikes/balloon-madvise`, 2026-05-30): yes, fully, with no
   unmap/protect first — dynamic memory is feasible; re-confirm on the shipping macOS release.

All three founding spikes are resolved. The standing rule remains: spike the gating unknown
before building on it (the M5 instance — "can a session client drive mutter's RemoteDesktop
clipboard?" — is listed in M5's risks).
