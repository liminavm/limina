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
- **Capability-scope the scanout IOSurfaces** (security) — the worker exports each guest scanout by
  its machine-global `IOSurfaceID`; any same-user process can brute-force-read the guest screen
  (`spikes/.../iosdump.swift` is a PoC). Bounded severity (local, same-user). Fix = export via
  `IOSurfaceCreateMachPort` + hand the port right to the window process (mach rendezvous). Roadmap M8
  (~line 664).
- **CapsLock/NumLock LED parity** — surface the statusq LED feedback (libkrun `worker.rs` no-op).
  Roadmap M8.

## Lifecycle robustness
- **Windowed guest reboot** — reboot-relaunch is shipped but **headless-only**; the windowed
  worker↔window socketpair re-wiring on relaunch is an explicit follow-up. A guest reboot in a window
  should Just Work. Roadmap cross-cutting decisions (~line 34).
- **libkrun panic→graceful exit paths** — a real boot hitting an unknown PSCI / ESR_EL2 EC / exit
  reason still `panic!`s the worker (`hvf/lib.rs:549/595-602/728`); convert to logged graceful stops.
  Roadmap M1 stretch (~line 133).

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
