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
- **virtio-gpu flip-completion gap** — no page-flip-complete event, so event-driven KMS clients
  (kmscube, SDL/DRM, plymouth) hang after ~2 frames; mutter uses fallback timing (most desktop apps
  fine). Roadmap M4 (~line 410); memory `limina-tier2-venus` open thread 2.
- **#28 coherency residue policy** — `VN_PERF=no_*_feedback` is the canonical workaround; decide
  policy (limina-agent sets it) vs a real host-clean-to-PoC coherency fix. Roadmap M4 (~line 412).
- **Cosmetics** — `num_capsets` hardcoded 5; the non-fatal `0x1200`/`0x203` (CTX_DETACH_RESOURCE →
  ERR_UNSPEC) dmesg lines; Firefox MSAA silent non-AA. Roadmap M4 (~line 413).

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
- **Pointer warps** — guest-initiated pointer warps (guest `cursormove` positions) are ignored
  host-side; reconcile with **pointer capture** (relative mode). Roadmap M2 (~line 167) / M8.
- **Fullscreen** (NSWindow `toggleFullScreen:`), **keymap remap / Command-Option swap** (host-side
  kVK→KEY table edit), **system-combo capture** (CGEventTap behind a TCC toggle), **multi-display**
  (multiplex scanouts by `scanout_id`). Roadmap M8 (~line 648).

---

When a milestone's loose ends are all closed, fold the remainder back into the roadmap milestone
status. Greenfield milestones still ahead: **M6 dynamic memory**, **M7 USB**, **M8 audio + x86**.
