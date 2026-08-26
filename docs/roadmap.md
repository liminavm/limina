# limina Roadmap

A milestone-based, bisectable plan for **limina** — a Rust macOS app on top of libkrun
(Hypervisor.framework) to replace Parallels for running Linux guests on Apple Silicon.

Each milestone has: **Goal**, **Key tasks**, **libkrun patches**, **Done test**, **Risks /
spike first**. Milestones are ordered by dependency; ship and tag each one before starting the
next so regressions bisect cleanly.

Grounded in the research under `docs/research/01..11`. API names are verified against the
vendored header `third_party/libkrun/include/libkrun.h` unless noted.

---

## Where we are (refreshed 2026-08-08)

The milestone numbers are a *dependency* order, not a schedule, and by now they are only
loosely a sequence: several later ones shipped before earlier ones finished. Read this
section for the honest state; read each milestone for the detail.

**Shipped and in daily dogfood use.** M1 boot, M2 display+input, M2.5 console/serial, M3
NAT+SSH, M6 dynamic memory, M9 suspend/resume + snapshots, M10 multi-disk + ISO, M11 the
`cargo xtask` surface. The desktop-polish half of M8 and the audio half both shipped.
M4 (venus 3D) and M5 (clipboard/virtiofs/agent) are green at their cores. M14's biometric
work shipped *both* halves — FIDO and the impersonated fingerprint reader — along with the
emulated xHCI controller it shares with M7, which is default-on.

**In flight.** M15 (display pipeline v2): wave 1 parts 1+2 shipped, wave 4's spike closed
with a partial win; per-host-display + VRR and waves 2–3 remain. This is the main line of
work.

**Planned, not started.** M13 (visibility/power render adaptation). M12's clipboard half
shipped alongside M5's (#37, 2026-08-15); its file-transfer half, and an arbitration probe that
tests the right thing, are what remain.

**Deferred by decision, not by neglect.** M3 bridged networking and M7 real-device USB
capture both wait on the single shared privileged helper (`docs/design/privileged-helper.md`);
M8's x86 emulation and multi-display remainder are unscheduled.

**Moonshot, design captured.** M16 (LiminaOS: a purpose-built guest distro + system
compositor) — design discussion recorded 2026-08-10, deliberately unscheduled. The
compositor half is sequenced to be shippable on the Fedora enhanced tier first.

**Not a milestone, but load-bearing.** Since roughly 2026-07 a growing share of the work has
been *robustness* rather than features — host-memory discipline, guest-triggerable aborts,
lifecycle correctness. It now has its own cross-cutting section below, because filing it
under whichever milestone the symptom appeared in was losing it.

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
- **Repo hygiene — the FORK model (migration completed 2026-08-06).** Every dependency we
  patch is a fork under `github.com/liminavm`, checked out into gitignored `third_party/` and
  pinned by rev in the committed `third_party/manifest.toml`. **The fork's `limina` branch IS
  the delta** — there is no patch series any more, and the `patches/**` directories are
  tombstones (the one exception, `patches/mesa-guest/`, is a committed *export* derived from
  the guest-mesa fork, because the RPM build runs where that checkout isn't reachable). To
  change a dependency: commit on its `limina` branch, push, bump the manifest rev. Branches
  get rewritten as patches land upstream, so **tag before every rewrite** — every rev ever
  pinned must stay reachable. `cargo xtask vendor` recreates every tree from the manifest.
  Upstreaming status per dependency: `docs/upstreaming/ledger/`.
  - **Reading older text in this doc:** a reference like `patches/linux/0006` or "libkrun 0119"
    now names a *commit on that fork's `limina` branch*, not a file in a series. The numbering
    survived the migration because it is how we talk about these changes; the directories did
    not.

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

## Robustness & resource discipline (cross-cutting)

Booked as its own section on 2026-08-08. These items kept being filed under whichever
milestone the symptom surfaced in — a memory leak found while chasing a display bug is not
display work — and were getting lost. The unifying property: **the guest is untrusted and
the host must survive it**, whether the guest is malicious, buggy, or merely enthusiastic.

Two failure shapes drive nearly all of it. First, **host resources the guest can consume but
cannot see**: memory allocated on its behalf lands in the *worker's* address space, so the
guest's own accounting stays flat while macOS eventually jetsams the worker and the whole VM
dies at a moment unrelated to the cause. Second, **guest-reachable aborts**: a malformed
command that trips an assert in a host library kills the VMM for every client.

**Shipped.**
- **Host GPU-memory budget** (`docs/design/gpu-memory-budget.md`) — a per-context ledger with
  an exact-size histogram, always on, plus an opt-in cap that refuses and deliberately kills
  the offending context rather than letting the worker grow until jetsam takes it. Since
  2026-08-08 the guest is also *told* the truth through `VK_EXT_memory_budget`, which is the
  one backpressure channel venus doesn't discard (a refused allocation can't be reported —
  the transport drops our `VkResult` — but a budget query round-trips synchronously), so a
  well-behaved client can shrink its caches instead of being killed.
- **Scanout retention** — the holder turned out to be the *supervisor*, not the worker: an
  unbounded frame-apply surface cache, and then, once capped, a send right it never dropped.
  Worth remembering as a method lesson — capping the cache **bounded** the growth and read
  like a fix for a week; the actual retention was still there underneath. Guarded by
  `scanout_churn_retention`. The buffer-lifetime matrix that found it
  (`spikes/venus-churn-retention/buffer-lifetime-matrix.md`) is now fully run, all paths.
- **Guest-triggerable host aborts** — four instances of the empty-clear-rect class fixed at
  the vkr trust boundary; the dogfood KosmicKrisp now builds with asserts off.

**Open.**
- **Audit the vkr dispatch boundary for guest-triggerable invalid usage** (was task #12). The
  fixes so far were reactive, one incident at a time. A systematic pass over the venus
  dispatch entry points — every place a guest-supplied count, offset, handle or rect reaches
  a host API — would turn a recurring class into a closed one. Pairs naturally with the
  upstreaming work, since "validate at the trust boundary" is exactly what upstream wants.
- **Rein in the leak-hunt instrumentation's log volume** (was task #6). The census and
  per-allocation tracing that made the 2026-08 leaks findable are too loud to leave on by
  default; they need levels, so the diagnostic power survives without the noise.
- **GL-only guests are unbounded.** The GPU-memory cap is enforced at `vkAllocateMemory`,
  which a pure-GL (vrend) session never reaches. Its allocations *are* accounted — they show
  up in the shared vrend bucket — so the ledger sees the growth; nothing stops it.
- **Smaller lifecycle nits** are tracked in `docs/hardening-backlog.md` rather than here, and
  get folded into the next commit that touches their file.

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
- **Supervision:** `crates/limina` spawns the signed worker in its own process group and forwards
  graceful shutdown on SIGINT/SIGTERM, climbing the rungs of the stop ladder. It never kills on a
  timer — see M12.5.

**Known caveat:** a *stock* EFI Fedora guest does not honor the GPIO power button (KRUN_EFI's ACPI
doesn't advertise it), so graceful power-off needs either our `limina-agent` (M5) or the stock
`qemu-guest-agent` rung (M12.5).

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
  the agent SHUTDOWN, and falls back through the power button and the stock guest agent for stock
  guests; `l1_shutdown` asserts the ladder).

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
  alongside venus**. **See `docs/graphics.md` for the authoritative, current tier model** (this bullet
  is kept only to flag the reversal that confused past readers).
- **Tier-2/3 is a "coexist" device, not a flag flip.** The rutabaga serves Vulkan (venus) **and** GL
  (vrend); it does NOT implement the 2D commands the firmware GOP / efifb / fbcon / scanout-present use.
  Our software-2D patch (0001) serves exactly those. **libkrun patch 0010** makes both live in one
  virtio-gpu, routed by ring/command type (2D → software-2D CPU path; 3D ctx/submit/capset →
  rutabaga/venus), and **degrades gracefully to software-2D on renderer-init failure** (no panic).
  **Coexist is the DEFAULT** (`--gpu-software-2d` overrides for the capture oracle). Design: `docs/graphics.md` §2.
- **The 16 KiB-host / 4 KiB-guest blob map — SOLVED, and no longer a reason for 16 KiB pages.**
  venus RESOURCE_MAP_BLOB → `hv_vm_map` requires host addr, guest addr, AND size to be
  16 KiB-multiples, and a 4 KiB guest breaks that two ways. The **size** half was fixed *host-only*
  (libkrun `0043` rounds map/unmap identically; virglrenderer `0023` opens the zink map-info gate) —
  so the old "no host-only fix exists" line here was wrong. The **offset** half (two 4 KiB-packed
  blobs sharing one host page) needs the guest to keep an aligned lattice, which a 16 KiB kernel
  gets for free but is *not* the only way to get: `guest/virtio-gpu-dkms/` does it on stock 4 KiB
  guests (validated 2026-07-03 — venus enumerates on the stock tier), and upstream has since merged
  the negotiated form, `VIRTIO_GPU_F_BLOB_ALIGNMENT` (in 7.2-rc). **16 KiB stays *the* enhanced
  tier because it is better — but nothing hard-requires it any more.** The other things 16 KiB buys
  (udmabuf `mach_vm_remap` stitching, virtiofs DAX, balloon reclaim granularity) all degrade
  gracefully on 4 KiB, and the balloon's sub-page coalescing is already implemented
  (`virtio/balloon/device.rs:83-140`). venus was the sole hard failure — and the sole one because
  its failure mode poisoned the whole Vulkan loader rather than degrading. Full analysis, the
  three-link chain, the ordering hazard, and the short upstream list that would make a stock distro
  work unaided: `docs/design/16k-page-requirement.md`.
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
default** in the coexist flags — see `docs/graphics.md` §3.2. What *was* parked is the unrelated
**virgl-over-ANGLE** variant (a separate GL→VK→Metal translation stack); zink-on-KK replaced the need
for it. Native-context (vdrm) backed by Metal is recorded only as a curiosity — venus is the top tier.

**Dependency changes (shipped):** coexist (0010), fence-present series (0017–0022), the
virglrenderer fork, KK perf/XFB work, guest-kernel fork commits. The two carried **mutter patches
are RETIRED** — guest mutter has been stock since 2026-07-11, and the user is writing a
gnome-shell/mutter replacement, so patching GNOME's compositor stopped being a path we invest in.
Remaining: the upstream queue (`docs/upstreaming/ledger/`).

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
  the stock guest agent → wait; see M12.5 — it never kills on a timer). Explicit `--vsock-*` still passes raw plumbing for the harness.
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
  the plan was **ext-data-control-v1** in a carried mutter (GNOME refuses it upstream, mutter#524),
  giving a focusless client with no "screen is being shared" indicator. **That patch is retired and
  guest mutter is stock** (since 2026-07-11). The `clipboard@limina` shell extension that took
  its place was itself retired 2026-08-15 (#37 step 4): under GNOME the clipboard now rides stock
  `spice-vdagent`, and `limina-agent-session` — ext-data-control, with the
  `org.gnome.Mutter.RemoteDesktop` D-Bus clipboard API as the opt-in fallback (the orange
  indicator is its documented cosmetic cost) — covers the sessions vdagent cannot serve. Loop prevention = track our own live source / `is_owner` echo.
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
rewritten after the fact (2026-07-01) — the authoritative rationale lives in `docs/graphics.md`
(§5.1 Delivery) and `docs/images.md`. Short form:

- **Userspace = rebuilt Fedora SRPMs replacing stock at `/usr`** (mesa pinned to our version +
  dnf-versionlocked; mutter is **stock** since 2026-07-11 — the carried patches are retired). Why not sysext for mesa: our-mesa-vs-stock **libgallium SONAME mismatch**,
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

**Status: 🟢 emulated USB ships default-on; host-device passthrough is what remains, blocked on the
privileged helper.** Note the shape changed after this milestone was written: the **emulated xHCI
controller** built for M14 (libkrun 0095–0098, `docs/design/usb-xhci.md`) is now the guest's USB
controller, default-on since `f9646d0`, and it carries our own gadgets (FIDO, fingerprint) into a
*stock* guest with zero guest components. The USB/IP work below is still the plan for passing a
**real host device** through, but it is no longer the only USB in the product. Original status,
still accurate for that half: **mock passthrough works end-to-end; real-device capture proven; the
helper is DEFERRED.**
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

**Key tasks (USB was entirely net-new when written; our test/enhanced kernels have since enabled USB —
`build-test-kernel.sh:111-132` sets `CONFIG_USB_SUPPORT`/`USBIP_VHCI_HCD`/xHCI + class drivers — and an
emulated xHCI controller shipped as M14 shared infrastructure, libkrun 0095–0098):**
1. **PREREQUISITE — enable USB in OUR kernel config (cheap now).** The enhanced tier standardized on
   our own kernel (`scripts/build-test-kernel.sh`, 16 KiB pages, built from the kernel fork), so
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

**Status: 🟢 the polish half is SHIPPED** (fullscreen, keymap remap, system-combo capture, pointer
grab, multi-display/display modes; audio shipped 2026-07-17; x86/FEX remains). The gesture follow-on
work is listed in `docs/design/trackpad-gestures.md`.

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
     by default** (PC-style muscle memory out of the box); `--no-normalize-modifiers` opts out,
     last-wins with `--normalize-modifiers`.
     Renamed and made positional 2026-08-24 — it reads macOS's own modifier remapping and inverts
     it, so the rule lands on the physical key. Fully customizable keybindings beyond the
     swap still ahead.
   - ~~**System-combo capture (Cmd-Tab/Cmd-Space/Ctrl-arrows):**~~ **DONE (keyboard)** — the capture
     CGEventTap consumes keyDown/keyUp/flagsChanged while captured and forwards them to the guest,
     so system key-combos act in the guest, not the host. Re-enables on `kCGEventTapDisabledByTimeout`.
     **Limitation:** multi-finger trackpad gestures (Mission Control / Spaces swipe) are processed by
     the WindowServer upstream of a session tap and are NOT interceptable (two-finger scroll is).
     Secure Input (password fields) can still suppress the tap — acceptable.
   - ~~**Pointer capture:**~~ **DONE** — `Cmd-Ctrl-G`. *(The design described in this paragraph
     was later superseded by the fullscreen pointer grab —
     `docs/design/fullscreen-pointer-grab.md`; commits `43bf803`, `02d606b`, `d44f14c`,
     `ead3520`.)* A **session-level consuming
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
   - ~~**Multi-display:**~~ **DONE** — display modes, per-host-display identity/HiDPI, fullscreen
     carrier + notch overlay, and mode/display restore shipped (`docs/design/display-modes.md`,
     `docs/design/display-cutouts.md`; commits `519b3de`, `31f9d4f`, `5c14568`, `49ee225`,
     `3a4c633`).
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
   - **`virtio-rtc` — sub-second guest time (booked, low priority).** PL031's `RTCDR` is a 32-bit
     *seconds* counter, so both tiers land within ~1 s of the host after a resume: stock via the
     kernel's sleeptime injection, enhanced via agent TimeSync (which steps only at ≥1 s error).
     `virtio-rtc` (mainline since 6.16 — `drivers/virtio/virtio_rtc_*`, including
     `virtio_rtc_arm.c` and `virtio_rtc_ptp.c`) hands the guest a `(host CLOCK_REALTIME, guest
     counter)` pair captured atomically and exposes it as `/dev/ptpN`; chrony consumes that as a
     PHC refclock. There is no *drift* to correct — the guest's CNTVCT is the host counter
     (`mach_absolute_time() - vtimer_offset`, `src/hvf/src/lib.rs:1287`), same crystal — so the
     win is purely the **offset** at the moments the counter↔wallclock relationship breaks: host
     sleep and snapshot restore. Concrete motivation is **virtiofs**: host files carry host mtimes,
     and a guest clock up to a second off makes `make`/`ninja`/`cargo` see future-dated or stale
     inputs across the share. Guest-side cost is one chrony `refclock PHC` line, so this is not
     zero-touch on the stock tier — lighter than the agent, but still a config drop.
     **Deliberately not `ptp_kvm`:** its discovery chain is gated on `PSCI_VERSION_MAJOR(ver) >= 1`
     (`drivers/firmware/psci/psci.c` → `psci_init_smccc` → `kvm_init_hyp_services`), and we
     advertise PSCI **v0.2** (`src/hvf/src/lib.rs:1017` returns `2` = major 0, minor 2), so the
     guest never issues the vendor-hypervisor UID probe at all. Enabling it would cost a PSCI-level
     bump on a working boot path (obliging `PSCI_FEATURES`, `ARM_SMCCC_VERSION` ≥ 1.1) plus
     impersonating KVM's vendor-hypercall UID — for the same result a virtio device gets natively.
     **Gates before starting:** confirm Fedora's aarch64 kernel enables `CONFIG_VIRTIO_RTC`, and
     that a measured post-resume error justifies it (the recorded figure is ~0.14 s).

**libkrun patches:** native virtio-snd (largest); optional LED parity; a `virtio-rtc` device
(booked). (The runtime display-reconfigure call shipped as 0025/0026.)

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

**Open hardening item (2026-08-03, from `spikes/suspend-resume-adhoc/`): guard the two
ad-hoc suspend footguns.** (a) A raw `SIGUSR1` to the worker snapshots WITHOUT the s2idle
bracket, and the restore path assumes a bracketed snapshot — restoring such a snapshot
livelocks the guest (one vCPU captured mid-userspace). Make the raw trigger bracket-first
(or refuse / tag the snapshot as unbracketed and refuse to restore it) so no reachable
path can produce a poisoned snapshot. (b) An ad-hoc `--disk` run with `--snapshot-file`
still hard-resolves window-close to Shutdown (`main.rs` `on_window_close` — "nothing to
persist into" no longer holds); arm close→suspend there, and consider a `--on-window-close`
flag. Related test gap: no L2 covers snapshot round-trip under dynamic memory + FRQ.

**Status: ✅ M9.1–M9.4 SHIPPED.** M9.3 (windowed/GPU suspend — snapshot machinery libkrun 0076–0086 +
the virgl snapshot/restore journal 0033–0040; `bdba55b`, `993d6c0`, `87c2330`) and M9.4 (suspend/resume
UX + snapshot speed, v6: 6.6s save / 465 MiB / 2.3s restore apply; `460e3df`, `f881807`, `ce97194`)
landed on top of the headless M9.1+M9.2. The paragraph below is the historical record as of the M9.2
close. M9.1 (host-side vCPU+GIC+RAM snapshot
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
- **`app [--debug]`** — assemble the full self-contained `target/Limina.app` (the shipping bundle
  with the whole host venus/GL closure), wrapping `scripts/build-app.sh`. Builds **release by
  default**; `--debug` opts into a debuggable bundle (`xtask/src/main.rs:98-105`).
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

**Status: 🟢 clipboard shipped 2026-08-15 (#37); the arbitration probe tests the wrong thing.**
libkrun already had named multiport ports, so no new device was needed (task 1 corrected below); the
stock agent wakes on the port, answers our capability announce, and a guest copy arrives as a real
`CLIPBOARD_GRAB`. The host broker is live (`crates/limina/src/vdagent/`) behind M5's single
pasteboard owner, and `limina-agent-session` yields to a live vdagent per session. The
guest-triggerable VMM panic on port *reopen* that spike #1 uncovered is fixed (libkrun 0125). What
remains: the per-session probe below (a vdagent that is *alive* is not a vdagent that *works*), and
client→guest file transfer (task 3), not started.

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
1. ~~**libkrun: virtio-serial named multiport device exposing `com.redhat.spice.0`.**~~
   **SETTLED by spike #1 (2026-07-31) — no new device needed.** `PortConfig::InOut { name, .. }`
   (`vmm/src/resources.rs:124`) already announces `VIRTIO_CONSOLE_PORT_NAME` to the guest
   (`console/device.rs:224-228`), so ~40 lines of limina code exposes the port. Verified on an
   **unmodified** F43 guest: `/dev/virtio-ports/com.redhat.spice.0` appears, the stock udev rule sets
   `SYSTEMD_WANTS=spice-vdagentd.socket`, `vdagentd` starts and opens the port, and it **answered our
   `VD_AGENT_ANNOUNCE_CAPABILITIES`** — protocol round-trip proven with zero guest components.
   (Caveat: vdagentd is socket-activated by the *session* agent, so it needs a graphical session —
   headless it exits with `Error getting active session`.)
   **What replaced this task — a guest-triggerable VMM panic on port reopen — is FIXED**
   (libkrun 0125). The console device `take()`d the port queues on `PORT_OPEN` and never returned
   them on close, so the *second* open of any port aborted the worker (`port rx queue should exist`)
   and killed the VM — `systemctl restart spice-vdagentd`, a package update, or a plain `dd` on
   `/dev/vportNpM`, and it hit `hvc0` and the `krun-std*` ports too. Fixed RED-first
   (`crates/limina-test/tests/l1_port_reopen.rs` + `limina.port_reopen` in the L1 init) and
   re-validated on the real thing: two `vdagentd` restarts now leave the VM healthy and the port
   still round-trips. Still worth upstreaming — it is not a SPICE bug.
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
4. **Arbitration with the native path — SPICE is the DEFAULT, native is the per-session patch
   (user-decided 2026-08-01; this reverses the earlier "native always wins", which was also wrong
   about the granularity).** vdagent is stock and free, so where it works we should just use it and
   stop carrying the expensive alternative. Where it *doesn't* work, the native helper plugs the hole
   — and that is a **per-session** question, not a per-VM one: different logind sessions on one guest
   run different compositors with different capabilities (verified on dogfood-guest 2026-08-01: `kov` on
   gnome-shell, `gsrs` on niri, same VM).
   - **The dependency is XWayland.** F44's `spice-vdagent-0.23.0` links `libX11.so.6` and no Wayland
     library — its clipboard is X11-only (`src/vdagent/x11.c`), so on Wayland it rides XWayland +
     mutter's X11↔Wayland selection bridging, exactly as Boxes does. No XWayland ⇒ no SPICE clipboard
     in that session.
   - **CORRECTED 2026-08-15 — do not group compositors by capability.** This bullet used to claim
     "the compositors vdagent *can't* serve (niri, wlroots, KDE) are the ones that ship
     **ext-data-control**", so the tiers fit together neatly. That is two independent properties
     welded together, and only one of them was measured. Shipping `ext-data-control` (niri,
     wlroots, KDE — true) says nothing about whether a session has **XWayland with X11↔Wayland
     selection bridging**, which is the actual dependency. KDE Plasma ships XWayland by default and
     bridges selections; so do sway and most wlroots compositors — vdagent very likely serves both.
     The only *verified* gap is **niri**, where the dogfood guest had no XWayland process at all
     (2026-08-01), and niri needs `xwayland-satellite` as a separate component.
     Note the self-contradiction this produced: the very next task says **probe positively, never
     infer from "is XWayland installed"** — and then this bullet inferred coverage from compositor
     *identity*, which is the same error a level up. The tiers may still fit together; we have not
     shown that they do, and the arbitration must rest on a per-session probe either way.
     Retiring the `clipboard@limina` extension was the prize (it is what breaks on GNOME
     updates — see the mutter-left-the-delivery history in `limina-enh-delivery`); DONE
     2026-08-15, #37 step 4.
   - **The decision must be made IN THE GUEST, per session, and merely honored by the host.** The
     facts it needs (which compositor, is there an XWayland, did vdagent actually get the selection)
     exist only inside a session; the host sees one vdagent channel plus N helper connections and
     cannot tell which is which. So each `limina-agent-session` probes its own session and either
     **claims** the clipboard (advertises the cap, takes a quiet tier) or **stands by** because vdagent
     covers it; the host keeps its single `crates/limina/src/clipboard.rs` pasteboard owner and routes
     to the SPICE transport plus whichever peers claimed. No host-side policy.
   - **Probe positively, don't infer.** "Is XWayland installed" is wrong in both directions — GNOME
     starts XWayland **on demand** (none was running on dogfood-guest at all), and an XWayland that
     exists may still not bridge selections (task 5). *Which* positive test to run is settled in task
     5: bind our own backend, because vdagent liveness turned out not to imply vdagent function. Note
     the cost while we are at it: vdagent autostart is itself an X client, so adopting SPICE keeps an
     XWayland resident in every GNOME session that would otherwise never start one.
   - **The wrinkle that does not decompose: `vdagentd` serves only the logind-ACTIVE session.** SPICE
     coverage is therefore per-session *and* time-varying — gsrs is covered while active and uncovered
     the moment you switch to kov. Two options, and the choice is deliberate: (a) the helper watches
     logind `ActiveSession` and re-claims when its session goes inactive (tighter, but races a session
     switch mid-copy), or (b) **native always claims in inactive sessions and SPICE only ever serves
     the active one** (simpler, and preserves the cross-session copy/paste the user values). Leaning
     (b). Same seam as the "active session" trade-off in `docs/hardening-backlog.md` §Clipboard.
   - **Two-tier floor unchanged:** a stock guest with neither XWayland nor `limina-agent` has no
     clipboard. That is a degraded baseline, not a broken one.
   - **Where the switch is thrown:** capability negotiation, not the port. Expose the port always (it
     must exist at VM start for the udev rule) and answer `VD_AGENT_ANNOUNCE_CAPABILITIES` **without**
     the clipboard bits when no session wants SPICE to have them, so vdagent never grabs and no
     two-owners fight can start. Withdraw/restore **event-driven, exactly once** — never on a timer:
     spike #1 found vdagentd reads every announce as a *new SPICE client* and resets clipboard state,
     which makes a repeating announce a clipboard suppressor (usable as a deliberate withdrawal
     signal, lethal as a heartbeat).
   - **Why not de-duplicate downstream:** the host pasteboard is safe either way (`set_text` records
     the resulting `changeCount`, so a duplicated guest copy cannot echo), but the guest side is not.
     Two clients claiming CLIPBOARD in one session, tied together by mutter's X11↔Wayland bridging,
     fight over ownership and re-report each other's sets as fresh copies. Arbitration therefore has
     to mean *not enabling the SPICE clipboard for that session*, never merging afterwards.
   - Build this **with** task 2, not after it: the announce path is where the decision lives, and
     bolting it on later means writing the clipboard message set twice.

5. **Invert the probe: test our own backend, not vdagent's liveness.** The shipped probe asks "is
   `spice-vdagent` alive in this session" (`guest/limina-agent-session/src/vdagent.rs`) and yields
   when the answer is yes. That is a *liveness* fact standing in for a *function* claim, and the two
   come apart: a vdagent can be running, connected, and completely unable to move a selection, with
   every component reporting healthy. Replace it with a positive test of the thing we can actually
   establish from inside the session — **bind `ext_data_control_manager_v1`** (which
   `guest/limina-agent-session/src/wayland_clip.rs` already does when acquiring a backend) and claim
   when the bind succeeds. Liveness stays as the tiebreak for sessions where our backend is absent.
   - **What broke, measured 2026-08-23 on the jabuticaba deploy** (synoik session, `DISPLAY=:0` via
     `xwayland-satellite` 0.8.2): host→guest-X11 worked in ≤3 s; guest-X11→Wayland, Wayland→X11 and
     guest→host all failed. Everything limina owns was healthy — port present, zero `vdagent:`
     warnings across a 10 MB supervisor log, `vdagentd`/`vdagent` alive, the helper correctly
     yielded. **The break is the X11↔Wayland selection bridge.** satellite binds only
     `wl_data_device` and primary-selection (`src/server/selection.rs:27-37`), never data-control;
     its X11→Wayland push is gated on `last_kb_serial` (`selection.rs:113`), which is written only
     from `wl_keyboard::Enter`/`Key` on a surface resolving to an X window (`src/server/event.rs:864`,
     `:903`). In a session whose apps are all Wayland-native, no X window is ever focused, so the
     serial stays `None`, the push is skipped, and `wl_data_device.selection` is never delivered to
     satellite either. vdagent creates no focusable window, so it is structurally blind there.
   - **The compositor is not the faulty half.** synoik advertises `zwlr_data_control_manager_v1` v2
     *and* `ext_data_control_manager_v1` v1, and notifies data-control clients on every selection
     change (verified with `wl-paste --watch` across three successive copies). Our own backend works
     in exactly the session where vdagent does not.
   - **This does not retire vdagent — it is still the only transport on GNOME.** mutter will not
     ship data-control (mutter#3941, wontfix), and vd_agent MR !57 (a Wayland-native vdagent over
     ext-data-control, open since 2026-04-06) explicitly does not fix GNOME either. The corrected
     coverage model, with the two properties kept separate as the 2026-08-15 correction above
     demands:

     | session | X11↔Wayland bridging | ext-data-control | carried by |
     |---|---|---|---|
     | GNOME / mutter | yes | never | vdagent |
     | KDE, sway, Hyprland, wlroots | yes | yes | **either — they overlap** |
     | niri, synoik (`xwayland-satellite`) | no (focus-gated) | yes | our helper |

   - **UTM has strictly less, so there is nothing to copy.** Both its backends are vdagent-only —
     `VZSpiceAgentPortAttachment` on Virtualization.framework
     (`Configuration/UTMAppleConfigurationVirtualization.swift:192`), spice-gtk's `shareClipboard`
     on QEMU (`Services/UTMSpiceIO.m:103`) — with no data-control path and no fallback agent
     anywhere in the tree. vd_agent issue #26 is a UTM user reporting this exact failure.

6. **The mute we don't have: nothing can tell vdagent to stand down.** The arbitration is one-way by
   construction — the helper yields to vdagent and nothing runs in the other direction.
   `crates/limina/src/vdagent/codec.rs:110` `our_caps()` is a constant announced once per port open
   (`session.rs:124`), and the poller (`crates/limina/src/control.rs:184`) fans every host copy to
   the vdagent transport *and* the claiming peers unconditionally. Today that is safe only because
   the helper always yields first. **Inverting the probe removes that safety**: in the overlap row
   above both transports would serve one session, which is precisely the two-owners fight the
   yield-first design exists to prevent (the host pasteboard is protected by the `changeCount`
   ratchet; the guest side is not).
   - **The primitive already exists upstream, unused by us.** Announcing capabilities with
     `request=1` and **without** `VD_AGENT_CAP_CLIPBOARD_BY_DEMAND` makes `vdagentd` call
     `do_client_disconnect()` (`src/vdagentd/vdagentd.c:257`), broadcast
     `VDAGENTD_CLIENT_DISCONNECTED` (`:169`), and the session agent then calls
     `vdagent_clipboards_release_all()` (`src/vdagent/vdagent.c:256`) — it **drops X selection
     ownership**, it does not merely go quiet. Thereafter `do_agent_clipboard()` short-circuits on
     the missing cap (`vdagentd.c:741`), so no guest grab or request reaches the port at all.
     Re-announcing with the bits restored walks vdagent through the same reconnect and hands the
     clipboard back. Host→guest needs no protocol at all: skip `broker.host_copy()` while muted.
   - So the work is a *muted* state on `Session`, flipped by whether any control peer currently
     announces the `clipboard` cap, plus the poller skipping the vdagent leg while muted. Two
     constraints carry over unchanged: the flip must be **edge-triggered** (a repeating announce is
     a clipboard suppressor, per the announce-once rule above), and the mute is **per-VM while the
     claim is per-session** — the host still cannot map a peer to a guest session, so the guest must
     only claim when it can genuinely serve, which is what task 5's bind-test buys.

**Sequencing (decided 2026-08-23): fix the bridge first, revisit arbitration after.** synoik gains
X11↔Wayland selection bridging on its own side, which restores vdagent in the one session where it
is currently blind and unblocks dogfood without touching limina. Tasks 5 and 6 stay booked and
unstarted; the overlap they create is only worth paying for once a session needs both transports.

**libkrun patches:** the virtio-serial named-multiport device (task 1) — the one real patch. Broker +
clipboard bridge are pure limina code.

**Done test:** boot an **unmodified** stock Fedora image (no `install-enhanced.sh`, no `limina-agent`)
under limina; `spice-vdagentd` comes up against `/dev/virtio-ports/com.redhat.spice.0`; copy text in
the host and paste it in the guest and vice-versa. Baseline-tier compatibility floor (L2) stays green.
**Second done test (task 4, the mixed guest):** one guest running two sessions on different
compositors — GNOME (XWayland-capable, SPICE-covered) and niri (no XWayland observed in that
session, ext-data-control) —
copies and pastes in **both**, with exactly one clipboard owner per session and no ownership
ping-pong. dogfood-guest is already shaped like this, so it is a real configuration, not a contrived one.

**Risks / spike first:**
- ~~**Spike #1 (gating):** does exposing a named `com.redhat.spice.0` virtio-serial port actually wake
  stock `vdagentd`…~~ **DONE 2026-07-31, GREEN** — `spikes/m12-spice-port/RESULTS.md`. Yes, and the
  libkrun device change is zero (the port-reopen panic above is what needs fixing instead).
- ~~**Wayland reality.**~~ **CLOSED 2026-07-31 — the guest clipboard reaches us.** A copy in a
  GNOME/**Wayland** session produced a real `VD_AGENT_CLIPBOARD_GRAB` (selection=CLIPBOARD,
  types=[UTF8_TEXT]) on the host. F43's `spice-vdagent` 0.23.0 clipboard is X11-only (links
  `libX11`, `src/vdagent/x11.c`), so it rides XWayland + mutter's X11↔Wayland selection bridging —
  exactly how Boxes ships it. **Two traps produced false negatives first** (see the spike RESULTS):
  `wl-copy` over ssh silently does *not* set the clipboard (an unfocused Wayland client has no
  input serial for `set_selection`; verify with `wl-paste`, and use `xclip` on `DISPLAY=:0`
  instead), and a *repeating* announce timer reads to `vdagentd` as a new client connecting every
  tick, resetting clipboard state — **the broker must announce once per port open, not on a
  timer.**
- ~~**Confirm the default-install assumption on the actual base image.**~~ **DONE 2026-07-17** — a
  stock F43 boot confirmed `spice-vdagent-0.23.0-1.fc43` present + dormant + udev-triggered on the
  named port (see the load-bearing facts above). The "$0 guest install" case holds.
- **Overlap with M5 is intentional but must not double-own the pasteboard** — one host clipboard
  owner, two possible guest transports.

---

## Milestone 12.5 — QEMU guest-agent support (stock tier; clock first)

**Status: 🟢 steps 1–2 shipped — the port, the client, the guest clock, the shutdown rung and the
guest inventory.** Steps 3–5 below are not started.

**Goal:** expose `org.qemu.guest_agent.0` and speak to the `qemu-guest-agent` that a stock guest
*already has*, the same additive on-ramp M12 built for SPICE. `limina-agent` stays the enhanced-tier
path; this is what a guest that never installs our components gets.

### Load-bearing facts (measured 2026-08-26, in a booted F44 enhanced guest)

- **The package ships by default.** Fedora's comps put `qemu-guest-agent` (with `spice-vdagent` and
  `spice-webdavd`) in `guest-desktop-agents`, all **mandatory**, and that group is in the *mandatory*
  grouplist of `workstation-product-environment` and of every other desktop environment. Server and
  Cloud get `guest-agents` as an **optional** group. Ubuntu desktop ships `spice-vdagent` as a hard
  dependency of `ubuntu-desktop`. **Debian ships neither by default** — not in `task-desktop`, not in
  `gnome`, not in the official cloud images — so the fallback is simply absent there.
- **The trigger is the port name**, not a DMI name table: `99-qemu-guest-agent.rules` matches
  `SUBSYSTEM=="virtio-ports", ATTR{name}=="org.qemu.guest_agent.0"`, and the unit is
  `BindsTo=dev-virtio\x2dports-org.qemu.guest_agent.0.device`. Confirmed: exposing the port on an
  otherwise untouched guest brought `qemu-guest-agent-10.2.2-1.fc44` up on its own.
- **Fedora blocks nothing.** `/etc/sysconfig/qemu-ga` ships with its `--block-rpcs` line commented
  out; the guest answered `guest-info` with **43 commands, every one `enabled`** — `guest-exec`,
  `guest-file-*`, `guest-set-user-password` and the ssh-key commands included, all as root. The
  trust boundary is therefore ours: only the supervisor process holds the host end of the port.
- **`guest-set-time` works against our PL031.** The agent sets `CLOCK_REALTIME` and then writes the
  RTC; libkrun's `RTCLR` write arm stores it as an offset from host wallclock, so nothing rejects it.
  Measured: a guest shoved 7200.3 s back was stepped to the host's clock within one tick, and again
  after `systemctl restart qemu-guest-agent` (the port-reopen path, `l1_port_reopen.rs`).

### Why the clock came first

A stock guest that **stays running** across a host nap, or that ignores its RTC, had no corrector at
all: the RTC path needs the guest kernel to consult it (s2idle thaw), our `TimeSync` needs
`limina-agent`, and libkrun's vsock port-123 timesync has no consumer in our guests. The fallback
rides the existing `limina-timesync` thread, which already carries both triggers — the oversleep
detector (the host napped) and the periodic tick (drift) — and fires **only when no
`timesync`-capable peer took the message**, so the enhanced tier always wins.

### Shape

- Worker: `console::attach_named_port` puts both agent ports on the bus; the supervisor makes the
  socketpair per spawn (`--qga-fd`), exactly like `--spice-fd`, so the guest's device topology never
  depends on how the VM was launched.
- Supervisor: `crates/limina/src/qga/` — `codec` (pure line/sentinel framing), `client` (one
  outstanding call under a mutex; `guest-sync-delimited` is the only resync; timeout classes and a
  `fire` variant for the commands that answer nothing), `policy` (pure: when to step a clock).
  `guest-info`'s per-command `enabled` flag is the capability gate for everything below.

### Step 2 — lifecycle + inventory (shipped)

- **The stop ladder gained a rung, after the power button**: agent (5 s) → GPIO power button
  (5 s) → `guest-shutdown` → wait. It only comes up when a probe
  has already succeeded (probing inside a stop ladder would eat the grace on a guest that has no
  agent) and when the grace can still hold it, so a tight `--shutdown-grace-secs` — the test
  harness uses 3 s — behaves exactly as before. Worth having despite the button: the agent runs
  `shutdown -P` as **root**, which goes through even where the button does not. The windowed close
  path climbs the same ladder, where a stock guest previously went straight to SIGKILL.
- **A guest that accepts the request gets more time** (`QGA_GRACE`, 45 s). Measured on a seated F44
  desktop, 2026-08-26: `shutdown -P +0` needs ~28 s from request to the VM being gone — it is
  running the real systemd teardown, not just calling poweroff. The operator's grace is about a
  guest that will not *answer*; one that just took a shutdown request is answering, and killing it
  mid-teardown is how filesystems get hurt. A second stop signal still forces immediately.
- **Inventory is logged once**, when the agent first answers: OS pretty-name + kernel + machine +
  hostname, non-loopback IPv4 addresses, logged-in users, and a second line of mounted filesystems
  with used/total. Log-only by choice — the surface is an incident report that can say what the
  guest *was*. Every command is optional; a blocked or missing one contributes nothing.
- **An ordinary stop never kills the guest.** Every rung is a *request*; a guest that ignores all of
  them keeps running, and the supervisor says so once (`the guest has not powered off … and is still
  running`). The grace is a **reporting** deadline, not a kill deadline. Ending a guest that will not
  stop is an explicit human act: a second stop signal (double Ctrl-C, `limina stop --force`) or Force
  Stop in the window's menu. A timer-driven SIGKILL costs unsaved work the user never agreed to risk
  — and it fired most readily on exactly the guest most likely to have work open, the seated desktop
  holding logind inhibitors. `l1_stop_never_kills` pins it.

### Next steps

3. **Storage integrity** — `guest-fsfreeze-freeze`/`-thaw` inside the snapshot bracket (app-consistent
   snapshots, including the guest's own `/etc/qemu-ga/fsfreeze-hook.d` scripts), `guest-fstrim`
   alongside reclaim.
4. **Provisioning** — `guest-exec`, `guest-file-*`, `guest-ssh-add-authorized-keys`: deliver the
   enhanced tier into a stock guest without needing SSH first, which is the bootstrap floor the
   two-tier guarantee asks for.
5. **Telemetry / hotplug ack** — `guest-get-diskstats`/`-cpustats`/`-load`; `guest-get/set-vcpus` and
   the memory-block commands only if CPU/memory hotplug ever lands (ballooning covers memory today).

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

**Status: 🟢 BOTH halves shipped — passkeys and the fingerprint reader.** The stock-tier wave
completed: the emulated xHCI controller plus the FIDO gadget *and* the impersonated MOC
(elanmoc) fingerprint reader are default-on since `f9646d0`, so a stock guest gets both with no
guest components installed (`docs/design/usb-moc-fingerprint.md`, `docs/fingerprint-reader.md`).
What remains is an L2 FIDO guard with a test-only Touch-ID bypass, payload/app-bundle
delivery polish, and the host/guest shared-identity follow-up booked at the end of this section.
Original CTAP2-core detail, still accurate:

**CTAP2 core GREEN end-to-end (2026-07-24, `spikes/touchid-fido/RESULTS.md`). Spikes A
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
   `com.apple.developer.web-browser.public-key-credential` entitlement is NOT needed — it gates
   asking *macOS's own* authenticator for credentials on arbitrary relying parties, which is a
   different design (and the one the host/guest-shared-identity follow-up below would need).
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
4. ✅ **Productize (mostly done 2026-07-24):** per-VM store wired to the bundle dir
   (`<bundle>/fido-credentials.json`); app-bundle ships `liblimina_sep.dylib` in Frameworks with
   the supervisor rpath (`build-app.sh`); agent bumped to 0.3.0; browser oracle GREEN (webauthn.io
   in guest Firefox); `pam_u2f` recipe verified + documented (`docs/fido-authenticator.md`); both
   F44 enhanced images refreshed to agent 0.3.0 (agent-only pass + reclone). **Remaining:** an L2
   FIDO guard (fido2-assert round-trip with a test-only Touch-ID bypass).
5. **PAM recipe** + L2 guard (`fido2-assert` round-trip; test-only auto-approve knob for CI since
   the prompt needs a finger).
6. **Stock wave — FIDO half ✅ done (2026-07-24):** the emulated xHCI controller (patches
   0095–0097) + the **FIDO gadget** (patch 0098 `HidReportPipe` mechanism, wired to the CTAP2/SEP
   authenticator as policy in a **proxy** split — worker gadget = thin CTAPHID transport over a
   UNIX socket to the supervisor's one `FidoAuthenticator`/store/keepalive engine). A stock guest
   with `limina --usb` binds it as `/dev/hidrawN` (usage page 0xF1D0) with zero guest components.
   Guarded by `l1_xhci_fido_authenticator` (INIT + getInfo, presence-free); Touch-ID credential
   flows (fido2-cred/browser/pam_u2f) are the manual follow-up (same SEP path as the verified uhid
   transport). **Fingerprint half ✅ shipped:** the impersonated MOC (elanmoc) gadget landed with
   its own design doc (`docs/design/usb-moc-fingerprint.md`; commits `3c4eaa4`, `5eb6ab0`,
   `4124f90`, `f9646d0`, `e32d204`) and a user doc, `docs/fingerprint-reader.md`; USB + fingerprint
   are default-on since `f9646d0`. Controller design: `docs/design/usb-xhci.md`.

**Done test:** on a **stock** F44 guest (USB build): Firefox registers + asserts a passkey on
webauthn.io with Touch ID prompts appearing on the host; GNOME Settings shows Fingerprint Login;
enrollment + GDM/sudo fingerprint work via host Touch ID. On the **enhanced** image (uhid build):
the same passkey flow, plus the L2 guard green.

**Risks / spike first:** MOC protocol fidelity (some drivers do pairing/PSK — choose the target
by protocol simplicity); fwupd's actual probe behavior vs the version bluff; enroll-stage UX
(each "touch" = a host prompt); LAContext prompt from all launch modes (terminal proven; Dock
launch to verify — cf. the fd-limit and TCC launch-env traps); multi-VM prompt attribution.

### Follow-up: one passkey identity across host and guest (📋 booked 2026-08-17)

**Wanted:** guest Firefox signs in with the *same* passkey as host Firefox, so a VM is not a
separate identity — and **per-VM configurable**, because plenty of users want the guest's
credentials deliberately untied from the host's. Design the switch in from the start
(`vm.toml`, alongside the other hardware toggles); today's behavior is the isolated end of it.

**The obvious route is closed, and the reason generalizes.** Sharing the host's passkeys means
asking macOS's own platform authenticator for them (AuthenticationServices), which also happens to
be the only way to get Apple-rooted attestation. Its RP identifier must be a domain the app is
associated with, and the escape hatch for arbitrary RP identifiers is
`com.apple.developer.web-browser.public-key-credential` (macOS 13.3+) — a **managed capability
Apple grants only to web browsers**: the Account Holder applies, and the app must declare the
HTTP/HTTPS schemes, offer a URL field / search / bookmarks on launch, and render the destination's
own web content. A VM meets none of that. (This also settles the neighbouring question: **we cannot
relay Apple attestation.** `SecKeyCreateAttestation` is absent from the public macOS SDK entirely,
and App Attest (`DCAppAttestService`, macOS 11+) attests only a key *it* generates and binds our own
App ID, not the guest site's RP identifier — verified against the SDK headers, not recalled.)

**The route we control inverts the direction:** rather than the guest borrowing the host's
credentials, present *limina's* enclave-backed authenticator to the **host** as well, so both sides
share one store and one identity. Host browsers only speak to real HID devices, so this needs a
virtual USB/HID device on macOS — DriverKit (`com.apple.developer.driverkit.transport.hid`), itself
an entitlement, but one granted to ordinary device developers rather than to browsers only.
**Unverified**: whether a DriverKit HID device is reachable by browsers as a security key, and what
that entitlement actually requires. Spike that before promising the feature.

Sharing then makes the per-VM switch load-bearing rather than cosmetic: one store means a guest
compromise can *ask* for signatures on host credentials (each still gated by a Touch ID sheet whose
reason string must name the asking VM — see the multi-VM prompt-attribution risk above).

---

## Milestone 15 — Virtual display pipeline v2: native refresh, hardware planes, scanout modifiers

**Status: 🚧 in flight — Wave 1 Parts 1+2 SHIPPED 2026-07-31; Wave 4 spike CLOSED** (`0f4a9d3`,
`02e13ce` → the XBGR/ABGR partial win shipped as `patches/linux/0006`; non-LINEAR buys nothing,
render-direct-to-LINEAR is the win). Remaining: wave 1 per-host-display + VRR, waves 2–3.
(Booked 2026-07-30 from the compositor-side asks in
`dogfood-guest:Projects/gnome-shell-rs/docs/fork/present-misses.md` §30–§31; scanout-modifier spike
started same day.) The present-miss investigation closed with GPU frame *cost* as the one lever
(their §30: with the instrument off the frame path, misses are 0.00% at 60 Hz), and the guest side
is now targeting 120 Hz (8.33 ms) / 144 Hz (6.94 ms) budgets. These waves are the host-side half
of that: give the guest real refresh targets, cheaper scanout, and hardware planes for video.

Context that shapes all of it: fence feedback is now **truthful end-to-end** — the fence-present
chain is default-on, §28's Bug A/B are fixed, and the shown-ack split (c33d9a0) separates
"new frame at glass" (fence) from "old buffer free" (release). The old "guest hrtimer grid"
framing of flip completion is obsolete: the guest's flip completion rides the real CA latch, so
exposing real display timing to the guest is now meaningful rather than cosmetic.

### Wave 1 — per-hardware-display virtual displays with native refresh (incl. VRR)

**Part 1 (stable EDID identity + connector events) ✅ SHIPPED 2026-07-31** — libkrun `0119`-`0121`,
`crates/limina-displayctl`, `crates/limina/src/window/hostdisplay.rs`, L1-verified by
`crates/limina-test/tests/l1_edid.rs`. The guest now gets the *identity* (vendor/product/serial/
name from the panel's own EDID numbers), density and refresh of the host display the window is on,
re-pushed when the window migrates; the range descriptor lands in the exact form
`drm_get_monitor_range` accepts. Real connector disconnect/reconnect works too (it's the scanout's
`enabled` flag, which was hardcoded true). Design + what's still open (windowed human verification,
boot-time EDID, a fuller DisplayID mode list, `vrr_capable`
plumbing): `docs/design/stable-edid-hotplug.md`.

**Part 2 (DisplayID extension + HiDPI) ✅ SHIPPED 2026-07-31** — libkrun `0123`. A base EDID
detailed timing tops out at 655.35 MHz of pixel clock, so a Retina panel at device pixels
(3024x1964 @ 120 Hz, ~866 MHz) could not be advertised honestly; a **DisplayID 2.0 type VII**
block now carries the real timing alongside the clamped base one. (A *CTA-861* block would not
have worked — its detailed timings share the base block's 16-bit clock field.) On top of that,
`[display] hidpi` (default on, `--no-hidpi` opts out) drives the guest at the host display's
**device pixels** instead of its points, so a 2x panel renders natively and the guest picks the
2x scale itself rather than having Core Animation upscale a half-resolution scanout.


One virtual display per host display, each advertising **what that panel actually supports**:
120 Hz ProMotion/VRR on the dev/dogfood MacBook panels, 60 Hz on external monitors. The mode
list/EDID is ours (krun-display, libkrun patches); the honest-pacing prerequisite already shipped.
Guest side confirms there is no 120/144 Hz mode today and flags that the frame-budget work
cannot be acceptance-tested without one — this wave unblocks it. VRR/ProMotion is the interesting
design half: CA latch cadence is variable, and the guest should see that (EDID VRR range /
`vrr_capable`) rather than a fixed grid. Rides on the per-hw-display work already planned for
multi-display (M8 remainder).

**Part 3 (more than one virtual display) ✅ SHIPPED 2026-08-18** — a boot-time pool of scanouts
(`--display-pool`, default 4 windowed), a slot table where **a host panel owns a connector**
stably by `panel_key` (so GNOME's saved per-monitor configuration still matches after a
migration), fullscreen across every attached panel, and a Displays menu that switches a panel
out of the guest's arrangement. `crates/limina/src/window/displays.rs` holds the policy;
`docs/graphics.md` has the model and the traps.

**Part 4 (arrangement relay) ✅ SHIPPED 2026-08-19** — the guest is told *where* each connector
sits, so a guest with no saved configuration comes up laid out like the host instead of
left-to-right by accident. The rect that `virtio_gpu_resp_display_info` always carried now
flows end to end: limina computes guest-desktop positions from the host arrangement
(`crates/limina/src/window/arrangement.rs` — structure detected in point space, metric rebuilt
in predicted logical units, and the whole set re-validated against mutter's own
adjacency/overlap rules: emit a clean full set or nothing), libkrun carries them
(`DisplayInfo::position` → `GET_DISPLAY_INFO` r.x/r.y), and the enhanced kernel exposes them as
the DRM `suggested X`/`suggested Y` connector properties **plus `hotplug_mode_update`**, which
compositors gate the offsets on (mutter returns early without it — the offsets alone are
silently ignored). Enhanced tier; stock guests unaffected. Three ordering/timing rules were
load-bearing on the rig: the whole suggested set must be in place *before* the connect's
hotplug (in-place moves are sent first, positions ride the connect); one config-change event
carries every update that can share it, and the device holds further events until the guest
acks (the ack race otherwise loses the second event — regression:
`l1_multidisplay::l1_b_back_to_back_updates_survive_the_ack_race`, which needs the 7.1.8+
guest driver where the window exists); and a position is never pushed to a slot already at
the device default. Rig-verified 2026-08-19: fractional-scale BenQ at (1512, 0), hidpi
built-in at (0, 747), matching the host, user-confirmed at the seam. The metric is
self-correcting: where the guest picks a scale the host prediction can't see (fractional on
a fixed-mode connector), the agent's reported logical rects replace the predicted sizes
before the walk (`arrangement::correct_metric`, design in
`docs/design/arrangement-relay.md`). When corrections take effect is mutter's call — mutter
≥ 50 re-applies an existing config over any in-place suggested change, so corrected offsets
land at a set's first appearance or the next seat, never mid-session (verified end to end;
`l2_arrangement.rs` pins the precedence and the seat-time apply).

### Wave 2 — overlay planes on virtio-gpu (protocol extension, both sides)

virtio-gpu KMS exposes primary+cursor only; overlay planes need a device+guest-kernel protocol
extension — we own both ends (enhanced kernel + libkrun/virgl), and the mechanism is
upstreamable. Host side is genuinely cheap: a plane maps naturally onto a CALayer, which is the
compositing our window already does. Feature-flag gated / capset-negotiated: stock guests keep
today's two planes (two-tier guarantee holds). The guest consumer already exists (their
`tty.rs` implements primary/overlay/cursor direct scanout with nothing to bind to).

Fold into the same protocol extension: **device-advertised plane formats/modifiers.** The
guest driver hardcodes its plane format list (`virtgpu_plane.c`) — there is no virtio-gpu
mechanism for the device to say what it can scan out, which is why every format we enable
(ARGB8888 = linux 0002, XBGR/ABGR = 0006, 10-bit/fp16 later) costs a kernel patch. A
device→driver format/modifier query makes the hardcoded list the fallback and future formats
config, not patches — genuine virtio-spec/dri-devel material, and overlay planes need new
device→driver plumbing anyway.

### Wave 3 — NV12/P010 + `COLOR_ENCODING`/`COLOR_RANGE` on those planes

Bundled with wave 2 rather than planes-for-RGBA alone: the payoff is video (scanning out decoder
output with no conversion pass), and CA scans out biplanar YUV IOSurfaces natively. Schedule
coupled to the VA-API/Vulkan Video work — the guest side is starting multi-planar sampling in
their renderer ahead of it either way. Expectation set on both sides: overlay planes are an
adjunct for video/fullscreen, not a general compositing speedup.

### Wave 4 — non-LINEAR scanout on the primary plane (spike first — running now)

The highest-leverage ask against the guest's frame budget: LINEAR-only primary is why the
compositor shadow-renders and pays a full-damage 4K present blit + RGBA→BGRA reorder inside every
frame's GPU bracket. Spike = `spikes/scanout-modifiers/`. Questions it must answer:
(a) can the guest render **directly into** the scanout buffer — note CAMetalLayer drawables are
IOSurface-backed render targets, so "renderable linear/IOSurface scanout" may be the real win and
tiled modifiers moot; (b) does KK/venus plumbing support `VK_EXT_image_drm_format_modifier` or
renderable-LINEAR feature bits today, and what's missing; (c) where the LINEAR-only advertisement
comes from (guest KMS vs our device) and what modifier negotiation needs; (d) the cheap partial
win regardless: advertise `XBGR8888`/`ABGR8888` on the primary plane so the swizzle half of the
blit dies (host Metal/IOSurface handles either byte order for free). A "no" on (a)/(b) is a
useful answer — it makes the guest's blit permanent and turns it into their optimisation target.

### Wave 5 — small/parked items

- **Cursor larger than 64×64** (HiDPI + accessibility sizes): protocol-size lift, we own both
  sides — **booked low-priority**; their bigger cursor win is guest-side anyway (setting
  `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` to leave the software cursor, which also kills the
  §26 pointer-motion repaint storm).
- **Price WindowServer's GPU share while the guest composites 4K** — profiling task
  (Metal counters / `powermetrics`, see `docs/` profiling playbook); discriminates their
  leading candidate for the ~1.5× frame-time spread. Cheap for us, decisive for them.
- ✅ **already done, recorded for the ledger:** the §22 never-signaling-fence-on-context-death
  wedge is fixed (libkrun 0117, `f809201`: refused context fences retire as lost, plus the
  `venus_fence_lost` L2 guard) — the guest-side doc's "queued as a host-side fix" predates it.

### Wave 6 — zero-copy udmabuf (guest-RAM dmabuf) import into venus

**Status: phase 1 ✅ shipped, phases 2–3 📋 planned** (raised by the user while fixing the totem
crash: *"is it really impossible to import udmabuf, or is it a matter of giving venus a new
primitive?"* — the answer is the second, and it needed GUEST-side work first).

**What a udmabuf is and why it matters.** `/dev/udmabuf` wraps plain guest anonymous pages (a
sealed memfd) as a dmabuf. It is how every *software*-decoded media frame reaches the GPU stack:
GStreamer allocates into a memfd, wraps it, and hands the dmabuf to zink/GL. The GL path now works
(phase 1) at the cost of one host-side copy per frame; venus still refuses the import, and
removing that last copy is what phases 2–3 are for.

**The import does reach the host.** A PRIME-imported udmabuf arrives as a guest-memory blob whose
pages libkrun translates into host-VA iovecs — measured `[BLOB-CREATE] ctx 0 res 293 blob_mem=1
blob_flags=0x2 size=3686400 -> 225 iovec(s), 3686400 bytes`. (Until 2026-08-23 it did not, and the
resulting *untyped* host resource made `CREATE_SAMPLER_VIEW` fail and poisoned the player's GL
context permanently — the fix chain is phase 1 below.)

Three phases, each independently useful:

1. **✅ Make the import reach the host, and make GL sample it.** Four fixes, one chain:
   - guest kernel (`liminavm/linux`) — record `blob_mem` on a PRIME-imported object so
     `RESOURCE_INFO` reports it, and give the dma-buf GEM funcs `.open`/`.close` so the resource
     is attached to the render context;
   - guest mesa (`limina-guest`) — report `blob_mem` for a resource found in the winsys cache
     (every plane after the first of a multi-planar dma-buf, and, because planes import in reverse
     order, plane 0 — the only one allowed to emit `SET_TYPE`);
   - guest mesa — let planar YUV reach the sampler bitmask, so the composite format is used
     instead of one host resource per plane;
   - virglrenderer (`limina`) — type the blob into a real GL texture, fill it from the guest's own
     pages (macOS has no dmabuf to alias, so we copy) and re-read before every command batch that
     samples it; planar YUV is converted to RGBA on the way in.

   Frames therefore reach the GPU with **one host-side copy** and no guest-side one.
2. **Host (virglrenderer `limina`) — consume an iov-backed classic resource in venus.** libkrun
   already translates the backing into **host-VA iovecs** (`virtio_gpu.rs` `attach_backing` →
   `virgl_renderer_resource_attach_iov`), so the pages are visible in the worker; what is missing
   is a `proxy_context_attach_resource` path for "iov-backed, no IOSurface, no map_ptr". Stitch the
   scattered pages into ONE contiguous host VA with `mach_vm_remap` (share, don't copy), then
   import through the existing host-pointer route (`VK_EXT_external_memory_host` → KK →
   `newBufferWithBytesNoCopy`). Contiguity — not visibility — is the only reason a single
   MTLBuffer can't span the pages today.
3. **Pinning + allocation-granularity policy (the part expected to bite).** `mach_vm_remap` works at
   the host's **16 KiB** granularity — but **guest page size is the wrong variable here** (corrected
   2026-08-15; the earlier text said a stock 4 KiB guest "stays on the fallback"). What a remap
   needs is that each 16 KiB-aligned *buffer offset* sit on 16 KiB-aligned, physically-contiguous
   backing. A 16k guest gets that because every page is such a quad, but that is sufficient, not
   necessary: buddy-order-2 allocations are naturally aligned, and shmem large folios / hugetlb
   memfds give 2 MiB runs — udmabuf already pins via `memfd_pin_folios()` and accepts
   `shmem_file() || is_file_hugepages()`. So a THP-backed memfd on a stock 4 KiB guest stitches as
   well as an enhanced one. Make the granularity a **hint** on the phase-1 guest path, and make the
   host scan the iov list and hybridise — remap the qualifying 16 KiB slots (`VM_FLAGS_OVERWRITE`),
   `memcpy` only the ragged remainder — so degradation is proportional rather than binary. Caveat:
   a copied slot is a snapshot, not shared storage, so that hybrid is sound for write-once frames
   and needs a per-frame re-copy (or a fully-remappable gate) for buffers the guest rewrites in
   place. Rationale and derivation: `docs/design/16k-page-requirement.md`. While Metal has the pages wired they
   must not move or be reclaimed, so the balloon / free-page-reporting path has to treat them as
   pinned for the resource's lifetime — reconcile with `docs/design/m6-dynamic-memory.md` before
   writing any of phase 2. Coherency is free (UMA, same physical pages as the existing host-pointer
   imports).

**Done test:** with a 16k enhanced guest, `vkudmabufimport.py` reports `IMPORT OK` + `ALIAS OK`
(the pattern written into the memfd read back through the imported venus memory via a GPU copy —
proof the two views share storage, not just that the call returned), a udmabuf frame reaches the
GPU with **zero** guest-side copies, and a measured before/after on software-decoded playback
(`gpu p50` + CPU per frame). A stock 4k guest **on a THP/hugetlb-backed memfd should reach the same
zero-copy result** — test it explicitly rather than assuming the fallback; a 4k guest whose backing
is genuinely fragmented exercises the hybrid path (proportional copy) and must still play cleanly.

**Done test:** wave 1 — a seated guest on the dogfood/dev panel enumerates a 120 Hz (VRR-capable)
mode, runs at it, and the guest frame clock tracks real latches; wave 2/3 — a fullscreen NV12
surface reaches a CALayer with no guest-side conversion pass, stock guest unaffected; wave 4 —
spike RESULTS.md answers (a)–(d) with measurements, and whichever of direct-render/modifier/format
wins ships with a before/after on the guest's heavy-band `gpu p50`.

---

## Milestone 16 — LiminaOS: a purpose-built guest distribution + system compositor (moonshot)

**Status: 🔨 distro prototype under way; compositor phase still 💭 unscheduled.** The boot chain is
proven on real firmware; the image model below is built and boots.

**Plan of record for build detail is `~/Projects/LiminaOS/README.md` on the LiminaOS build VM**, not
this section. This section is the design memo and the durable record of *decisions*; where the two
disagree on build detail, the README wins.

**Goal:** our own image-based guest distro on the GNOME OS strategy — **BuildStream** builds on
the **freedesktop-sdk** base, **systemd-sysupdate** A/B image updates (not OSTree, not packages)
— built solely for Apple Silicon guests (aarch64, 16 KiB-page-clean userspace, venus-first), with
**Plymouth and GDM replaced by one Wayland system compositor** that owns the display from early
boot to shutdown. LiminaOS becomes the first-class enhanced guest and eventually replaces the
RPMs-over-Fedora enhanced delivery; the **two-tier guarantee is untouched** — stock Fedora keeps
booting, this is the top tier, never the entry fee.

### The system compositor (the heart of it)

One Wayland compositor, started as early in boot as possible, running **unprivileged**, that is
the *only* display owner for the machine's lifetime. No VTs, no KMS-master handoff, no
Plymouth→GDM→session flicker chain. User sessions run **our session compositor**
(the gnome-shell/mutter replacement) as its sole Wayland client, doing unredirected fullscreen
**scanout passthrough** — the session's buffer flips straight to the primary plane. Boot splash,
login, logout, lock, session switch all become one continuous visual timeline the system
compositor animates.

- **Prior art that makes this credible:** Wayland's *original* architecture (system compositor
  hosting session compositors — dropped by desktops, fine in constrained environments, and a VM
  guest is exactly that); **gamescope** (production proof of the nested-host + fullscreen-client
  direct-scanout model, and of its core trap: forward client commits without imposing your own
  frame clock or you add a frame of latency); **ChromeOS** (production proof of no-VTs, with a
  minimal recovery console — our equivalents are serial + ssh + limina's console paths).
- **Explicit non-goal: hosting stock mutter/GNOME nested.** The session compositor is ours, so
  the system↔session protocol is **private and versioned in lockstep** — passthrough negotiation,
  animation handoff, and everything a session compositor normally gets from KMS directly
  (gamma/color, VRR, mode setting, DPMS) becomes protocol between two components we both own.
  Stock guests get the stock Fedora path; they never meet this compositor.
- **Privilege split:** the compositor gets the DRM master fd handed to it once (udev uaccess tag
  / seatd / logind `TakeControl` on a VT-less seat — one line of policy). Session *tracking*
  stays logind + pam_systemd (XDG_RUNTIME_DIR, polkit, ACLs); session *launching* — GDM's actual
  job — is a small greetd-shaped privileged helper (PAM auth + spawn as user), same pattern as
  `docs/design/privileged-helper.md`. Security win worth naming: the entire user session runs
  with **no /dev/dri master and no /dev/input access at all**.
- **Passthrough constraints on our stack:** the session's buffers must be LINEAR dmabufs (KK
  modifier support is LINEAR-only — see M15 wave 4; render-direct-to-LINEAR is the win there
  too, so the constraints compose).
- **Host-side splash handoff — a lever no bare-metal distro has:** the limina window presents
  its own boot visual instantly, before the guest produces a frame, and cross-fades to the
  system compositor's first frame. Perceived boot is seamless regardless of how early the guest
  compositor truly starts — which also means it can be an ordinary early systemd unit; the
  Plymouth-style initrd/survive-switch-root trick is unnecessary (see boot chain below).
- **Sole-display-owner obligations:** with every fallback display path deleted, compositor
  failure needs deliberate design — systemd respawn policy, `sd_notify` watchdog, and serial as
  the only oracle when it's down. ChromeOS accepted the same trade; it's a decision, not a
  default.

### Boot chain — every stage ours, no pivots

**KRUN_EFI → systemd-boot → UKI (kernel + tiny initrd + cmdline) → verity `/usr` → systemd →
system compositor.**

**This chain is proven on real KRUN_EFI**, booting to a login prompt in ~10s on our own 16 KiB
kernel, through verity `/usr` and switch-root. systemd-boot loads from
`ESP:/EFI/BOOT/BOOTAA64.EFI` (the removable path). Two properties worth stating because they are
easy to assume otherwise: **KRUN_EFI's ESP is writable**, so systemd-boot's boot-count rename
reaches the disk; and the firmware falls through to PXE **with no error message at all** when it
cannot read the ESP, so a silent boot failure means "firmware could not read the ESP", not "no
bootloader".

- **No simpledrm, no framebuffer inheritance:** in a VM, virtio-gpu exists from cycle zero —
  build it in, turn `CONFIG_SYSFB_SIMPLEFB` off, and `/dev/dri/card0` is there before PID 1
  moves. The simpledrm→virtio-gpu handoff (the fiddliest part of "replace Plymouth" on bare
  metal) simply doesn't exist. Consider `CONFIG_VT=n` outright: no fbcon, nothing to fight for
  the DRM device on panic. Text (kmsg, emergency shell, rescue) lives on serial `ttyAMA0` — and
  the Plymouth-details-mode trap that `console=ttyAMA0` causes today dies with Plymouth.
- **A tiny initrd, inside the UKI — and do not try to remove it.** The obvious simplification is
  the ChromeOS-style no-initrd boot (`CONFIG_DM_INIT` + `dm-mod.create=` verity on the whole root),
  and it does not work here: verity protects **`/usr`**, not the whole root, and mounting a verity
  `/usr` before PID 1 on a merged-usr system requires early userspace. What the initrd buys is
  systemd's paved paths — `systemd-veritysetup-generator`, sysext-in-initrd, repart-on-first-boot,
  credentials — instead of a road nobody else walks. It is still **one signed, measurable UKI
  object** for sysupdate to replace, so nothing in the update or rollback story changes.
  - **Keep it genuinely tiny — `dracut --no-kernel`.** Everything needed before `/usr` is `=y` by
    our own build-it-in rule, so the initrd needs **no modules at all**. The prototype's first
    attempt shipped the entire kernel module tree and came to 64.9 MB: an initrd whose only job is
    to reach the filesystem that *contains* the module tree.
- **⚠️ `CONFIG_EFI_ZBOOT` must NEVER be set — a hard correctness requirement, not a size
  preference.** systemd's UKI stub refuses an inner kernel with a non-empty PE base relocation
  table (`pe_kernel_check_no_relocation`); the only bypass is `load_via_boot_services()`, taken
  solely under SecureBoot+shim, which we deliberately don't have — so for us the check is
  **unconditional and permanent**. ZBOOT wraps the kernel in a self-decompressing PE carrying 68
  fixups; a raw arm64 `Image` has none. The failure mode is nasty: it breaks *after* systemd-boot
  has already spent a boot-count try, so it presents as a mysterious rollback rather than a
  misconfiguration. **Not implicated: `CONFIG_RELOCATABLE` / `CONFIG_RANDOMIZE_BASE` — we do not
  trade away KASLR** (those relocate at runtime via the kernel's own stub and add nothing to the
  PE relocation table). Detector runs on the **shipped artifact**, not the config: parse the UKI's
  PE sections and assert `.linux` is a raw arm64 `Image` with an empty relocation table. Note the
  regression is **nested** — `CONFIG_EFI_ZBOOT=y` produces `vmlinuz.efi`, a PE whose own `.linux`
  section holds the zimg, so a naive "is there a zimg at offset 4" check misses precisely the case
  it exists to catch.
- **Kernel config: verify what you asked for, and beware `=m` where you meant `=y`.** Take
  freedesktop-sdk's `expected-configs` verification mechanism. Kconfig **silently drops** options
  whose dependencies are unmet, and three separate cases have already been caught this way
  (`DM_VERITY_VERIFY_ROOTHASH_SIG_SECONDARY_KEYRING` needing `SECONDARY_TRUSTED_KEYRING`;
  `VIRTIO_VSOCKETS` needing a `VSOCKETS` parent). Without verification each would have surfaced
  much later as "vsock doesn't work" or "a verity signature won't validate", far from the cause.
  A check that accepts `=y` **or** `=m` is not a check: `CONFIG_SQUASHFS` — the filesystem `/usr`
  *is* — was declared that way and happened to resolve to `=y` from defconfig, leaving it correct
  and unprotected at the same time, indistinguishable from correct-and-protected until a defconfig
  change moves it. A module cannot mount the filesystem that contains the modules.
  - **Two interfaces are needed by consumers that never declare a dependency on them**, so they go
    silently absent and surface far from the cause. **`CONFIG_DMI_SYSFS=y`** — systemd reads SMBIOS
    Type 11 credentials from `/sys/firmware/dmi/entries/11-*/raw`; without it that whole tree does
    not exist and host-set credentials are unreadable, *while the kernel still prints its `DMI:`
    banner* (that comes from the built-in type 0/1 scan and says nothing about Type 11). Note an
    absent directory and an empty one look nearly identical to `ls`, so a probe that reads this
    tree must assert the directory exists or a missing kernel interface reads as a negative result
    about SMBIOS delivery. **`CONFIG_CRYPTO_SHA256=y`** — dm-verity resolves its hash by name
    through the crypto API at runtime, so Kconfig happily accepts `DM_VERITY=y` beside a modular
    SHA-256, and verity then fails at boot with the root filesystem unverified.
- **`SECONDARY_TRUSTED_KEYRING` is load-bearing for the developer-mode policy.** "Developer mode
  enrolls an additional certificate, it never disables verification" is not just a slogan — the
  builtin keyring carries the LiminaOS key and the secondary keyring is where a locally enrolled
  developer key lands. The policy call therefore constrains the kernel config.

### Image model — verity `/usr`, A/B, and where configuration lives

Adopted wholesale from Lennart Poettering's *Fitting Everything Together*, with the deltas our
substrate forces. The build system is **BuildStream on a freedesktop-sdk base** — we
occupy `gnome-build-meta`'s position, junction fdsdk and override elements rather than forking it.

- **Discoverable Partitions Spec + `systemd-repart`.** GPT type UUIDs make the image
  self-descriptive, so there is **no `/etc/fstab` and no `root=`**. repart runs on first boot to
  create what the shipped image omits (the B slot, `/var`, swap) and to size the filesystem to the
  actual disk. **No installation step: every image is a live image** — creating a VM is *copy the
  image and let repart grow it*, which deletes the installer from the product entirely.
  - **Host constraint that bounds the layout:** the guest sees a **fixed-capacity** virtio-blk, so
    repart's grow is limited by the image size at boot and the guest can never enlarge the backing
    file. "repart will grow it later" is true on metal and **false here** — the host must size the
    image up front. This is the expensive thing to get wrong in the partition plan.
- **Hermetic `/usr`, verity-protected, selected by `usrhash=` on the UKI's embedded cmdline.**
  `/usr` carries everything needed to bootstrap `/etc` and `/var` via `systemd-sysusers` +
  `systemd-tmpfiles`, so an empty root self-populates. Filesystem is **squashfs, chosen on
  measurement** (159 MiB vs 244 MiB for erofs-lz4hc). Note erofs-zstd was smaller still at 220 MiB
  and **unbootable** — our kernel has `CONFIG_EROFS_FS_ZIP_ZSTD` unset, and it built cleanly *and*
  passed `veritysetup verify`, failing only at mount. The filesystem writer and the kernel that
  reads it are configured independently and nothing cross-checks them.
  - **⚠️ dm-verity validates LAZILY, per block, on read — it is not a gate the image passes at
    boot.** Activation checks only the hash tree's root; a data block is hashed when something
    actually reads it. Demonstrated: a `/usr` with 4 KiB of garbage 50 MiB into a 166 MB
    filesystem **booted cleanly and was blessed on the first attempt**, because nothing touched
    that block during boot. Three consequences that shape the design rather than just the tests:
    (a) a guest can boot, pass every check, run for days, and then throw EIO when an application
    first opens a damaged file — with no event at update time and none at boot; (b) **corruption
    therefore does not reliably trigger rollback**, since boot counting only catches failures that
    happen *during* boot, and a broken-but-running system is the one state A/B has no answer for;
    (c) the guest-side `sha256sum -c` of the update share is consequently **the only whole-image
    check that ever happens** — verity never validates the whole image at any single moment, so
    that pre-check is not defence in depth behind verity, it is the only belt.
  - **A host-side lever worth building:** limina can verify a slot's whole `/usr` against its root
    hash **offline, from outside, with the guest powered off** — the complete check the guest
    never performs, on an image it cannot tamper with while stopped. That belongs in the same
    powered-off slot-health path that reads `.osrel`/`.cmdline` from the ESP. Built and proven in
    `spikes/liminaos-slot-health/`; the moment to run it is **immediately after a rollback**.
  - **⚠️ "dm-verity activated" is evidence about the hash TREE, not about the image.** A payload
    with a corrupt data block can ship a byte-identical, intact tree whose root still matches the
    GPT — activation has nothing to object to, and the failure surfaces at the first read of the
    damaged block. Anything reporting `verified` as reassurance is reporting the tree.
  - **Open, and it decides what the product protects against: how much of `/usr` the boot path
    actually reads.** Corruption at data block 0 rolls back; corruption 50 MiB in boots clean and
    gets blessed — same image class, opposite outcomes, separated only by *where* it sat. If that
    read-on-boot set is small, verity + A/B is a **tampering** defence being quoted as a
    **corruption** defence. Measurable: we own the virtio-blk backend, so a read-trace across a
    boot gives it. The answer is a fraction plus which regions — it moves when an early unit is
    enabled, so it is not a constant to quote later.
- **The cmdline must be embedded in the UKI**, since without a SecureBoot chain an externally
  supplied cmdline is unauthenticated and could simply drop `usrhash=`, making verity decorative.
  This is safe on our stack: **limina passes no cmdline at all on the EFI path** (it sets only the
  firmware blob; `--cmdline` reaches the direct-kernel path alone), so the UKI's `.cmdline` is the
  sole source, neither appended to nor overridden. For the same reason systemd-boot ships with
  `editor no`.
- **Boot counting sorts an exhausted entry last; it does NOT refuse it.** The natural reading —
  "tries exhausted ⇒ systemd-boot skips to the older UKI" — is wrong, and wrong in the direction
  that looks fine in testing. Observed: `+3 → +2-1 → +1-2 → +0-3`, then systemd-boot **reopens
  `+0-3` and tries it again**. So a single-slot image with a broken UKI is an **infinite
  retry loop, not a rollback**. Protection comes from having a good slot to *prefer*, which means
  **both the bless side (`systemd-bless-boot` / `boot-complete.target`) and a populated B slot must
  exist** before boot counting protects anything at all.
  - **Proven on real KRUN_EFI**, not only under TCG: `+3-0 → +2-1 → +1-2 → +0-3`, every rename
    surviving the power cycle, then the exhausted entry sorts last and the good slot takes over.
    That the two bootloaders agreed is a *result* — they are not the same code executing.
  - Corollary for a headless guest: with `timeout 0` + `editor no`, boot counting is the *only*
    path back from a bad update, so confirm the menu is still reachable on a held key over serial
    before depending on it. **And a verity failure parks the guest in an emergency shell that a
    locked root makes unusable**, so there is nothing behind boot counting — which means `timeout 0`
    needs a positive justification, not merely a passing reachability test.
  - **A successful rollback leaves the guest with no working fallback, and says nothing.** After
    recovering, the machine runs fine with zero failed units while its only fallback is a slot that
    is both exhausted and corrupt — one bad block from having nothing to boot. No guest-side lever
    can see this *by construction*: verity validates blocks on read and a dormant slot is read by
    nothing; sysupdate reasons about versions, to which two slots present is the healthy shape;
    there is no failed unit because the degradation is not in the running system. The obligation is
    host-side, and reporting a recovery as an unqualified success is the actual defect.
- **`systemd-sysupdate` A/B** on partitions + UKIs in the ESP, with **the limina twist: the host
  serves updates over virtiofs** — sysupdate takes local paths, so a LiminaOS guest needs no
  network and no update server. Host contract: `--share updates=<dir>` → tag `limina-updates`,
  a flat directory of payload files. The guest should mount it **on demand and unmount after**: a
  permanently-mounted virtiofs share blocks guest s2idle, which would make every LiminaOS guest
  unsuspendable. Proven end-to-end on real KRUN_EFI: the guest installs into the free slot, the
  running slot is untouched, and the counter clears on the next successful boot.
  - **⚠️ The directory transport carries NO integrity checking, and no setting turns it on.**
    sysupdate's `SHA256SUMS` manifest is an **HTTP-source mechanism** — read for `Type=url-file` /
    `Type=url-tar` only, with `Verify=` controlling that manifest's *signature*. Against a plain
    directory (`Type=regular-file`) there is no manifest step at all, so the man page's
    "downloaded payload files are unconditionally checked against the SHA256 hashes" is true and
    **vacuous for us** — it quantifies over an empty set, and `Verify=true` would have nothing to
    verify. A corrupt payload beside a stale manifest installs cleanly, `RC=0`. **Verifying the
    share is therefore ours to do**, in the guest, before sysupdate runs. This is a real cost of
    choosing a directory over HTTP and it was not visible when that call was made.
  - **Ceiling to know about before signing matters:** this transport can never carry
    sysupdate-*native* signature verification, because `Verify=` only ever applies to a manifest
    that a directory source never fetches. Integrity against corruption is solvable in the guest;
    **authenticity is not**, without either our own verification step or an HTTP source. Worth
    settling when "signed images" stops being a design word and becomes a shipped mechanism.
- **Configuration lives in `/usr`, not in factory `/etc`.** Factory `/etc` is a **first-boot
  seeding mechanism, full stop** — no `systemd-tmpfiles` `C` variant ever overwrites an existing
  file (`C` skips a non-empty destination entirely; `C+` descends into it; the `!` suffix means
  "only safe to execute at boot" and has nothing to do with replacement). So anything seeded into
  `/etc` is **frozen on that guest from first boot**, and a bad default shipped once can never be
  corrected by an update — the update mechanism becomes structurally incapable of fixing its own
  mistake. Therefore: everything LiminaOS sets goes in `/usr` (`/usr/lib/systemd/system/*.d/`,
  `/usr/lib/sysctl.d/`, …), where an update genuinely replaces it and `/etc` still outranks it for
  admin overrides. Factory `/etc` is reserved for what has no vendor search path, and that list is
  kept small enough to audit.
  - **Unit enablement must ship in `/usr`, not be left to first boot.** `systemctl --root
    preset-all` at build time writes enablement symlinks into
    `/etc/systemd/system/*.target.wants/`, which a hermetic image does not carry. A first boot
    survives this — systemd runs `preset-all` itself when `/etc` is empty (`Populated /etc with
    preset unit settings`) — so the defect is **invisible on day one and arrives on day two**:
    those presets are applied *once* and frozen in `/etc`, so an update that changes the enabled
    set never reaches an existing guest, and anything enabled at build time that no preset covers
    is lost outright. Enablement in `/etc` is also indistinguishable from an admin's `systemctl
    enable`, when it is a vendor default. Fix: merge preset output into
    `/usr/lib/systemd/{system,user}` at build time and assert `/etc/systemd/*` is empty afterwards.
    A `C`-line would seed the first boot and then be permanently unable to ship a *changed* set.
    (`.wants/` directories are a **union** across `/usr` and `/etc`, not an override, so an update
    can ADD an enabled unit but cannot RETRACT one frozen in `/etc` — pending empirical
    confirmation on the build leg.)
- **Per-VM secrets ride SMBIOS Type 11, not the cmdline.** ✅ Shipped host-side:
  `limina --smbios-oem-string` publishes OEM strings the guest's systemd imports as credentials
  (`io.systemd.credential:<name>=<value>` → `/run/credentials/@system/<name>`). Baking a secret
  into the UKI `.cmdline` would make it per-*image* (shared by every guest built from it,
  changeable only by rebuilding) and world-readable from `/proc/cmdline` inside the guest.
  EFI-boot only, since libkrun writes SMBIOS only on the firmware path — and it needs
  `CONFIG_DMI_SYSFS=y` in the guest kernel (see §Boot chain), without which the credential is
  delivered but unreadable.
- **Crypto tier deliberately skipped; verity kept.** KRUN_EFI has no SecureBoot and libkrun has no
  TPM, so LUKS2-sealed-to-TPM2 and PCR measurement have no substrate. We take dm-verity + signed
  images (integrity, rollback protection, measurable objects); confidentiality stays at the host
  layer (FileVault/APFS), which was already this section's position. **The partition layout must be
  designed so the crypto tier can be added later without a re-layout.** Named future lever, not
  scheduled: a **paravirtual TPM2 in libkrun backed by the macOS Secure Enclave** — M14 already has
  the SEP machinery, and it would unlock the article's model verbatim. `systemd-homed` is likewise
  skipped for now: its wins are host-layer concerns for a VM whose disk is already in FileVault.
- **Factory reset** via repart's erase-on-reset partition marking, exposed as a **host UI action**
  ("reset this VM to factory") — far more natural in a VM than on metal.
- **Not adopted (the one article idea left out): portable services.** Our system-service tail goes
  in the image or in a sysext; portable services would add a fourth delivery format with no current
  consumer. Revisit if a service ever wants its own image.

### How software gets installed (the image-based elephant)

Three tiers, and we deliberately do **not** build package layering (Silverblue's
`rpm-ostree install` lesson: a crutch that reintroduces package management with worse
ergonomics):

1. **GUI apps → Flatpak.** Out of the base image, own update cadence, survive rollbacks in
   `/var`. "The OS ships complete, apps come from Flathub" is the whole story for non-dev use.
2. **CLI/dev → containers (toolbox/distrobox), first-class.** A mutable Fedora-or-anything
   userland with real dnf, home shared, base sealed — how people actually live on
   Silverblue/ChromeOS (Crostini is this model). We own the distro, so it ships preconfigured
   with session integration (exported apps/binaries, default terminal target). This tier doubles
   as the **agent-isolation boundary** (no access to the sealed base, the session compositor's
   socket, or unshared mounts) — with clone-VM-per-agent as the stronger lever limina uniquely
   makes cheap.
3. **System-level tail → systemd-sysext.** Overlayfs on `/usr`, layers fine over the verity
   root, composes with sysupdate. Because we control the image, common needs go *in the image*
   next release; sysext is the escape hatch, not a pillar.

### Development workflow for the compositor itself (the ladder)

Inner loop → outer loop: (1) **nested** — the system compositor runs windowed as a client of the
running session (we own both ends, small backend, zero blast radius); (2) **scratch clone VM**
with the test build attached as a sysext — tests the real thing (DRM master, boot ordering,
serial as log oracle) against a disposable file; (3) **sysext on the dogfood guest** as final
soak — `systemd-sysext refresh` to install, `unmerge` or reboot-without to revert to the
image's known-good build. This is GNOME OS's own hacking model; the reversibility is the point
when the thing under test is the only display owner.

### Compositor restart without dropping clients

Two different problems, deliberately different answers:

- **System compositor restart → reconnect model.** It has exactly one client, ours, on a private
  protocol: build reconnect-and-republish into the session compositor and system-compositor
  restarts are free. Do this one **first** (days, not weeks) — it alone makes live iteration on
  the display owner painless, and the layer stays upgradeable forever.
- **Session compositor planned restart/upgrade → exec-in-place handover** (generic reconnect is
  a dead end for arbitrary clients: Qt can rebuild (`QT_WAYLAND_RECONNECT`), GTK can't and won't
  soon). The design: freeze; **quiesce and drain both directions** (stop reading clients, finish
  or snapshot in-flight requests, flush outgoing fully — an unflushable tail goes in the
  snapshot and is written first by the successor, or a message is torn); keep the fds open
  (CLOEXEC cleared, manifest of fd→role, systemd fd-store naming); write the state snapshot
  (scene graph, serials, un-acked configures, frame callbacks) to a memfd; `exec` the new binary
  **same PID**; successor deserializes and resumes. Load-bearing details:
  - **DRM fds survive exec ⇒ master status, framebuffer objects, and GEM handles survive** —
    the currently-scanned-out FB stays live, no modeset, no black frame. Handover is invisible.
  - **Driver state does not survive**: EGL/Vulkan contexts die; therefore every long-held buffer
    is held *as a dmabuf fd* (shm pools as fds), re-imported/re-mmapped by the successor.
  - **libwayland is the least handover-friendly layer**: recreate every `wl_resource` with the
    same object ID (`wl_resource_create` takes an explicit id) but the global serial counter has
    no setter — patch libwayland-server (small) or own the server library. We own the stack.
  - **Failure plan**: `exec` failing returns to the old process (handle it, resume). The
    successor's deserialize is read-only-until-validated; on any error it execs *back* to the
    old binary path recorded in the manifest — A/B semantics for the compositor binary itself.
  - **Watchdog keeps ticking across exec** — successor must `sd_notify(WATCHDOG=1)` before
    anything slow; snapshot load must fit the window.
  - **Exercise it constantly** (restart-into-self on every scratch-lane deploy, snapshot
    version round-trips in CI) — systemd's `daemon-reexec` stayed boring because it runs all
    the time; a twice-a-year handover path rots into the scariest code we own.
- **Crashes are scoped out, explicitly.** A crashed compositor can't serialize; crash survival
  means an always-alive fd-holding shadow process (a real architecture commitment) or the
  reconnect model's toolkit limits. Honest, shippable answer: crash = clean animated "session
  ended" screen from the system compositor + relaunch. Sealed images + the test ladder should
  make it rare.

### Sequencing — compositor first, distro second

The system compositor is independently valuable and **derisks the distro decision rather than
depending on it**: it can ship on the *Fedora* enhanced tier first (RPMs replacing Plymouth+GDM
through the existing enhanced delivery), where it's also the differentiated payoff — seamless
boot-to-desktop is exactly the Parallels-polish gap. The distro is the bigger commitment and its
real cost is not the initial build but the cadence forever after (security updates, toolchain
bumps, kernel tracking) — freedesktop-sdk as the base layer is what makes that survivable for a
small team; inherit it, don't rebuild it.

**The distro leg went first anyway**, on a rule worth reusing: *answer the cheap plan-killer
first*. "Does KRUN_EFI boot a UKI at all" was answerable in days and would have invalidated the
whole approach; the compositor's gating unknown (exec-in-place handover) is expensive and
invalidates only the restart story. The compositor is still what ships value earliest and can land
on the Fedora enhanced tier before LiminaOS is usable.

**Done test (compositor phase):** an enhanced Fedora guest boots with no Plymouth/GDM into the
system compositor, host-splash→guest cross-fade is seamless, login/logout are animated with no
mode switch or black frame, the session runs with zero /dev/dri-master or /dev/input access, and
a system-compositor restart mid-session is invisible to the seated session.
**Done test (distro phase):** a LiminaOS image boots KRUN_EFI → systemd-boot → UKI → verity `/usr`
→ compositor, with no simpledrm and no VTs; sysupdate applies an A/B update and a forced-bad slot
auto-rolls back via boot counting **with both slots populated and the bless side live** (the
single-slot case retries forever by design, so it does not test rollback); Flatpak, toolbox, and a
sysext all install and survive the update; a session-compositor exec-handover upgrade keeps a
running GTK client alive.

**Risks / spike first:**
- (a) **exec-in-place handover** on a toy compositor — fd manifest, snapshot round-trip, exec-back
  rollback. Still open, and still the gating unknown for the restart story.
- (b) ✅ **CLOSED** — boot chain `KRUN_EFI → systemd-boot → UKI` proven on real firmware with our
  own 16 KiB kernel, ESP writable. Closed alongside it: offline unprivileged `systemd-repart`
  works, and the kernel builds byte-identically across 4 KiB and 16 KiB build hosts — an invariant
  worth wiring into CI, since anything that breaks it is a regression in *something* even when
  everything still boots.
- (c) **passthrough latency** — prove the system compositor adds zero frames on the fullscreen path
  (gamescope's problem) before building the animation layer on top. Still open.
- (d) **16 KiB-clean userspace is only half-verified.** fdsdk's toolchain (gcc/binutils/glibc) is
  clean, but **Mesa and the graphics stack have not been built** — which is exactly where 16 KiB
  assumptions historically live, and exactly what the venus tier needs. Do not inherit "fdsdk is
  16k clean" without this qualifier.

---

## Summary of net-new code vs libkrun patches

| Milestone | Net-new limina code | libkrun (or fw/virgl) patches |
|---|---|---|
| M1 boot ✅ | CLI, internal-API `limina-vmm`, child supervisor, codesign | (optional) harden panic exit paths |
| M2 display+input ✅ | supervisor IOSurface window, native-Rust display backend, input provider, kVK→KEY table | software-2D scanout (0001); hw-cursor queue (0008); Darwin input worker ran as-is |
| M2.5 console/serial ✅ | serial command/getty shell (`l1_command` + stock getty), serial pane in window, boot-console-frame test | PL011 tty (0004 HVF halfword-MMIO + 0005 FDT `arm,primecell`); hvc0 (0003), PL011 WouldBlock (0002); KRUN_EFI EDK2 + VirtioGpuDxe GOP (0006/0022 + PlatformBm.c) |
| M3 networking ✅ (NAT+SSH; bridged deferred) | gvproxy supervision + gateway cleanup; well-known-MAC static lease | none needed (reconnect-on-HANG_UP still optional) |
| M4 3D 🟢 | coexist routing, zero-copy + fence-accurate present path, KK as host driver | coexist (0010), fence-present series (0017–0022), virglrenderer fork, KK perf/XFB, guest-kernel fork commits, mutter ×2 (retired — see M4); remaining: upstream queue |
| M5 clipboard/fs/agent 🟢 core | guest agent (from L1 vsock seed), NSPasteboard bridge, ext-data-control + RemoteDesktop clipboard clients, virtiofs share + auto-mount, enhanced-tier installer (remaining) | mutter 0003 (ext-data-control); none for transport (vsock+virtiofs exist) |
| M6 dynamic memory ✅ | PSI autoballoon policy + `BalloonControlHandle` / `--memory` / control socket (internal Rust API, not a C ABI) | reclaim fix (MADV_FREE_REUSABLE) + 16 KiB align/coalesce + inflate/deflate handlers + DEFLATE_ON_OOM (0033/0034) |
| M7 USB 🟢 emulated / 🟡 passthrough | host claim/attach, usbip plumbing; real-device capture waits on the privileged helper | emulated xHCI controller (0095–0098, shared with M14, default-on); our-kernel config edit (USB+uinput) |
| M8 audio/x86/polish 🟢 polish + audio shipped; x86 + multi-display open | fullscreen, keymap, multi-display, pointer capture, IOSurface mach-port scoping, FEX wiring | native virtio-snd; runtime resize/EDID; LED parity; `virtio-rtc` device (booked) |
| M9 suspend/resume + snapshots ✅ | host-side VMM snapshot (file format/CRC, `--restore` wiring, device schema + mapped-blob set, named-snapshot manager + clone + APFS `clonefile` disk, agent freeze bracket, proto `Snapshot`/`Restore`/`TimeSet`, capability probe, UX); Mesa-venus object-graph replay + **device-local content readback** + blob copy-back (venus tier) | multi-vCPU HVF pause/quiesce (incl. WFE-parked wakeup) + vCPU save/restore (wrappers, FFI exists) + GIC state (spike #2 green) + `CNTVOFF` set + `--restore` mode + device (de)serialize + virtio freeze/thaw hardening + snapshot-time GPU quiesce (restore = fresh worker, no in-process renderer reset; `reset_session` rutabaga-context fix already shipped, 0035); carry `patches/linux` Dongwon-Kim drm/virtio freeze-restore (virgl) |
| M10 multiple disks + ISO ✅ | repeatable `--disk`, stable virtio serial identity, `--cdrom`, qcow2 sniff, EFI-ISO boot | imago discard→punch-hole fix (fork, pinned by `third_party/manifest.toml` + `[patch.crates-io]`) |
| M11 productization ✅ | `cargo xtask` command surface (`setup`/`vendor`/`build`/`sign`/`test`/`run`/`app`/`bundle`) wrapping the tested scripts; `docs/dev-onboarding.md` | none |
| M12 SPICE agent 🟢 clipboard | host vdagent broker (framing + clipboard) shipped behind M5's NSPasteboard bridge; remaining: client→guest file transfer, and a **per-session** arbitration probe that binds `ext_data_control_manager_v1` instead of testing vdagent liveness — plus the host-side mute (a clipboard-less announce) the resulting overlap needs; display-resize deliberately excluded (native EDID already covers it) | virtio-serial named multiport port `com.redhat.spice.0` (wakes stock `spice-vdagentd`); no crate reuse |
| M13 visibility/power render adaptation 📋 planned | front-end occlusion/Space/power signal + hysteresis policy, `vm.toml [power]/[render]` config, host present cap/pause (reuse s2idle), agent frame-rate throttle message | present pause/cap knob (extends s2idle 0089) + fence-feedback pacing knob; relax deep-idle bias on occlusion |
| M14 biometric auth ✅ both halves shipped | host CTAP2 authenticator (SEP ES256 + LAContext, CryptoKit blob store per VM), agent uhid FIDO bridge + vsock channel, later xHCI + FIDO/MOC-fingerprint gadgets, pam_u2f/authselect recipe | none for uhid transport (vsock exists); stock wave = xHCI controller + gadget device models in libkrun (shared with M7) |
| M15 display pipeline v2 📋 planned | per-hw-display window/present policy (native refresh + VRR pacing), CALayer-per-plane compositing, WindowServer GPU-share profiling | krun-display EDID/modes per host display (incl. VRR range), virtio-gpu overlay-plane + YUV(NV12/P010)+color-props protocol extension (device + guest kernel, capset-gated), primary-plane format/modifier advertisement (XBGR/ABGR now; non-LINEAR per `spikes/scanout-modifiers/`), cursor-size lift (low-prio) |
| M16 LiminaOS 💭 moonshot | system compositor + private session protocol, greetd-shaped session-launch helper, host-splash→guest cross-fade, exec-handover machinery; distro phase: BuildStream/fdo-sdk + sysupdate image pipeline | none host-side; guest kernel config (SYSFB off, `CONFIG_VT=n`, DM_INIT, virtio built-in) + small libwayland-server handover hooks on our forks |

## First three things to spike

All three founding spikes are **RESOLVED** (M6 reclaim now shipped, 2026-06-26): M1 boot path
(EFI+disk, no remount — `spikes/m1-boot`); M2 input worker on Darwin arm64 (builds + wakes); M6
reclaim (`MADV_FREE_REUSABLE` drops `phys_footprint` on an `hv_vm_map`'d region —
`spikes/balloon-madvise`, re-confirmed on the shipping macOS release).

The standing rule remains: spike the gating unknown before building on it.
