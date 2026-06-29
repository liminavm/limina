# Hardening / finish-what's-shipped backlog

Consolidated "finish what's shipped" punch-list (cross-cutting, drawn from the milestone sections in
`docs/roadmap.md`). These are loose ends on **already-shipped** milestones (M1–M5) plus the cheap M8
polish wins — closing them rather than opening a new milestone. Each entry points at the roadmap
section / file where it's detailed. Prioritized roughly by user-visible value; pick top-down or by
appetite.

Done first (2026-06-23, with user): **runtime window resize** — ✅ SHIPPED, see below.

## Display / window
- **Runtime window resize / EDID hotplug** — ✅ SHIPPED 2026-06-23 (all 4 layers; L1 sysfs test GREEN
  + windowed-VM log-verified, guest re-modesets with no oscillation). Resizing the limina window
  reflows the guest resolution, no reboot. Design + as-built notes:
  `docs/design/runtime-display-resize.md`; memory `limina-display-resize`. libkrun patches 0025/0026.
- **Capability-scope the scanout IOSurfaces** (security) — ✅ **DONE 2026-06-23 (sw2d + venus)**.
  The worker used to export each scanout as a machine-global `IOSurfaceID` any same-user process
  could brute-force-read (`spikes/venus-draw-probe/iosdump.swift` PoC). Now **both** display paths
  create their scanout/cursor IOSurfaces **non-global** and hand each one's Mach port to the
  supervisor (`limina-surfaceport`: `SurfacePortSender`/`Receiver`, bootstrap rendezvous), keyed by
  id; the supervisor resolves ids from the Mach map. `LIMINA_GLOBAL_SCANOUT=1` re-enables global for
  the debug oracle.
  - **sw2d/baseline path** — the `limina-display` `WindowBackend` publishes its ring. Spike
    `spikes/iosurface-machport`; commits `5980de2 8cafa44 13c6428 383460e`; RED-first test
    `non_global_scanout_is_hidden_from_strangers`.
  - **venus zero-copy path** — the renderer runs in the worker process, so `vkr_mtl_iosurface_alloc`
    publishes directly to the same receiver (no rutabaga/krun_display/virtio_gpu FFI). Patch
    `spikes/virgl-zink-kk/patches/virglrenderer-venus-iosurface-scoping.patch`;
    `LIMINA_SURFACE_PORT_NAME` env from `--surface-port-name`; commit `138d7f6`. Verified live
    (dev-enh + KosmicKrisp): the exact venus-allocated scanout ids (from `SET_SCANOUT_BLOB`) return
    "not alive" to a stranger while a `LIMINA_GLOBAL_SCANOUT=1` contrast dumps the screen; pure
    zero-copy (`LIMINA_PRESENT_COPY=0`) presents them via the Mach map with 0 "unresolved" skips.
- **CapsLock/NumLock LED parity** — surface the statusq LED feedback (libkrun `worker.rs` no-op).
  Roadmap M8.

## Lifecycle robustness
- **Windowed guest reboot** — ✅ **DONE (shipped `efa285f` 2026-06-13, verified live 2026-06-23).**
  A guest reboot in a window keeps the same NSWindow and relaunches the worker, re-wiring everything:
  input/ack fds (`WorkerConn::swap`), a fresh scanout/control reader (`spawn_reader`), gvproxy recycle,
  the resize listener (unlink+rebind), and the surface-port receiver (persists across relaunch; the new
  worker re-publishes its non-global scanouts). Verified: `systemctl reboot` over SSH → worker exit 125
  → relaunch → guest SSH back + window re-displays the desktop and stays interactible, 0 "unresolved"
  present skips, surface-port re-scoped. Regression guard: `worker_conn_swap_retargets_every_field`.
- **libkrun panic→graceful exit paths** — ✅ **DONE 2026-06-23 (libkrun patch 0028).** The aarch64
  HVF vCPU loop no longer `panic!`s on unhandled guest traps: an unknown PSCI/SMC function returns
  `PSCI_RET_NOT_SUPPORTED` and the guest keeps running (standard PSCI semantics), while every other
  unhandled trap (exception class, system register, exit reason, MMIO size) logs the specifics and
  returns `Error::Unhandled`, which `vstate::run_emulation` maps to a clean VM teardown
  (`FC_EXIT_CODE_GENERIC_ERROR`) instead of aborting the worker process. Healthy guests never hit
  these arms (L1 boot still green). RED-first: `spikes/hvf-trap-probe` (96-byte bare-metal arm64
  Image) + `crates/limina-test/tests/hvf_graceful.rs` — verified RED (SIGABRT) before, GREEN after.

## M4 venus residue
- **GLX / Xwayland apps present black on venus** (open, low priority — diagnosed 2026-06-29) — on the
  F44 enhanced tier, `glmark2` (no args → GLX) and `glxgears` show a **black window**; native-Wayland
  GL (Firefox WebGL) is fine. **Root-caused to the PRESENT path, not render/context** (verified on the
  live enhanced VM): `glxinfo -B` shows the GLX context creates and is accelerated — `direct rendering:
  Yes`, renderer `zink Vulkan 1.3(Virtio-GPU Venus … MESA_KOSMICKRISP)`, GL 3.1 (the KK
  custom_border_color cap, [[limina-kk-feature-gaps]]) — and `glxgears` renders **395 FPS** while the
  window stays black. So GL renders fine; the X11 present (zink's **kopper** DRI3/Present WSI on the
  Xwayland-backed-by-venus surface) never gets the rendered pixmaps to the window. This is the known
  "kopper X11 regression" the `venus_replay` X11 probe trips over (`venus_replay.rs:211`). User doesn't
  use GLX apps → deferred. Investigation start: the kopper DRI3 pixmap sharing / X Present on a
  venus-backed Xwayland (is the present-buffer a virtio-gpu blob that isn't flushed/attached to the
  Xwayland Wayland surface?); compare client-direct DRI3 present vs Xwayland glamor.
- **Stock/basic tier: guest Vulkan doesn't degrade to lavapipe** (open, to investigate — reported
  2026-06-29 while dogfooding) — on the basic tier (stock F44, 4 KiB pages, virgl GL works, no venus),
  guest Vulkan apps **fail** instead of falling back to **lavapipe** (Mesa's CPU/llvmpipe Vulkan). This
  breaks the two-tier graceful-degradation guarantee: the stock baseline should still offer *working* (if
  slow) Vulkan via lavapipe when hardware/venus Vulkan is unavailable. Likely cause: stock Fedora ships the
  **venus ICD** (`libvulkan_virtio.so`), which on a 4 KiB-page guest under the 16 KiB host fails init
  (`vkEnumeratePhysicalDevices → ERROR_INITIALIZATION_FAILED`), and the Vulkan loader doesn't fall through
  to lavapipe — either lavapipe (`libvulkan_lvp.so`) isn't installed, or the loader picks the failing venus
  ICD and stops. Investigation start: in a basic guest, `ls /usr/share/vulkan/icd.d/` + `vulkaninfo` ICD
  enumeration (is lavapipe present? does `VK_ICD_FILENAMES`→lvp work?), then decide the fix (ensure
  lavapipe + loader fall-through, or a venus ICD that declines cleanly so the loader tries the next). The
  enhanced tier (16k + venus) is unaffected — venus enumerates the real GPU there; this is a *basic-tier*
  usability gap.
- **virtio-gpu flip-completion gap** — ✅ **RESOLVED (verified 2026-06-23); item was stale.** Already
  fixed by `patches/linux/0001` (drm/virtio fence blob-scanout flushes, 2026-06-11): host3d_blob
  (venus) scanout FBs now carry the same fence the dumb path has, so `virtio_gpu_resource_flush`
  `dma_fence_wait`s (50 ms cap) before commit-tail, which gates `drm_atomic_helper_fake_vblank` →
  the (fake) page-flip-complete event fires. Verified on the enhanced tier with `kmscube -A`
  (atomic + fencing): two clean runs rendered 299 and 359 frames at a steady **30 fps**, rc=0, no
  dmesg errors — event-driven atomic clients render, they do not hang. Legacy `drmModePageFlip`
  events also work. GOTCHA that masked this: kmscube polls **stdin** alongside the DRM fd, so over a
  non-interactive SSH session it sees EOF→POLLIN, prints "user interrupted!", and bails after ~1
  frame — run it as `sleep N | kmscube …` to give it a quiet stdin.
- **Direct-KMS double-buffered clients cap at 30 fps** (investigated 2026-06-23) — understood, narrow,
  NOT fixing now. kmscube `-A` runs 31 fps regardless of `LIMINA_FENCE_LATCH_MS` (8 vs 35 ms both
  31 fps), so it is *not* the open-loop latch fallback — the present fences complete via the truthful
  CA-latch ack. Host is 60 Hz, so 31 fps ≈ 2 vsyncs/frame: a strictly double-buffered client that
  blocks on flip-complete misses every other vsync because the #8 fence-accurate present does two
  sequential waits (GPU-render-complete, then CA-latch) and that round-trip exceeds one vsync. The
  Wayland desktop + Wayland fullscreen apps hit 60 fps (mutter triple-buffers and pipelines the next
  frame while the current one latches). So only strictly-double-buffered, blocking, *direct-KMS*
  clients (kmscube, bare SDL-KMS demos) are affected — not the real workload. Fix directions if ever
  pursued: (1) decouple the atomic-KMS fake-vblank from the full CA-latch (fire at render-complete /
  on a vsync-cadence timer — mirrors real hardware, but a #8 design change that must not reintroduce
  tearing), or (2) shave the present round-trip below one vsync (needs worker instrumentation to
  quantify it first). Revisit only if direct-KMS double-buffered fullscreen clients become a target.
- **#28 coherency residue policy** — ✅ **CLOSED (2026-06-23): no action needed; keep venus feedback
  disabled.** Re-framed after a design panel over-stated it. `VN_PERF=no_*_feedback` turns off venus's
  host-visible *feedback* buffers (host writes fence/semaphore/event/query completion into a
  guest-pollable buffer so waits resolve locally with no guest→host round-trip); off, sync rides the
  virtio-gpu per-context ring fence our stack already retires. **We never want feedback on:** the
  round-trip elimination it buys only matters for fine-grained-sync-heavy GPU **compute/ML** (krunkit's
  domain), not a vsync-paced GNOME+WebGL desktop (a handful of syncs per 16 ms frame, blocked on the
  frame fence anyway — the saving is invisible under 60 Hz). And enabling feedback would *exercise* the
  #28 SLC-beyond-PoC host-visible-coherency fragility, i.e. trade robustness for perf we can't use.
  Feedback-off (ring fence) is already tier-2 GREEN — more robust **and** sufficient. So nothing to fix,
  productize, or spike. The earlier "productize or a fresh enhanced guest *hangs*" claim was an
  unverified inference; the two-tier floor is already safe via venus's graceful llvmpipe degrade
  (venus-init failure → software-2D, `VN_PERF` only read when venus actually renders = the enhanced
  tier, which is our own baked image). Two of the panel's "real fix" candidates stay dead on physics
  regardless (host-clean-to-PoC is a no-op — Shared `MTLBuffer` already host-coherent; HVF stage-2
  attrs are not expressible — `hv_memory_flags_t` is permission-only, `hvf/src/lib.rs:289`). **Revisit
  only if limina ever grows a venus-compute tier** (out of charter — it's a desktop VM).
- **Cosmetics** — ✅ **mostly DONE 2026-06-23 (libkrun 0029/0030).** Verified on the seated venus
  tier: the desktop now boots with **zero** `virtio_gpu` dmesg errors (was: a `capset_id=2` GL-probe
  EINVAL + `0x1200` responses for `CTX_ATTACH/DETACH_RESOURCE` 0x202/0x203), venus rendering
  unchanged (gnome-shell `init=0x4` contexts, tier-2 GREEN).
  - `num_capsets` hardcoded 5 → **fixed (0029):** the device hardcoded 5 while `create_rutabaga`
    passed `capset_mask=0` (registers all 9); now both derive from `virgl_flags` via one helper, so
    a `VENUS|NO_VIRGL` guest enumerates exactly the venus capset and never probes ones we can't serve.
  - `0x1200`/`0x202`/`0x203` `CTX_ATTACH/DETACH_RESOURCE`→ErrUnspec → **fixed (0030):** in coexist
    mode the 2D scanout resources (boot fb / fbcon) aren't in the 3D renderer's map, so the kernel's
    attach/detach of them to a 3D context is now an idempotent no-op (also covers the detach/teardown
    race). Real 3D resources (de)attach normally.
  - **Firefox MSAA silent non-AA** → **documented, NOT chasing (known cosmetic).** Core MSAA works on
    zink/venus (`spikes/venus-draw-probe/msaa-test.c` passes); the gap is Firefox-specific — its
    `MozFramebuffer::CreateImpl` combo (color RENDERBUFFER + DEPTH24_STENCIL8 @ samples=4) reports the
    backbuffer incomplete, so it silently falls back to non-AA. One app's AA quality, with the general
    path working — not worth a venus/zink/KK FBO-completeness rabbit-hole. Reopen only if MSAA breaks
    broadly. (`msaa-test.c` is the standing oracle; memory `limina-tier2-venus` thread 7.)
  - Remaining (untouched, genuinely low-value): KK GPU-side per-draw root re-fetch (only if GPU-bound
    workloads reappear). Roadmap M4 (~line 413).

## M5 hardening
- **Clipboard test-coverage gaps** — the ext-data-control (enhanced) backend is live-verified only,
  not under automation (`l1_session_helper` exercises only the RemoteDesktop fallback); plus
  stale-serial races, multi-peer broadcast + dead-peer pruning, helper reconnect after supervisor
  restart / D-Bus session death. Roadmap M5 (~line 522); memory `limina-m5`.
- **virtiofs DAX/shm window** — `VirtioShmRegion` in `fs/device.rs`; confirm shm-window alignment +
  FUSE_SETUPMAPPING/SHMCAP on 16 KiB host pages (enhanced tier already runs 16k guest = host-page;
  test stock-4k separately) + host↔guest uid mapping. Roadmap M5 (~line 502).

## M3 networking
- **gvproxy reconnect-on-HANG_UP** — today the net worker logs FATAL and permanently disables the NIC
  on HANG_UP (`worker.rs:146`); the supervisor recreates the path. A small libkrun reconnect patch
  would survive a gvproxy restart without a full VM restart. Roadmap M3 (~line 321); memory
  `limina-m3-networking`.

## M2 / M8 polish wins (cheap, host-side)
- ~~**Pointer warps / pointer capture**~~ — **DONE** (2026-06-27). `Cmd-Ctrl-G` capture mode feeds
  the guest a separate relative-mouse virtio-input device; closes the guest-warp gap. Host cursor
  pinned by warp-to-centre (CGAssociate-false alone insufficient on macOS 26).
- **Pointer-capture containment — revisit fresh** (parked 2026-06-27). The current scheme re-pins the
  (hidden) host cursor to display-centre on *every* captured motion event. Verified facts: macOS 26
  `CGAssociateMouseAndMouseCursorPosition(false)` does NOT freeze the cursor (its `CGEventGetLocation`
  tracks the mouse 1:1), the session `CGEventTap` never disables under load, and with the per-event
  warp removed the relative deltas fed to the guest are perfectly clean. **Works well locally.** The
  warp's weakness shows only when *another* agent also drives the macOS cursor — a **remote-desktop
  client** used to operate this Mac — where the constant re-pin fights the RD cursor and reads as
  jitter/snapping (an edge-only-warp variant was tried and reverted; it didn't clearly help under RD).
  **Plan when we return:** research how VNC/RDP **servers** solve the same capture problem (they have
  this exact problem space and are far more numerous) before redesigning. Candidate angles: a real
  cursor-freeze API instead of warp, `CGDisplayHideCursor`+associate semantics, an `IOHIDEventSystem`
  relative-tap, or detecting/co-existing with an upstream RD capture. Flat guest pointer profile
  (enhanced tier, `accel-profile='flat'`) is the other half and works.
- ~~**Fullscreen**~~ (`Cmd-Ctrl-F`) and ~~**keymap remap / Command-Option swap**~~ (`--swap-cmd-opt`)
  — **DONE** (2026-06-27). Remaining M8 polish: **system-combo capture** (CGEventTap behind a TCC
  toggle — extends the existing `match_host_shortcut` framework to system combos), **multi-display**
  (multiplex scanouts by `scanout_id`). Roadmap M8.

## Dogfooding / Parallels migration
Surfaced 2026-06-29 while planning the migration of a stock Fedora 44 Parallels VM onto limina on a
second Apple-Silicon Mac (full runbook: `docs/dogfooding-parallels-migration.md`).
- **`gvproxy` not bundled** — ✅ **DONE 2026-06-29** (`458664b`). `limina --net` resolved gvproxy only
  from `$LIMINA_GVPROXY_BIN`/Homebrew/`PATH`, so networking was dead on a Mac without Homebrew. Now
  vendored into the app (`Contents/MacOS/gvproxy`, copied + ad-hoc signed by `build-app.sh`) and
  resolved bundle-relative; priority `override > bundled > Homebrew > PATH` is a pure, unit-tested
  policy (`gateway.rs::resolve_gvproxy_bin`, RED-first test).
- **Agent install was a separate SSH flow** — ✅ **DONE 2026-06-29** (`458664b`). `install-enhanced.sh`
  now also installs `limina-agent` (+ unit + flat-pointer gschema override) when staged into the
  payload, and runs `restorecon` so it starts on a stock SELinux-**Enforcing** guest (the dev guest
  dodged this with `selinux=0`). The whole enhanced upgrade now rides the one offline virtiofs channel
  instead of also needing network + gvproxy. (`install-guest-agent.sh` remains the dev-loop SSH path.)
- **No Parallels-import tooling** (open) — converting an existing Parallels disk (merge snapshots →
  `qemu-img -f parallels` → raw, the `virtio_mmio` initramfs regen, `console=` GRUB args, Tools
  removal) is documented in the runbook but not scripted. A guided `import` helper would de-risk the
  footgun (`virtio_mmio`, not `virtio_pci`).
- **No guest-tools distribution path** (open, architectural) — the enhanced tier can't be built or
  installed from `limina.app` alone (no RPMs, no toolchain on a second Mac). Recommended: a versioned
  out-of-band payload + a `limina install-guest-tools` subcommand that stages it into a `--share`. The
  F44 in-guest build prep addresses the *build* half; host orchestration is still open.
- **No payload↔guest version manifest check** (open) — `install-enhanced.sh` doesn't verify the
  payload's Fedora release matches the booted guest; a mismatch (esp. mutter↔gnome-shell ABI) breaks
  the desktop silently. Add a manifest + `/etc/os-release` guard.
- **KK/Metal never tested cross-machine** (open/unknown) — the host Vulkan-on-Metal stack was only
  exercised on the M1 Max / macOS 26.5 dev Mac. `--gpu-software-2d` is the degraded fallback if venus
  init aborts on different silicon/macOS.
- **F44 enhanced tier blocked** (open) — GNOME 49→50 mutter/cogl scanout regression; tracked with the
  F44 enhanced build prep. Basic-tier F44 is unaffected.

---

When a milestone's loose ends are all closed, fold the remainder back into the roadmap milestone
status. Greenfield milestones still ahead: **M7 USB**, **M8 audio + x86**. (**M6 dynamic memory** shipped
2026-06-26 — see `docs/design/m6-dynamic-memory.md` + memory `limina-m6-dynamic-memory`.)
