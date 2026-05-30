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

## Milestone 1 — Boot Fedora-Workstation-43.raw to a serial console

**Goal:** `limina run Fedora-Workstation-43.raw` boots the distro's own kernel via EFI and reaches a
login prompt on the host terminal (serial console). No window, no GPU, no real NIC. This is the
smallest end-to-end path that exercises HVF + disk + console + entitlement.

**Key tasks (concrete):**
1. **Build/vendor libkrun-efi.** Build `third_party/libkrun` with `make GPU=1 INPUT=1 NET=1 BLK=1`.
   For the firmware blob: Homebrew already ships `/opt/homebrew/lib/libkrunfw-efi.dylib`
   (confirmed present) — this is the EDK2/EFI guest firmware libkrun links so the guest can boot a
   distro kernel off the ESP. Build/link the `-efi` flavor (libkrun built against libkrunfw-efi
   rather than the bundled-Linux libkrunfw). The gpu+input features can be compiled in now even
   though M1 does not use them — keeps one library for the whole product. Verify with `nm -gU` that
   the resulting dylib exports `krun_set_firmware`, `krun_add_disk2`, `krun_set_console_output`.
2. **Minimal limina binary / `krun-sys` bindings.** Crate that links the vendored libkrun and exposes
   the C API. The M1 launch sequence:
   - `krun_create_ctx()` -> ctx_id
   - `krun_set_vm_config(ctx, vcpus=4, ram_mib=4096)` (static RAM; dynamic memory is M6)
   - `krun_set_firmware(ctx, <path to EFI firmware>)` — boots the distro kernel from the ESP.
     (No `efi` Cargo feature exists; `krun_set_firmware` is gated only by `not(tee)`.)
   - `krun_add_disk2(ctx, "root", "Fedora-Workstation-43.raw", DISK_FORMAT_RAW, read_only=false)`
     — the image is a multi-GiB MBR/GPT disk with an EFI partition; EFI+disk lets the distro boot
     its own kernel/initramfs/drivers, sidestepping the bundled-kernel `root_disk_remount` path.
   - `krun_set_console_output(ctx, ...)` and/or default console to the controlling TTY so the guest
     serial console is visible in the host terminal.
   - **Networking: TSI default** (zero glue). Optionally `krun_set_port_map` BEFORE adding any net
     device. Do NOT add a virtio-net device yet (that is M3).
   - `krun_start_enter(ctx)` in the child VMM process.
3. **Codesign** the limina executable with `com.apple.security.hypervisor` via `hvf-entitlements.plist`
   (`codesign --entitlements hvf-entitlements.plist -s - limina`). Wire this into the build so every
   binary that calls HVF is signed.
4. **Child-process supervision skeleton.** GUI/CLI parent spawns the signed VMM child, forwards the
   console, and treats child exit (PSCI shutdown -> process teardown) as VM-stopped.

**libkrun patches:** none required for the happy path. Stretch: harden the `panic!` exit paths a
real Fedora boot may hit — `hvf/lib.rs:549` (unknown PSCI), `:595-602` (unknown exit reason),
`:728` (unknown ESR_EL2 EC). Convert panics to logged graceful stops once we see which fire.

**Done test:** From a clean terminal, `limina run Fedora-Workstation-43.raw` prints the EFI +
kernel boot log and reaches the Fedora login / `getty` prompt on the serial console; entering
`poweroff` shuts the guest down and the limina child process exits 0.

**Risks / spike first:**
- **Boot path is the #1 risk.** Spike EFI+disk first. If the firmware does not find/boot the ESP,
  fall back to `krun_add_disk` + `krun_set_root_disk_remount(device, fstype, options)` — but Fedora
  rootfs is btrfs and may be on LVM / multiple partitions, which breaks the simple `/dev/vdaN`
  remount assumption. Audit the actual partition layout of the `.raw` before relying on remount.
- Confirm `libkrunfw-efi.dylib` is the firmware libkrun expects and how the `-efi` build selects it
  (vs the bundled-Linux `libkrunfw`). If brew's `-efi` blob is stale/incompatible, build it from
  `third_party/libkrunfw` (libkrunfw-efi / EDK2).
- Verify `krun_has_feature(KRUN_FEATURE_BLK)` / `NET` on whichever libkrun we link.

---

## Milestone 2 — Display + input (native Metal window)

**Goal:** A native macOS window shows the guest framebuffer (2D scanout) and a keyboard + pointer
work. Fedora boots to a graphical login (llvmpipe/software GL is fine — 3D is M4).

**Key tasks:**
1. **Native NSWindow + CAMetalLayer display backend** implementing the verified
   `krun_display_backend` vtable (`libkrun_display.h`): `configure_scanout`, `disable_scanout`,
   `alloc_frame`, `present_frame`. Written in Rust against `krun-sys`. `alloc_frame` returns a
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
- Cursor: UPDATE/MOVE_CURSOR are unimplemented (panic) — use a software-composited guest cursor for
  M2; hardware cursor is an M8 patch.

---

## Milestone 3 — Networking (NAT, then bridged)

**Goal:** Real virtio-net NIC with outbound internet and DNS via user-mode NAT; bridged as an
opt-in later sub-step.

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

## Milestone 4 — 3D acceleration (Venus)

**Goal:** Hardware-accelerated 3D in the guest: GNOME runs on real GPU, GL apps work via Mesa zink.

**Key tasks:**
1. **Reuse upstream Venus(Vulkan) -> MoltenVK -> Metal.** Build libkrun `--features gpu` and pass
   `virgl_flags = USE_EGL | VENUS | RENDER_SERVER | THREAD_SYNC | USE_ASYNC_FENCE_CB` (matching the
   in-tree gui_vm macOS config; EGL/RENDER_SERVER are effective no-ops on macOS virglrenderer). Do
   NOT set `NO_VIRGL`, `DRM`, or `USE_EXTERNAL_BLOB`. Host virgl **GL** is a dead end on Apple
   Silicon — desktop GL apps go through in-guest Mesa **zink** (GL->VK->Venus).
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
- **Spike #1: does Homebrew virglrenderer carry the Apple blob patches?** Decides whether MAP_BLOB
  and Venus host-visible memory work at all. Test with `examples/gpu_vulkan.c` early.
- Does Fedora 43 Mesa 25.2 auto-select venus and does zink-on-venus accelerate GNOME/Firefox or fall
  back to llvmpipe? Verify in the actual `.raw`.
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
| M1 boot | CLI, krun-sys, child supervisor, codesign | (optional) harden panic exit paths |
| M2 display+input | Metal display backend, input provider, kVK->KEY table | (likely) Darwin input-worker epoll/eventfd shim |
| M3 networking | gvproxy supervision; bridged helper integration | (optional) worker.rs reconnect-on-HANG_UP |
| M4 3D | virgl flags wiring; IOSurface present-texture backend | virglrenderer Apple-blob fork build; SET_SCANOUT_BLOB accept path + display-vtable surface-export callback (zero-copy scanout) |
| M5 clipboard/fs/agent | guest agent, liminad, NSPasteboard bridge | none for transport (vsock+virtiofs exist) |
| M6 dynamic memory | PSI autoballoon policy | reclaim fix (MADV_FREE_REUSABLE — spike-confirmed) + 16KiB align + inflate/deflate + krun_*balloon* API + DEFLATE_ON_OOM |
| M7 USB | host claim/attach, usbip plumbing | libkrunfw kernel rebuild (USB on); later native virtio-usb + krun_add_usb* |
| M8 audio/x86/polish | fullscreen, keymap, multi-display, FEX wiring | native virtio-snd; runtime resize/EDID; hw cursor; LED parity (zero-copy scanout already landed in M4) |

## First three things to spike (highest uncertainty, gate the most)

1. **M1 boot path:** EFI+disk vs root_disk_remount against the real Fedora `.raw` layout (btrfs/LVM).
2. **M2 input worker on macOS:** does `--features input`'s epoll/eventfd shim build and wake on
   Darwin arm64?
3. **M6 reclaim:** does `MADV_FREE_REUSABLE` actually drop `phys_footprint` on an `hv_vm_map`'d
   region? (Decides whether dynamic memory is feasible at all.)
