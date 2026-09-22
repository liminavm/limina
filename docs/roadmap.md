# limina Roadmap

A milestone plan for **limina** — a Rust macOS app on libkrun (Hypervisor.framework) that replaces
Parallels for running Linux guests on Apple Silicon. Milestone numbers are a *dependency* order, not
a schedule. Each milestone states what it is for, what it established (the load-bearing facts), and
what is still owed. Research lives in `docs/research/`; the render/present stack in
`docs/graphics.md`; windows and input in `docs/input-and-windows.md`; small loose ends on shipped
work in `docs/hardening-backlog.md`.

**State in one paragraph.** Shipped and in daily use: M1 boot, M2 display+input, M2.5 console,
M3 NAT+SSH, M4 venus 3D, M5 control plane/clipboard/virtiofs, M6 dynamic memory, the polish and
audio halves of M8, M9 suspend/resume + snapshots, M10 disks, M11 the `cargo xtask` surface, M12's
clipboard, M12.5's QEMU-guest-agent steps 1–4, M14 biometrics, the emulated xHCI of M7, and M15
wave 1. Open: the rest of M15, M13, M17, the M12 file-transfer half. Deferred by decision: M3
bridged networking and M7 real-device USB capture (both wait on the one privileged helper). M16
(LiminaOS) is the moonshot.

---

## Cross-cutting architecture decisions

- **Raw HVF via libkrun, not Apple Virtualization.framework.** Vz is closed and forbids the custom
  virtio devices, host-USB passthrough, fine-grained ballooning and patchable guest agents that are
  limina's point (research 02).
- **libkrun is consumed as a Rust crate** (internal API, no C ABI): the worker assembles
  `VmResources` in its `krun/` facade and runs `build_microvm` + the event loop itself.
- **Dedicated child-process VMM.** `krun_start_enter` loops forever and guest PSCI SYSTEM_OFF tears
  the whole process down (hvf `VcpuExit::Shutdown` → run-loop exit → exit eventfd), so the AppKit
  supervisor runs the VMM in a child and drives it over vsock + the shutdown eventfd.
  **Reboot = relaunch the child:** `SYSTEM_RESET` exits with `FC_EXIT_CODE_REBOOT` (125) and the
  supervisor relaunches the worker (recycling gvproxy, whose vfkit socket is single-connection)
  under a boot-loop cap. Guard: `reboot::guest_reboot_relaunches_the_worker`.
- **Codesigning is a hard gate.** The worker must carry `com.apple.security.hypervisor`
  (`crates/limina-vmm/hvf-entitlements.plist`), else `hv_vm_create` fails.
  `com.apple.vm.networking` is Apple-gated; the default network is user-mode NAT.
- **Page size.** The host has 16 KiB pages. Every VM is created at a 4 KiB stage-2 granule
  (`hv_vm_config_set_ipa_granule`, macOS 26+) unless its definition asks for `ipa_granule = "16k"`,
  so nothing *requires* a 16 KiB guest; the enhanced tier runs one because it is 4-8% faster on
  guest CPU-bound work (`perf/2026-08-27-ipa-granule.md`). Analysis: `docs/design/16k-page-requirement.md`.
- **Two-tier guarantee** (CLAUDE.md): an unmodified stock guest on upstream-shaped libkrun must
  always boot and stay usable, degraded where our components are missing; capabilities are detected
  granularly and additively.
- **Fork model.** Every patched dependency is a fork under `github.com/liminavm`, pinned by rev in
  `third_party/manifest.toml`; the fork's `limina` branch is the delta; tag before every rewrite;
  `cargo xtask vendor` recreates the trees. A reference like "libkrun 0022" or `patches/linux/0001`
  names a commit on that fork's branch — the numbering is how we talk about the changes; the patch
  directories are tombstones. Upstreaming status: `docs/upstreaming/ledger/`.

---

## Testing infrastructure

Tests drive the **shipped binaries** — `limina` → `limina-vmm` → libkrun/HVF — through the harness
in `crates/limina-test` (the `Guest` type: boot, await a console marker, teardown that never leaks a
live VM).

- **L0 — unit.** Pure Rust per crate; no HVF; plain `cargo test`.
- **L1 — fast boot.** A static Rust `init` (`guest/limina-init`, musl) served as the root over
  virtio-fs, direct-booted on our test kernel (`scripts/build-test-kernel.sh`, default v6.12,
  `PAGESIZE=4k|16k`; falls back to libkrunfw's Image). Boots and powers off in ~0.3 s
  (`tests/l1_boot.rs`). The init also runs a tiny vsock agent and, with `limina.console_shell`, a
  few in-process built-ins framed by a `LIMINA_SHELL_DONE rc=` terminator (`Guest::console_command`).
- **L2 — real images.** Stock Fedora through firmware + GRUB (`tests/boot.rs` — the two-tier floor),
  plus the enhanced images for venus, video, clipboard, suspend and restore tests.
- **Trace-replay rendering tests.** Capture a workload once, replay it twice in one boot (venus vs the
  software rasterizer as reference), tolerance-compare frames — no stored goldens.
  `venus_replay` (GL via apitrace), `venus_vk_replay` (Vulkan via gfxreconstruct),
  `venus_shell_replay` (the seated gnome-shell via an `LD_PRELOAD` egltrace). The present/scanout
  path is not covered by replay — that stays with the seated-desktop and iosdump oracles. Protocols:
  `spikes/trace-replay/RESULTS.md`. Perf trend ledger: `scripts/perf-ledger.sh` → `perf/ledger.csv`
  (a trend, never a gate).

HVF tests need a codesigned worker, so they are gated on `LIMINA_HVF_TESTS=1` and run by
`cargo xtask test` / `scripts/run-suite.sh`. Hosted CI (`.github/workflows/`) is a compile-and-unit
gate only; it cannot run HVF or the host GPU. **Fix bugs RED-first** (CLAUDE.md).

**Owed — host→guest synthetic input.** osascript keystrokes are unreliable (macOS eats F11, lone
modifiers may not route). We own the guest keyboard/pointer evdev devices, so add a path that writes
events straight into them, exposed as a control-plane message and a CLI/harness helper
(`limina sendkey <combo>`, a `Guest` API). Small; unblocks scripted UI tests and deterministic agent
control of the desktop.

**Owed — guest profiling without a guest PMU.** HVF shows the guest no PMU, so guest `perf` falls
back to hrtimer sampling, which is masked through hardirq and most softirq work — exactly the
interrupt-heavy paths (virtio-net RX, NAPI) come back invisible. In order:

1. **Host-side guest sampler** (`perf kvm --guest` shape): a thread in `limina-vmm`
   (`LIMINA_GUEST_PROFILE=<file>[,hz]`) kicks vCPUs out with `hv_vcpus_exit` at ~1 kHz; each records
   PC, LR, EL and optionally a frame-pointer walk; a vCPU already outside the guest is tagged by exit
   reason, so hypervisor time shows too. Symbolize offline against `/proc/kallsyms`/`vmlinux`. Needs
   nothing in the guest; lands regardless of guest interrupt masking. Kernels without frame pointers
   give flat profiles.
2. **Virtual PMUv3 backed by host counters**, so guest `perf` just works. Present the architectural
   PMUv3 (every arm64 kernel has its driver; Asahi's `apple_m1_cpu_pmu` takes an AIC FIQ and won't
   bind on our GICv3, but is the reference for the Apple events to map onto): advertise it in
   `ID_AA64DFR0_EL1` + a DT node, emulate the sysregs on trap, read real counters via kperf/kpc, inject
   the overflow interrupt. **Gates to measure first:** HVF traps guest PMU sysregs to us; kpc is
   usable by an unprivileged, sandboxed worker; counters can be filtered to guest execution. The
   sampler in (1) is the oracle for (2).

---

## Robustness & resource discipline (cross-cutting)

**The guest is untrusted and the host must survive it.** Two failure shapes drive nearly all of it:
host resources the guest consumes but cannot see (memory allocated on its behalf lands in the
worker, so the guest's accounting stays flat until macOS jetsams the whole VM), and guest-reachable
aborts (a malformed command tripping an assert in a host library kills the VMM).

**Established:**
- **Host GPU-memory budget** (`docs/design/gpu-memory-budget.md`) — a per-context ledger, always on,
  plus an opt-in cap that kills the offending context rather than letting the worker grow into jetsam.
  The guest is told the truth through `VK_EXT_memory_budget`, the one backpressure channel venus does
  not discard (a refused allocation's `VkResult` is dropped by the transport; a budget query
  round-trips), so a well-behaved client can shrink its caches.
- **Scanout retention** — the holder was the *supervisor* (an unbounded frame-apply surface cache,
  then a send right it never dropped). Method lesson: capping the cache **bounded** the growth and
  read like a fix while the retention was still underneath. Guard: `scanout_churn_retention`; matrix:
  `spikes/venus-churn-retention/buffer-lifetime-matrix.md`.
- **Guest-triggerable host aborts** — the empty-clear-rect class fixed at the renderer's trust
  boundary; the host KosmicKrisp builds with asserts off (`-Db_ndebug=true`, enforced by
  `scripts/build-app.sh`).

**Owed:**
- **Audit the venus dispatch boundary for guest-triggerable invalid usage** — every place a
  guest-supplied count, offset, handle or rect reaches a host API. The fixes so far were one incident
  at a time; a systematic pass closes the class. Pairs with upstreaming.
- **Levels for the leak-hunt instrumentation** (census, per-allocation tracing) so it can stay
  available without the noise.
- **GL-only guests are unbounded.** The cap is enforced at `vkAllocateMemory`, which a pure-vrend
  session never reaches; its allocations are accounted in the shared vrend bucket, but nothing stops
  them.

---

## Milestone 1 — Boot a stock Fedora image to a serial console

`limina --firmware <KRUN_EFI.fd> --disk <fedora.raw> --console <file>` boots the stock distro via
EFI to userspace. Load-bearing facts:
- **Firmware is a flat EDK2 `.fd`** read into guest RAM (`Payload::Firmware`), not a linked library.
  Ours is built by `scripts/build-krun-efi.sh` (M2.5).
- The distro's own initramfs mounts root (FAT ESP / ext4 `/boot` / btrfs `subvol=root` on `vda`).
- **Console:** `disable_implicit_console = true` + a `SerialConsoleConfig` (input fd must be
  kqueue-pollable); libkrun's PL011 is at `0x0a001000`
  (`earlycon=pl011,mmio32,0x0a001000 console=ttyAMA0,115200`). When no console is requested, limina
  still attaches an output-dropped PL011 — a consoleless FDT makes EDK2's serial library fail its
  constructor.
- **Supervision:** the supervisor spawns the signed worker in its own process group and forwards
  graceful shutdown up the stop ladder; it never kills on a timer (M12.5).
- A stock EFI Fedora guest does not honour the GPIO power button, so graceful power-off needs
  `limina-agent` (M5) or the stock `qemu-guest-agent` (M12.5).
- Unhandled guest traps (unknown PSCI/SMC, exception class, sysreg, MMIO size) return an error and
  a clean exit instead of panicking the worker (`crates/limina-test/tests/hvf_graceful.rs`).

---

## Milestone 2 — Display + input (native window)

- **Display is supervisor-hosted via a shared IOSurface.** The worker publishes each scanout into a
  cross-process IOSurface; the supervisor owns the NSWindow and presents via `CALayer.contents`, so
  AppKit lives in the process that survives the VMM.
- **Tier 1 is software-2D:** libkrun serves the 2D scanout from host CPU memory with no renderer, so
  it works on a GL-less host. A headless PNG-capture sink is the test oracle.
- **Input:** NSEvent → virtio keyboard + absolute pointer (+ relative for capture). The provider must
  emit explicit `EV_SYN`/`SYN_REPORT` — the worker copies events verbatim. ABI footgun: the input
  backend `void*`s are the Rust `#[repr(C)]` backend types (`krun_input`'s
  `into_input_config`/`into_input_events`), not the header structs.
- **Present path:** rect-limited swizzle into a CPU canvas → a 3-deep IOSurface ring (no
  write-while-composite tear); implicit Core Animation actions disabled. `alloc_frame` needs exactly
  `width*4` bytes per row.
- **Hardware cursor** via the virtio-gpu cursor queue (upstream never serviced it); the published
  cursor IOSurface becomes the `NSCursor` worn inside the content view.
- **Window close → orderly guest shutdown** through the M5 control plane, falling back through the
  power button and the stock guest agent (`l1_shutdown`).

Everything since — resize, modes, EDID/hotplug, multi-display, fullscreen, capture, the notch — is
in `docs/input-and-windows.md` and M15.

---

## Milestone 2.5 — Console & serial

- **PL011 `/dev/ttyAMA0`** is a real bidirectional tty (`l1_serial.rs`): the FDT node carries
  `arm,primecell` so `amba-pl011` binds, and the HVF data-abort handler accepts 16-bit MMIO (the
  driver's `writew`; a panic there once looked exactly like a guest hang). **virtio-console hvc0**
  via `PortConfig::ConsoleInOut` (`l1_console.rs`); a plain `InOut` port is a data port.
  `--console-pty` gives a human an interactive firmware/GRUB/kernel console. Automated guest access
  is SSH; the serial getty is the human debug shell.
- **On the EFI path GRUB owns the cmdline** (EFI LoadOptions override FDT bootargs), so a firmware
  graphics console is the lever for pre-kernel stages. Our `KRUN_EFI` (`liminavm/edk2`, built by
  `scripts/build-krun-efi.sh`) adds `VirtioGpuDxe` and puts the virtio-mmio GOP on ConOut; libkrun
  snaps a ready-but-unsized queue to `max_size` and joins the firmware-era GPU worker on reset. The
  RELEASE GOP build is the windowed default (`resolve_windowed_firmware`: `$LIMINA_GOP_FIRMWARE` →
  the bundle's copy → `target/krun-efi/` → krunkit's silent `.fd` with a warning) and ships in the app.
- Image prep (`scripts/prepare-efi-image.sh`, `scripts/provision/make-accessible.sh`) adds
  `console=ttyAMA0` for the serial oracle and clears a stale `/.autorelabel`.
- **Floor guards:** `boot::fedora_stock_image_efi_boots_to_userspace` (firmware → GRUB → kernel →
  getty `login:` + sshd) and `boot::fedora_stock_image_efi_renders_to_gop` (a rich boot-console frame
  before `login:`).

---

## Milestone 3 — Networking (NAT, then bridged)

**Established:** `limina --net` supervises a gvproxy gateway (`-listen-vfkit unixgram://<absolute
path>`) and attaches virtio-net via `krun_add_net_unixgram` (vfkit mode, `NET_COMPAT_FEATURES`);
no libkrun patch. The guest must reach userspace for NetworkManager to DHCP, so net tests boot a
writable COW clone. **Inbound SSH needs no forwarding config:** the well-known vfkit MAC
`5a:94:ef:e4:0c:ee` gets the static `.2` lease and gvproxy's built-in `127.0.0.1:<port> → .2:22`
forward; the host port auto-allocates from 2222 (or `--ssh-port`) and the supervisor logs the
command. `krun_set_port_map` is TSI-only. Runbook: `docs/images.md` §SSH access; multi-VM design:
`docs/design/multi-vm-networking.md`.

**Owed:**
- **Bridged (opt-in):** vmnet BRIDGED needs the Apple-gated `com.apple.vm.networking` entitlement;
  SHARED/HOST need root — both through the one privileged helper
  (`docs/design/privileged-helper.md`). Spike first: does BRIDGED work over Wi-Fi `en0`?
- **Net worker reconnect on HANG_UP**, so a gvproxy restart doesn't disable the NIC for the VM's life.
- **Offload tuning:** evaluate `GUEST_TSO6|HOST_TSO6` once verified non-corrupting; mind the macOS
  datagram limit against large GSO frames.
- **Ergonomic SSH** (GNOME Boxes prior art, `docs/research/prior-art-gnome-boxes.md`): (A) inject the
  host public key as an SMBIOS type-11 OEM string
  (`io.systemd.credential.binary:ssh.ephemeral-authorized_keys-all=<base64>`), which stock systemd
  feeds to sshd with no agent — libkrun already writes type-11 strings on the EFI path
  (`third_party/libkrun/src/smbios/src/lib.rs`), so what remains is plumbing the credential and
  confirming stock Fedora consumes it; (B) a host `ProxyCommand` helper that dials the guest's vsock
  ssh port, for a stable `ssh limina-<vm>`.
- Enumerate TSI gaps (ICMP, mDNS, VPNs, multicast) for product expectations.

---

## Milestone 4 — 3D acceleration (venus)

The seated GNOME desktop runs on venus with fence-accurate zero-copy presents; GL runs on the
same device through vrend → zink-on-KosmicKrisp. **`docs/graphics.md` is the authoritative model**
(tiers, coexist, scanout contract, pitfalls, open items); this section keeps only the decisions.

- **One coexist virtio-gpu** (`GPU_COEXIST_FLAGS` in `crates/limina-vmm/src/krun/mod.rs`): 2D
  commands go to the software-2D path, 3D contexts to the renderer; renderer-init failure degrades to
  software-2D, never a panic. `--gpu-software-2d` is for the capture oracle only.
- **The renderer is virglrs**, compiled into the worker as a Rust crate (CLAUDE.md), serving venus
  (Vulkan) and vrend (GL). **KosmicKrisp is the one host Vulkan driver**; MoltenVK is retired (it
  crash-looped the compositor instead of degrading). Every venus path forces the KK ICD and degrades
  to software-2D without it.
- **The 16k/4k blob-map problem is solved host-side**: `hv_vm_map` wants addresses and size at the
  stage-2 granule, and limina creates VMs at 4 KiB, so a stock 4 KiB guest enumerates venus and runs
  `vkcube` (`spikes/hv-ipa-granule/RESULTS.md`). A venus failure still poisons the whole Vulkan
  loader rather than degrading — keep that in mind for any future one.
- **IOSurface is the macOS dmabuf.** Resolving venus exportable images to IOSurface-backed textures
  gives both mutter's exportable `vkCreateImage` and a no-readback `SET_SCANOUT_BLOB`; the guest kernel
  fences blob-scanout flushes and the host holds them to the true Core Animation latch
  (`FENCE_PRESENT=1 COPY=0` on the enhanced tier; the stock kernel keeps the copy).
- **The virgl (vrend) tier is shipped and on by default** — its copy model is immune to the page-size
  problem, so a stock 4k guest gets accelerated GL.
- **The enhanced tier ships as RPMs** (M5 §Productization); the app bundle carries the host GL/Vulkan
  closure (`scripts/build-app.sh`). Software-2D + in-guest llvmpipe stays a shipping path, chosen by
  guest capability, not host-driver absence.

**Owed:** guest-tools distribution from the app (M5); the upstream queue
(`docs/upstreaming/ledger/`); the open graphics items in `docs/graphics.md`.

---

## Milestone 5 — Control plane, clipboard, virtiofs, guest agent

**Established:**
- **Control plane (`crates/limina-proto`):** a 16-byte `LIMINA` header + CBOR payloads; **unknown
  types → `ERR_UNSUPPORTED`, never fatal**. One multiplexed vsock connection with the guest connecting
  out; the supervisor owns the host side and turns window-close/SIGTERM into an orderly power-off
  (SHUTDOWN → agent grace → power button → the stock guest agent → wait; never a timed kill). A peer
  registry serves several guest peers (root agent + per-session helpers); SHUTDOWN goes to every
  shutdown-capable peer; heartbeats flag agents silent past `LIMINA_AGENT_SILENT_SECS` (default 5 s).
- **Guest daemons:** `limina-agent` (reconnect loop, heartbeats, SHUTDOWN → poweroff, share
  auto-mount) and `limina-agent-session` (per-session helper).
- **Clipboard:** `CHANNEL_CLIPBOARD` (OFFER/REQUEST/DATA, newest serial wins); the supervisor's
  NSPasteboard bridge (`crates/limina/src/clipboard.rs`). Under GNOME the guest side is stock
  `spice-vdagent` (M12); `limina-agent-session` covers what vdagent can't (ext-data-control, with
  mutter's RemoteDesktop D-Bus clipboard as the opt-in fallback). Guest mutter is stock.
- **virtiofs sharing:** `--share '[NAME=]PATH[:ro]'` → tag `limina-NAME`; the agent auto-mounts every
  `limina-` tag at `/media/NAME`, discovering tags via `/sys/fs/virtiofs/<id>/tag` (not the cmdline,
  so it survives GRUB-owned boots). No agent → `mount -t virtiofs` by hand. Linux ≥7.1 rejects a FUSE
  reply with used length 0 — `l2_share_71` covers shares on a ≥7.1 kernel, because L1 runs 6.12.

Tests: limina-proto L0; `l1_agent`, `l1_shutdown`, `l1_real_agent`, `l1_multi_agent`,
`l1_clipboard`, `l1_clipboard_multi_session`, `l1_session_helper`, `l1_share`, `l1_liveness`,
`l2_share_71`, `l2_clipboard_vdagent`.

### Productization: RPMs replacing stock at `/usr`

- **Userspace = rebuilt Fedora SRPMs replacing stock**, dnf-versionlocked. Not a sysext: our mesa vs
  stock differ in the libgallium SONAME and an overlayfs upper cannot remove stock files — the blend
  broke EGL. Rationale: `docs/graphics.md` §5.1, `docs/images.md`.
- **The kernel goes through the distro's own EFI machinery:** a kernel RPM whose `%post` runs
  `kernel-install add` (dracut + BLS entry); the stock kernel stays one GRUB choice away.
- **Bootstrap is a one-time "install guest tools"** (`install-enhanced.sh` + the guest-tools
  tarball) run in the *stock* guest, which already has a software-GL desktop — the two-tier bootstrap
  floor. Versions: `docs/images.md` §Component versions.

**Owed:**
- Guest-tools distribution **from the app** (`limina install-guest-tools`) + the payload↔guest
  version-manifest check (`docs/design/distribution.md`).
- **virtiofs DAX** (`VirtioShmRegion`; confirm window alignment and FUSE_SETUPMAPPING on 16 KiB host
  pages; test stock-4k DAX separately) + host↔guest uid mapping.
- Images/files/HTML on the clipboard. Live risks: an NSPasteboard promised-data provider blocking
  long enough for a guest round-trip; large chunked vsock transfers vs credit flow control.
- Clipboard coverage gaps: initial offer on connect, stale-serial races, dead-peer pruning, helper
  reconnect after supervisor restart, and the ext-data-control backend under automation.

---

## Milestone 6 — Dynamic memory (balloon, min..max)

The VM gets a `min..max` range, takes memory under guest pressure and returns it to macOS when idle
with `phys_footprint` actually dropping. Design and as-built log: `docs/design/m6-dynamic-memory.md`.
Tests: `balloon.rs` (FRQ reclaim drops `phys_footprint`), `balloon_inflate.rs`, `balloon_psi.rs`,
`balloon_burst.rs`, `ledger_sweep.rs`, the `balloon_bench_s*` scenarios.

- **Why it matters (measured 2026-06-11):** host RSS is a guest-page high-water mark — 5.2 GiB idle
  → 6.8 GiB after browsing — and never comes back without a balloon. Guest idle ~2 GiB is Fedora's
  own daemons, so reclaim is the lever, not guest slimming.
- **Reclaim uses `MADV_FREE_REUSABLE`/`MADV_FREE_REUSE`**: `MADV_DONTNEED` returns nothing on macOS,
  `MADV_FREE` is lazy; REUSABLE drops `phys_footprint` even while `hv_vm_map`'d
  (`spikes/balloon-madvise`). Re-confirm on each shipping macOS.
- **4 KiB guest / 16 KiB host:** reports are coalesced to host pages host-side, so a stock 4 KiB
  guest reclaims something; a 16 KiB enhanced guest reclaims 1:1.
- **Mechanism in libkrun, policy in limina:** inflate/deflate handlers + `num_pages`/`actual` +
  config interrupts in libkrun, driven through the internal `BalloonControlHandle` and
  `--balloon-control-socket` (no C ABI); the PSI autoballoon policy in the supervisor, fed by the
  agent's `/proc/pressure/*` + MemAvailable reports.
- **`DEFLATE_ON_OOM` is not negotiated.** With the bit, Linux keeps ballooned pages in `MemTotal` as
  *used*, so a fresh VM looks out of memory; without it inflation is transparent on both tiers.
  systemd-oomd preempts the guest-side OOM net anyway. Analysis: the addendum in the design doc.
- vm.toml `hardware.memory` is the maximum; managed VMs boot dynamic `1024..MAX` and
  `hardware.reclaim` sets how hard to squeeze.

**Owed:** virtio-mem (absent from libkrun; large) stays deferred; a host-page-aware
`mm/page_reporting.c` is cheap to carry if the stock-tier coalescing waste proves material.

### Dynamic vCPU offlining — the CPU sibling of ballooning

Offline online-but-idle guest vCPUs and bring them back under load, on **both tiers**. Default
**off** (`--cpu-reclaim disabled`); `--cpus` is the maximum,
`--cpu-reclaim disabled|light|moderate|aggressive` (vm.toml `[hardware] cpu_reclaim`) sets floors of
max, ⌈max/2⌉, 2, 1.

- **Why it pays (measured 2026-09-02, 10 vCPUs, eight threads waking at 1 kHz, identical work):**

  | online | worker vCPU CPU-sec / 60 s | arch_timer/s | IPI1/s |
  |--------|---------------------------|--------------|--------|
  | 10     | 26.61, 27.06              | ~14,400      | 90–110 |
  | 4      | 21.77                     | ~10,100      | ~1,180 |

  −19% of the worker's vCPU CPU time for the same work. **The cost is timer exits, not IPIs**
  (offlining raised IPIs tenfold). A truly idle guest already costs ~1.8% of a core under NO_HZ and
  a saturated one needs every vCPU — the win is the guest in between. Guest `/proc/stat` cannot see
  this (tick-sampled). Host *wakeups/s* is a different axis and did not move
  (`spikes/wakeup-probe/RESULTS.md`).
- **Mechanism (libkrun):** PSCI `CPU_OFF` parks the vCPU thread (zero host CPU), `AFFINITY_INFO`
  answers OFF for it, and `CPU_ON` is re-deliverable at runtime over a durable per-vCPU channel (PC/X0
  reset on the owning thread — HVF register access is thread-bound), with IRQ/vtimer re-affinity.
  Without it a guest offlining a vCPU wedged the whole VMM — a two-tier fix on its own.
  Guard: `l2_vcpu_hotplug.rs`.
- **Policy (`crates/limina/src/vcpu_policy.rs`):** pure and clock-injected, `(report, now) → target`,
  with no belief about its own past, so failed writes and diverged restores self-correct.
  **Asymmetric:** shrink one CPU at a time behind a 20 s dwell (`LIMINA_VCPU_DWELL_SECS`); grow
  straight to max — a vCPU returned late is a stall the user feels. A grow needs corroboration: a
  runnable-task spike must come with real CPU burned (`busy_x100`), because `nr_running` alone
  bounced a quiet desktop to max every ~2 minutes. **The worker's own CPU time**
  (`proc_pid_rusage`) is a tier-independent grow signal that sees a burst on the next tick; it can
  veto a shrink, never cause one.
- **Guest halves:** enhanced — `limina-agent` advertises `vcpu`, pushes `CpuPressure` each ~1 s and
  writes sysfs on `CpuTarget`; stock — `qemu-guest-agent` polled (`guest-get-load` as sensor,
  `guest-get/set-vcpus` as actuator, `crates/limina/src/qga/vcpu.rs`), standing down whenever
  limina-agent reports. `guest-get-vcpus` reports `can-offline`, so the guest names its candidates.
  Measured on F44 **Enforcing**: `virt_qemu_ga_t` permits `guest-set-vcpus`.
- **Snapshots:** the online set is not in the M9 snapshot, so the supervisor re-onlines every vCPU
  inside the suspend bracket and waits (bounded) before relaying SIGTSTP. Guard `l2_vcpu_policy.rs`
  suspends through the *supervisor* (the other seam signals the worker and skips the mitigation).
  **Owed:** the snapshot-format change and a `CPU_ON` ALREADY_ON idempotency guard (#41).

#### Parking cores without hiding them — EAS instead of hotplug

Offlining makes `nproc` lie (a build started while shrunk runs with 2 jobs on a 10-CPU machine), so
the direction is to make the guest *scheduler* consolidate. The guest has no cpuidle driver (WFI is
the floor, idle already costs nothing); the saving is fewer CPUs with work on them, each paying a
1000 Hz tick. **Energy Aware Scheduling** packs work onto the cheapest CPUs that fit while every CPU
stays online. Shipped host-side, visible to a stock guest:
- **`--cpufreq`** — a `qemu,virtual-cpufreq` MMIO device (`virt-cpufreq` is a stock Fedora module):
  cpufreq policies and frequency invariance.
- **`--little-vcpus N`** — the last N vCPUs are advertised slower in their own perf domain and their
  threads run at `QOS_CLASS_BACKGROUND`. Only background produces asymmetry (utility measured 1.0x,
  background 3.75x), so the advertised capacity (273) comes from that measurement.

Traps: capacities reach the kernel only through `/cpus/cpu-map`; every CPU in one perf domain must
report the same capacity or `em_dev_register_perf_domain()` refuses it.

**Owed — the energy model**, the last gate (`sched_debug`: `pd_init: no EM found for CPU0`). Plan: a
**DKMS module** registering a synthetic EM (`em_dev_register_perf_domain()` is `EXPORT_SYMBOL_GPL`),
so EAS lands on a stock kernel; `em_rebuild_sched_domains()` is not exported, so
`echo 1 > /proc/sys/kernel/sched_energy_aware` triggers the rebuild. Also owed: the libkrun
`vcpu_sched.rs` comment that says xnu won't serve a time-constraint thread on an E-core is
contradicted by observation (banded vCPUs were seen on E-cores); the rule "don't band a little vCPU"
may stand, its stated reason needs a measurement.

---

## Milestone 7 — USB

**Established:** the **emulated xHCI controller** (`docs/design/usb-xhci.md`) is default-on and
carries our own gadgets — FIDO, the fingerprint reader, a HID keyboard for the pre-`virtio_input`
window — into a *stock* guest with zero guest components. The USB/IP round (`docs/design/m7-usb-passthrough.md`)
proved the host side (`limina-usbip`: the wire protocol byte-exact to the kernel source, a
`UsbBackend` trait, a CDC-ACM mock, a libusb backend) and the guest side: `vhci_hcd` accepts an
`AF_VSOCK` fd directly, and a mock CDC-ACM device enumerates as `/dev/ttyACM0` with no hardware
(`tests/usb.rs`). Claiming an Apple-bound device works **as root with no entitlement** (a Solo 2 via
`spikes/usb-probe/run.sh`); `com.apple.vm.device-access` is App-Store-only.

**Owed — real host-device passthrough**, as the first client of the one privileged helper
(`docs/design/privileged-helper.md`). **Decide the distribution channel first**: MAS forbids the
root helper, so host USB is either MAS-with-Apple's-entitlement or Developer-ID-with-helper, and the
two share no implementation (`docs/design/distribution.md` §2.1). Not CI-testable (root + a device).
Open questions: the macOS 26 claiming matrix (FTDI, YubiKey, mass storage, webcam, keyboard);
isochronous transfers (out of scope for v1); USB3 storage over USB/IP-vsock vs just virtiofs. A cheap
standalone win: serial-over-virtio-console for FTDI/CP210x boards.

---

## Milestone 8 — Audio, x86 emulation, desktop polish

**Established:**
- **Audio:** a native in-VMM virtio-snd device driving CoreAudio (vhost-user is Linux-only and has
  no CoreAudio backend); mic capture is opt-in behind macOS mic TCC.
- **Desktop polish** — fullscreen (`Cmd-Ctrl-F`), positional Cmd/Opt normalization (default on,
  `--no-normalize-modifiers`), system-combo capture through a consuming CGEventTap, the fullscreen
  pointer grab, the soft keyboard grab, display modes and multi-display, runtime resize, the notch.
  All of it is described in `docs/input-and-windows.md` and the design docs it points to. Scanout
  IOSurfaces go to the supervisor by Mach port (`limina-surfaceport`), not as global IOSurfaces.
- Multi-finger trackpad gestures are consumed by the WindowServer upstream of any session tap;
  options are in `docs/design/trackpad-gestures.md`.

**Owed:**
- **x86 binaries: guest-side FEX-Emu** (primary) + `qemu-user-static` via `binfmt_misc` — Rosetta for
  Linux is bound to Vz. Confirm the binfmt wiring under our launch path.
- **Fully customizable keybindings** beyond the modifier normalization.
- **CapsLock/NumLock LED parity:** the virtio-input status queue is read and discarded
  (`third_party/libkrun/src/devices/src/virtio/input/worker.rs`).
- **`virtio-rtc` for sub-second guest time (low priority).** PL031's `RTCDR` counts whole seconds,
  so both tiers land within ~1 s of the host after a resume (recorded figure ~0.14 s). `virtio-rtc`
  (mainline since 6.16) hands the guest an atomic `(host CLOCK_REALTIME, guest counter)` pair as
  `/dev/ptpN` for chrony. There is no drift to fix — the guest counter is the host counter — only the
  offset after host sleep or restore; the motivation is virtiofs mtimes confusing build tools. Not
  `ptp_kvm`: its discovery needs PSCI ≥ 1.0 and we advertise v0.2. Gates: Fedora's aarch64 kernel
  enables `CONFIG_VIRTIO_RTC`, and a measured post-resume error justifies it.

---

## Milestone 9 — Suspend / resume + full VM snapshots (host-side)

Parallels-parity suspend: freeze the guest to a file, **tear the worker down** (reclaiming host RAM,
the GPU/Metal graph, gvproxy), and resume the same desktop later; plus snapshots. Design:
`docs/design/m9-suspend-resume.md`, `docs/design/m9.2-quiesced-snapshot.md`,
`docs/design/m9-freeze-trigger.md`, `docs/design/host-sleep-s2idle.md`.

**Established:**
- **Host-side VMM snapshot, not guest S4.** Pause vCPUs, quiesce virtio, serialize vCPU + in-kernel
  GICv3 + device state + RAM, kill the worker; resume = a fresh worker with `--restore`. HVF
  round-trips the full vCPU + GIC state (`spikes/m9-hvf-state-roundtrip/RESULTS.md`; `ICC_RPR_EL1` is
  read-only, so quiesce to no-IRQ-in-service first). Guest S4 was blocked by HVF gaps and cannot do
  snapshots (`spikes/s4-hibernate/RESULTS.md`).
- **The GPU is quiesced and re-initialized, never serialized** — host-side GPU-state serialization
  exists nowhere (QEMU blocks it, crosVM stubs it, Vz refuses it). Suspend-with-3D works when the
  resource graph is guest-backed; a live guest does *not* survive abrupt GPU loss, so the guest side
  must resubmit.
- **The production path is the s2idle bracket** (SIGTSTP): the guest quiesces virtio to INIT, the
  snapshot is taken, exit 126; the restore re-establishes the renderer from a journal. Reboot
  relaunch and suspend share the relaunch spine. Measured: 6.6 s save / 465 MiB / 2.3 s restore apply.
- A restore must not cycle the display connector; the raw `SIGUSR1` snapshot is an L1 test vehicle
  that dumps an unquiesced guest.
- HVF has no dirty-page log, so a snapshot is a stop-the-world RAM dump — fine for suspend, a UX
  note for snapshotting a live VM (`hv_vm_protect` DIY dirty-logging is a later option).

**Owed:**
- **Guard the two ad-hoc footguns** (`spikes/suspend-resume-adhoc/`): restoring an unbracketed
  (`SIGUSR1`) snapshot livelocks the guest, so the raw trigger should bracket first or the restore
  should refuse it; and an ad-hoc `--disk` run with `--snapshot-file` resolves window close to
  shutdown. No L2 covers a snapshot round-trip under dynamic memory + FRQ.
- **Guest-kernel virtio-gpu PM ops (low priority):** `virtgpu_drv.c` has no `.freeze`/`.restore`, so
  every thaw takes the bus fallback (reset → renegotiate with no queue re-programming); the host-side
  leniencies cover stock guests, which a kernel fix never can. Carry the Dongwon Kim freeze/restore
  series on the enhanced kernel and report the core gap upstream.
- Named snapshots (save/restore/clone/roll back) as a user feature on top of the mechanism, plus the
  multi-disk snapshot manifest (M10).

---

## Milestone 10 — Additional block devices

Design: `docs/design/m10-multiple-disks.md`. **Established:** repeatable
`--disk PATH[:ro][:create=SIZE]` (attach order = device order, first → `vda`), sparse creation,
writable-image `flock`, stable identity (the virtio serial is the `block_id` →
`/dev/disk/by-id/virtio-<id>`), qcow2 data disks (detected by magic), and `--cdrom`: an EFI aarch64
ISO attached as a read-only virtio-blk disk boots through our firmware (El Torito → ESP →
`BOOTAA64.EFI`), zero code (`tests/disks.rs::boots_efi_iso_to_bootloader`). Both shipping tiers boot
BLS `root=UUID=`, so attaching disks cannot shift root. A tail-reaching discard punch-holes instead of
truncating (the imago fork).

**Owed:** host-managed `BootOrder` via a baked EFI varstore (unattended installs, deterministic boot
with two bootable disks — attach order does not decide which disk boots); the multi-disk snapshot
manifest (adding a disk renumbers the trailing vsock/net devices); moving the dev direct-kernel path
from `root=/dev/vda3` to PARTUUID.

---

## Milestone 11 — Build, dev and delivery ergonomics

`cargo xtask <cmd>` (`xtask/src/main.rs`; `--help` lists it) is the one-command surface over the
tested scripts, which stay the source of truth: `setup`, `vendor`, `build`, `sign`, `test`, `run`,
`app`, `bundle`. CLAUDE.md describes each; onboarding is `docs/dev-onboarding.md`. The heavy
container builds (guest mesa, KRUN_EFI, kernels, the build image) stay scripts.

**Owed:** notarized distribution (Developer ID; `docs/design/distribution.md`); folding `bundle` into
`app` (`docs/hardening-backlog.md`).

---

## Milestone 12 — SPICE guest agent (stock-tier clipboard first)

Light up the stock `spice-vdagent` that default Fedora Workstation installs already carry, so a guest
with none of our components gets clipboard, then client→guest file transfer. Display resize is out of
scope — limina's EDID/mode machinery already does it natively.

**Established:**
- **The trigger is a named port.** Exposing a virtio-serial port named `com.redhat.spice.0` is
  enough: the stock udev rule (`70-spice-vdagentd.rules`) pulls `spice-vdagentd.socket`, and
  `PortConfig::InOut { name, .. }` already announces port names — no new libkrun device
  (`spikes/m12-spice-port/RESULTS.md`). vdagentd needs a graphical session. Reopening any port used
  to abort the worker; fixed RED-first (`l1_port_reopen.rs`).
- **We are the broker.** The vdagent framing assumes a SPICE server; limina implements the host end
  itself (`crates/limina/src/vdagent/`) behind M5's single pasteboard owner — no SPICE server, no
  GPLv3 crate.
- **Announce once per port open, never on a timer:** vdagentd reads every announce as a new client
  and resets clipboard state.
- **spice-vdagent's clipboard is X11-only**, so on Wayland it rides XWayland + the compositor's
  X11↔Wayland selection bridging. Coverage by session:

  | session | X11↔Wayland bridging | ext-data-control | carried by |
  |---|---|---|---|
  | GNOME / mutter | yes | never | vdagent |
  | KDE, sway, Hyprland, wlroots | yes | yes | either — they overlap |
  | niri, synoik (`xwayland-satellite`) | no (focus-gated) | yes | our helper |

  xwayland-satellite pushes X11→Wayland only after an X window has had keyboard focus, so in an
  all-Wayland session vdagent is structurally blind. mutter will not ship data-control (mutter#3941).
- **Arbitration lives in the guest, per session**: each `limina-agent-session` decides whether to
  claim the clipboard, and the host routes to the SPICE transport plus whichever peers claimed.
  vdagentd serves only the logind-active session; the chosen shape is that native claims in inactive
  sessions and SPICE serves the active one.
- Traps: `wl-copy` over ssh does not set the clipboard (no input serial) — verify with `wl-paste`,
  or use `xclip` on `DISPLAY=:0`.

**Owed (sequenced: synoik gains selection bridging first; these stay booked):**
- **Invert the probe** — claim when our own `ext_data_control_manager_v1` bind succeeds, not when
  vdagent is merely alive (`guest/limina-agent-session/src/vdagent.rs`); liveness only as tiebreak.
- **A mute for vdagent**, needed once the probe is inverted: announce with `request=1` and without
  `VD_AGENT_CAP_CLIPBOARD_BY_DEMAND`, which makes vdagentd disconnect the client and the session agent
  release X selection ownership; re-announce with the bits to hand it back; skip `broker.host_copy()`
  while muted. Edge-triggered, per-VM while claims are per-session.
- **File transfer** (`VD_AGENT_FILE_XFER_*`) from an AppKit drop target / "Send File…", honouring the
  guest's decline/cancel.
- Done tests: an unmodified Fedora copies both ways with no limina components; one guest with a GNOME
  and a niri session copies in both with exactly one owner per session.

---

## Milestone 12.5 — QEMU guest agent (stock tier)

Expose `org.qemu.guest_agent.0` and use the `qemu-guest-agent` a stock guest already has. Code:
`crates/limina/src/qga/` (`codec`, `client`, `policy`, `trim`, `bootstrap`, `vcpu`).

**Load-bearing facts (measured 2026-08-26, F44):**
- Fedora desktops install it by default (`guest-desktop-agents`, mandatory in every desktop
  environment); Debian installs neither it nor spice-vdagent.
- The trigger is the port name (`99-qemu-guest-agent.rules`); exposing the port starts the agent.
- **Fedora blocks no RPC** (`--block-rpcs` commented out; 43 commands `enabled`) and confines the
  domain instead: `virt_qemu_ga_t` cannot write `bin_t`, reach systemd's D-Bus, or touch
  `user_home_t`. `enabled` means "will attempt", never "will be allowed to finish" — measure each verb.
- `guest-set-time` works against our PL031 (its `RTCLR` write stores an offset from host wallclock).

### Step 1 — the clock
The fallback rides the `limina-timesync` thread (oversleep detector + periodic tick) and fires only
when no `timesync`-capable peer took the message, so the enhanced tier always wins. A stock guest that
stays running across a host nap otherwise had no corrector at all.

### Step 2 — lifecycle + inventory
- The stop ladder: agent (5 s) → GPIO power button (5 s) → `guest-shutdown` → wait. The QGA rung
  comes up only after a probe already succeeded and only if the grace can hold it. A guest that
  accepted the request gets `QGA_GRACE` (45 s; `shutdown -P +0` took ~28 s on a seated F44).
- **An ordinary stop never kills the guest.** Every rung is a request; the grace is a reporting
  deadline. Killing is an explicit human act (a second stop signal, `limina stop --force`, Force Stop).
  `l1_stop_never_kills` pins it.
- Inventory (OS, kernel, hostname, IPv4, users, filesystems) is logged once when the agent answers.

### Step 3 — giving disk back
`guest-fstrim` on a 6 h cadence (`crates/limina/src/qga/trim.rs`, `LIMINA_QGA_TRIM_SECS`, `0` = off).
Fedora already mounts btrfs `discard=async` and runs `fstrim.timer`; a trim recovers the residue
(958 MiB, ~6%, on a weeks-old image — `spikes/qga-fstrim/RESULTS.md`), more on guests that discard
nothing by default. Gated on a calm host and a guest not doing its own IO; a missing PSI reading means
"no reason to wait". `l2_qga_fstrim` asserts on `st_blocks`, never on `fstrim -v` (ranges walked).
**`guest-fsfreeze-*` is rejected by measurement** (`spikes/qga-fsfreeze/RESULTS.md`): a frozen root
deadlocks the s2idle bracket and cannot be thawed from inside; if a disk-level consumer ever needs it,
an unconditional thaw-on-attach is mandatory (qemu-ga never auto-thaws and its state file survives a
cold boot).

### Step 4 — bootstrapping the enhanced tier through the port
`crates/limina/src/qga/bootstrap.rs` delivers a small **kit** (`LIMINA_QGA_DEPLOY`, env-only for now)
via `guest-file-write` + `guest-exec` — the agent, its unit, an `install.sh` run as root — enough to
fetch the rest the ordinary way; a growing kit is the signal to bootstrap a fetcher instead. SSH keys
go in through `guest-ssh-add-authorized-keys`. It fires only when no `limina-agent/` peer connected
within `LIMINA_QGA_DEPLOY_AFTER` (120 s). **Scope: an unconfined agent** — on Enforcing Fedora the
domain cannot lift its own confinement, so those guests keep SSH as their delivery path.
`l2_qga_bootstrap` deletes the enhanced agent outright before deploying, and sha256-checks the result.

### What is left
`guest-get-diskstats`, `guest-get-cpustats` and the memory-block commands have no consumer; adding
them before something reads them is log noise the stock tier pays for. Twenty-two of the forty-three
commands are in use; the rest are unused by choice (`guest-suspend-*` because the bracket owns
suspend).

---

## Milestone 13 — Visibility- and power-aware render adaptation

**Not started; design only.** Scale render/present work to what is needed now: throttle hard when
the guest's output is not seen (occluded, another Space, minimized), cap or relax on battery / Low
Power Mode, and cost nothing when the window is live on AC. Resume must be instant and clean. The
rendering sibling of M6's "give back what you are not using".

**Signals** (AppKit front-end; policy in limina): `NSWindow.occlusionState` + `isOnActiveSpace`
collapse into one "output not visible" signal; display context (fullscreen, which display); host power
— the IOKit power-source read exists (`crates/limina-vmm/src/krun/battery.rs`) but is pull-only, so
add a notification-driven listener modelled on the host-sleep one (`crates/limina-vmm/src/power.rs`)
plus `NSProcessInfo.isLowPowerModeEnabled`; later, thermal pressure.

**Policy:** a hysteresis state machine from `(visible, power)` to a render budget, configured in
`vm.toml` with a disable switch — full rate when visible on AC, a hard throttle (a few fps or paused)
when not visible, a cap when visible on battery, most aggressive when both. Hysteresis and cooldown
against Space-flip oscillation (the balloon's oscillation lessons apply).

**Mechanism, cheapest first:**
1. **Host present cap/pause** (stock-safe): presents are event-driven with a 60 Hz fallback timer
   and no cap; add a host→worker knob on the existing per-worker control-socket seam. The s2idle GPU
   park is related but is not a general present-pause toggle.
2. **Guest backpressure by pacing the fence release**: the fence-accurate present path holds the
   guest's `RESOURCE_FLUSH` fence until the frame is shown; delaying that release slows the guest's
   own frame loop, throttling guest CPU/GPU and not just host compositing.
3. **Ring relax depth as a function of `(visible, vsync-capped, power)`.** Measured on the C renderer
   (`spikes/venus-ring-doorbell/RESULTS.md`, `spikes/wakeup-probe/RESULTS.md`): most idle-ring
   poll-sleeps are the walk across the warm plateau on every inter-frame gap; a capped, occluded or
   battery ring can coarsen safely (its added ≤640 µs is hidden by the frame budget), while an
   uncapped submit-latency-bound ring (vkmark) must keep the full plateau. A per-ring classifier
   by *fraction of wall-clock spent in long (≥2 ms) idle gaps* separated the two (vkcube −70%
   poll-sleeps, vkmark unchanged); a gap-*count* classifier cost vkmark −44%. There is no separate
   doorbell mechanism to build — the wakeup-suppression handshake is already race-free. **virglrs
   runs a fixed plateau** (`third_party/virglrs/src/venus/ring_thread.rs` `relax`) — the adaptive
   depth was not ported — so this knob starts from porting it. Ship guardrail: a vkmark A/B.
4. **Guest-cooperative throttle** (enhanced): a host→guest target-rate message → `limina-agent` →
   a compositor frame-rate hint; degrades to 1–3 without it.

**Constraints:** never throttle audio, mic, the control plane or networking; compose with s2idle,
snapshot, display resize and M6 as one idle posture; detection must hold across Spaces, minimize,
fullscreen on another display and multi-display. **Done test:** occluded → host GPU/CPU/wakeups drop
sharply and recover instantly on focus; on battery a measurable cap; a stock guest still throttles
host-side.

---

## Milestone 14 — Biometrics: host Touch ID → guest passkeys + fingerprint login

**Established:** raw sensor passthrough is impossible at any privilege level (the sensor is wired to
the Secure Enclave), so this is an **auth service**. The host is a CTAP2 authenticator backed by the
SEP (ES256 keys created in the enclave, a Touch ID sheet per assertion naming the VM and RP);
credentials are device-bound, namespaced per VM, attestation self/none. It reaches the guest as USB
gadgets on the emulated xHCI (default-on), so a **stock** guest gets both halves with no components:
- an honest **FIDO HID key** (usage page 0xF1D0) — browser passkeys and `sk-*` SSH keys
  (`docs/fido-authenticator.md`; worker gadget = thin CTAPHID transport over a socket to the
  supervisor's one authenticator/store);
- an **impersonated match-on-chip fingerprint reader** (libfprint's elanmoc; the driver source is
  the spec) so stock fprintd / GNOME Settings / GDM light up; match-on-chip only, fwupd neutralized
  by an impossibly high firmware version (`docs/design/usb-moc-fingerprint.md`,
  `docs/fingerprint-reader.md`).

The agent's uhid device is the fallback where USB is off. Facts that are traps: zero-entitlement
signing works for the enclave path; the data-protection keychain needs profile-backed entitlements
and a plain build is AMFI-killed, so credentials persist as CryptoKit `dataRepresentation` blobs in
per-VM state; one authenticator per guest (the host withholds the agent's `fido` cap when the gadget
serves); a silent `up:false` pre-flight must succeed. `hmac-secret` cannot live in the SEP.

**Owed:**
- An **L2 FIDO guard** (a `fido2-assert` round-trip with a test-only Touch ID bypass).
- `CTAPHID_CANCEL` is unhandled and the SEP signature is not cancellable, so a browser abort leaves
  the sheet up.
- Risks still open: Dock-launch LAContext prompts, multi-VM prompt attribution, enroll-stage UX.

### Follow-up: one passkey identity across host and guest

**Wanted:** guest Firefox signs in with the same passkey as host Firefox — **per-VM configurable**,
since many users want the guest deliberately separate (design the `vm.toml` switch in from the
start; today's behaviour is the isolated end). **The obvious route is closed:** sharing host passkeys
means asking macOS's own authenticator, whose escape hatch for arbitrary RPs
(`com.apple.developer.web-browser.public-key-credential`) Apple grants only to browsers; and Apple
attestation cannot be relayed (`SecKeyCreateAttestation` is absent from the public SDK; App Attest
binds our App ID). **The route we control inverts it:** present limina's enclave authenticator to the
*host* as a virtual HID device via DriverKit (`com.apple.developer.driverkit.transport.hid`), so both
sides share one store. **Unverified** whether browsers accept a DriverKit HID device as a security key
and what that entitlement requires — spike first. Sharing makes the per-VM switch load-bearing: a
guest could then *ask* for signatures on host credentials, each still gated by a sheet naming the VM.

---

## Milestone 15 — Virtual display pipeline v2: native refresh, hardware planes, scanout formats

The host half of the guest compositor's frame-budget work (120 Hz = 8.33 ms, 144 Hz = 6.94 ms):
real refresh targets, cheaper scanout, hardware planes for video. Fence feedback is truthful end to
end (the fence-present chain is default-on; the shown-ack separates "new frame at glass" from "old
buffer free"), so exposing real display timing to the guest is meaningful. The model, traps and the
multi-display policy live in `docs/graphics.md` and `docs/input-and-windows.md`.

### Wave 1 — per-host-display virtual displays with native refresh

**Established:**
- **Stable EDID identity + connector events** (`docs/design/stable-edid-hotplug.md`,
  `crates/limina-displayctl`, `crates/limina/src/window/hostdisplay.rs`, `l1_edid.rs`): the guest
  gets the identity, density and refresh of the host panel the window is on, re-pushed on migration;
  the range descriptor in the form `drm_get_monitor_range` accepts; real disconnect/reconnect.
- **DisplayID 2.0 type VII + HiDPI:** a base EDID timing tops out at 655.35 MHz, so a Retina panel
  at device pixels (3024x1964 @ 120 Hz, ~866 MHz) rides a DisplayID block (a CTA-861 block would not
  help — same 16-bit clock field). `[display] hidpi` (default on) drives the guest at device pixels.
- **Several virtual displays:** a boot-time scanout pool (`--display-pool`, default 4 windowed), a
  slot table where a host panel owns a connector stably by `panel_key` (so the guest's saved
  per-monitor config still matches), fullscreen across every panel, a Displays menu
  (`crates/limina/src/window/displays.rs`).
- **Arrangement relay** (`docs/design/arrangement-relay.md`, `crates/limina/src/window/arrangement.rs`,
  `l2_arrangement.rs`, enhanced tier): positions computed from the host arrangement and validated
  against mutter's adjacency rules (a clean full set or nothing), carried as `GET_DISPLAY_INFO` r.x/r.y
  and exposed by the enhanced kernel as DRM `suggested X/Y` **plus `hotplug_mode_update`** (mutter
  ignores the offsets without it). Ordering rules: the whole suggested set before the connect's
  hotplug; one config-change event per batch, the device holding further events until the guest acks
  (`l1_multidisplay::l1_b_back_to_back_updates_survive_the_ack_race`); never push a position to a slot
  already at the default. The guest-reported logical rects correct the predicted metric; mutter ≥ 50
  applies corrections only at a set's first appearance or the next seat.

**Owed:** **120 Hz ProMotion / VRR** — one display per host panel advertising what it supports, with
the VRR range and `vrr_capable` reaching the guest (CA latch cadence is variable); a fuller DisplayID
mode list; boot-time EDID. Done test: a seated guest on the MacBook panel enumerates a VRR-capable
120 Hz mode and its frame clock tracks real latches.

### Wave 2 — overlay planes on virtio-gpu

virtio-gpu KMS exposes primary + cursor only. Overlay planes need a device + guest-kernel protocol
extension (we own both ends; upstreamable); host-side a plane maps onto a CALayer. Capset-gated, so
stock guests keep two planes. Fold in **device-advertised plane formats/modifiers**: the guest driver
hardcodes its format list (`virtgpu_plane.c`), which is why every new format costs a kernel patch; a
device→driver query makes the hardcoded list the fallback.

### Wave 3 — NV12/P010 + `COLOR_ENCODING`/`COLOR_RANGE` on those planes

The payoff is video — scanning out decoder output with no conversion pass (CA scans out biplanar YUV
IOSurfaces natively). Coupled to the video work (M17). Overlay planes are an adjunct for
video/fullscreen, not a general compositing speedup.

### Wave 4 — primary-plane scanout formats

Spike closed (`spikes/scanout-modifiers/`): non-LINEAR modifiers buy nothing; rendering directly
into LINEAR/IOSurface scanout is the win; advertising `XBGR8888`/`ABGR8888` on the primary plane
shipped (the enhanced kernel), killing the swizzle half of the compositor's present blit.

### Wave 5 — small items

- **Cursors larger than 64×64** (low priority; the guest's bigger win is leaving the software cursor
  via `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`).
- **Price WindowServer's GPU share while the guest composites 4K** (Metal counters / `powermetrics`).

### Wave 6 — zero-copy udmabuf import into venus

A udmabuf wraps guest anonymous pages (a sealed memfd) as a dmabuf — how every software-decoded frame
reaches the GPU stack.

**Established (phase 1):** a PRIME-imported udmabuf reaches the host as a guest-memory blob that
libkrun translates into host-VA iovecs; guest kernel and guest mesa record and report its `blob_mem`
and let planar YUV reach the sampler; the renderer types it as a real GL texture, fills it from the
guest's pages and re-reads before every batch that samples it. Frames reach the GPU with **one
host-side copy**. The naming symptom of a break is `Illegal resource`.

**Owed:**
- **Phase 2 — venus imports an iov-backed resource.** The pages are visible in the worker; what is
  missing is contiguity. Stitch them into one host VA with `mach_vm_remap` (share, don't copy) and
  import through the host-pointer route (`VK_EXT_external_memory_host` → KK →
  `newBufferWithBytesNoCopy`).
- **Phase 3 — pinning and granularity.** A remap needs each 16 KiB-aligned buffer offset on 16 KiB
  aligned, contiguous backing — **guest page size is the wrong variable**: a 16k guest satisfies it,
  but so do buddy-order-2 allocations and THP/hugetlb memfds on a stock 4 KiB guest. Make the
  granularity a hint, and have the host remap qualifying slots and `memcpy` only the ragged remainder,
  so degradation is proportional (a copied slot is a snapshot — fine for write-once frames). Pages
  Metal has wired must be pinned against the balloon/FRQ path for the resource's lifetime — reconcile
  with `docs/design/m6-dynamic-memory.md` before writing phase 2. Rationale:
  `docs/design/16k-page-requirement.md`.
- Done test: `vkudmabufimport.py` reports `IMPORT OK` + `ALIAS OK` (a GPU copy reads back the memfd's
  pattern), zero guest-side copies, a measured before/after on software-decoded playback — including a
  stock 4k guest on a THP-backed memfd, tested rather than assumed.

---

## Milestone 16 — LiminaOS: a purpose-built guest distribution + system compositor (moonshot)

The distro prototype boots (the boot chain and image model below are built and proven on real
firmware); the compositor phase is unscheduled. **Build detail's plan of record is
`~/Projects/LiminaOS/README.md` on the LiminaOS build VM**; this section records the *decisions*, and
where the two disagree on build detail the README wins.

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
    our own build-it-in rule, so the initrd needs **no modules at all** (one that ships the module
    tree came to 64.9 MB — an initrd whose only job is to reach the filesystem containing that tree).
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
  (e.g. `DM_VERITY_VERIFY_ROOTHASH_SIG_SECONDARY_KEYRING` needing `SECONDARY_TRUSTED_KEYRING`;
  `VIRTIO_VSOCKETS` needing a `VSOCKETS` parent) — each would otherwise surface far from the cause.
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
    confirmation.)
- **Per-VM secrets ride SMBIOS Type 11, not the cmdline.** Shipped host-side:
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

The distro leg was prototyped first on a rule worth reusing: *answer the cheap plan-killer
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
  rollback — the gating unknown for the restart story.
- (b) Settled: the boot chain boots on real firmware with our own 16 KiB kernel; offline
  unprivileged `systemd-repart` works; the kernel builds byte-identically across 4 KiB and 16 KiB
  build hosts — an invariant worth wiring into CI, since anything that breaks it is a regression in
  *something* even when everything still boots.
- (c) **passthrough latency** — prove the system compositor adds zero frames on the fullscreen path
  (gamescope's problem) before building the animation layer on top.
- (d) **16 KiB-clean userspace is only half-verified.** fdsdk's toolchain (gcc/binutils/glibc) is
  clean, but **Mesa and the graphics stack have not been built** — which is exactly where 16 KiB
  assumptions historically live, and exactly what the venus tier needs. Do not inherit "fdsdk is
  16k clean" without this qualifier.

---

## Milestone 17 — Video: finish decode, then encode

**Established:** hardware decode of VP9 profile 0, AV1 main (M3+ hosts), H.264 Baseline/Main/High
and HEVC Main runs on virglrs's VideoToolbox backend (`docs/graphics.md` §video,
`docs/design/av1-decode.md`, `docs/design/h264-hevc-decode.md`). What sets each codec's cost is how
much bitstream survives the guest's VA frontend: AV1's frame header is discarded, so it needs a full
serializer and a reference-slot model; H.264/HEVC slices arrive whole (mesa prepends a start code and
passes the buffer verbatim), so only the parameter sets are synthesized.

**The guest gate is mesa's build**, in `auxiliary/vl/vl_codec.c`, shared by every gallium driver —
no host capability can conjure a codec the guest mesa was not built with. Fedora builds the default
`all_free` (`av1dec, av1enc, vp9dec, mpeg12dec, jpegdec`), so stock guests get H.264/HEVC only from
RPM Fusion's `mesa-va-drivers-freeworld` (libva probes `/usr/lib64/dri-freeworld/` first). **Our
enhanced mesa enables `h264dec,h265dec`** (`scripts/provision/f44/build-mesa-rpm.sh`) — an
obligation, not an option: otherwise installing our components would take away a capability a stock
guest with RPM Fusion has. `fedora-cisco-openh264` is a software codec with no VA driver and does
nothing for offload.

**Owed, in order:**
1. **MJPEG decode** — `jpegdec` is already in stock mesa, so this is stock-tier by construction and
   host-side only; each picture is an independent JPEG with no references.
2. **The AV1 super-resolution refusal** must move after submission, and the dav1d fallback is a
   decision to make (`docs/design/av1-decode.md` §Super-resolution).
3. **Encode** — plumbed everywhere except our backend (the protocol carries
   `PIPE_VIDEO_ENTRYPOINT_ENCODE`; `virgl_video_hw.h` defines the H.264/H.265 encode descriptors; the
   guest driver drives it); virglrs advertises no encode entrypoint. VideoToolbox *hands us* the
   parameter sets (`CMVideoFormatDescriptionGetH264ParameterSetAtIndex`), so no synthesis; the cost is
   lifecycle — `VTCompressionSession` is asynchronous where decode leans on in-order delivery — and
   mapping the guest's rate-control/GOP descriptor onto VT properties. Consumers:
   gnome-remote-desktop screen sharing and OBS, both over VA-API encode.

The standing rule for all of it: spike the gating unknown before building on it.
