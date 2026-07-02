# limina full review — 2026-07-01

Scope: design + implementation of the VMM (host crates + guest agent), every carried
patch series (with per-patch upstreamability triage), and the future-plans/docs state.
Method: four parallel deep-read reviews (VMM crates; libkrun/imago/linux patches;
graphics-stack patches; roadmap/design docs) synthesized into this memo. Line numbers
are as of commit `ae11388`.

**This memo is the input for the dedicated upstreaming session** — the per-patch
classification tables in Parts II and III are the artifact to start from.

---

## Executive synthesis

The codebase is unusually disciplined: the libkrun facade boundary, the pure-core
testing style, the granular two-tier degrade paths, and the teardown engineering are
better than typical VMM front-ends. The patch portfolio (~80 patches across 7 series)
is high quality individually but is carried as *history* rather than curated series:
dead MoltenVK-era weight, un-squashed fixups, one accidental 65k-line binary blob,
and gkvm→limina naming drift inflate every rebase. Roughly **half the portfolio is
upstreamable near-as-is**, and upstreaming is the best defense against the #1 project
risk (rebase burden). Plans are strong where they exist (M9 is well designed but gated
on one undone spike) and thinnest exactly where "the product replaces Parallels"
lives: distribution/signing, VM persistence/management, audio, multi-display.

Top action items (agreed 2026-07-01; all in flight except upstreaming, which gets a
dedicated later session):

1. Fix the control-plane blocking-write hazard and the `kill(-pid)` guard.
2. Patch hygiene sprint (re-export virgl 0014 clean, squash superseded patches, drop
   the MoltenVK arc, make `patches/mesa/` a real series, finish the rename).
3. *(deferred to a dedicated session)* Fire upstreaming Wave 1; add an upstreaming tracker.
4. Run M9.0 spike #2 (HVF vCPU+GIC round-trip) — gates all of M9.
5. Close the two two-tier floor cracks (stock Vulkan→lavapipe fall-through; GRUB
   keyboard via the virtio-input EFI driver).
6. Write the two missing designs (VM definition/persistence; distribution/signing/
   updates) and fix the stale doc blocks.
7. Extract `WindowedSession` from `main.rs`; split `window.rs`.

---

# Part I — VMM design & implementation (crates/ + guest/)

## 1. Architecture map

### Process model

Two host processes plus the guest, exactly as decided in D3 (`docs/design/architecture.md:34`):

- **`limina` (supervisor/UI, un-entitled)** — parses the CLI, resolves the VM config,
  spawns and monitors the worker, owns the AppKit window, the control plane, gvproxy,
  and all policy (balloon, keymap, clipboard). Entry: `crates/limina/src/main.rs:208`.
- **`limina-vmm` (worker, codesigned with `com.apple.security.hypervisor`)** —
  translates a typed `VmSpec` into libkrun's internal `VmResources` and blocks forever
  in the event loop; guest power-off calls `libc::exit` inside `krun-vmm`, killing only
  this disposable child (`crates/limina-vmm/src/krun/mod.rs:365-371`). Reboot exits
  with the hand-synced code 125 (`crates/limina/src/supervisor.rs:33`) and the
  supervisor relaunches a fresh worker.
- **Guest side** (`guest/`, its own workspace, aarch64-linux): `limina-init` (890 LOC,
  the L1 test init + frozen agent seed), `limina-agent` (285 LOC, root control-plane
  daemon: HELLO/heartbeat/shutdown/mempressure + virtiofs automount),
  `limina-agent-session` (545+336 LOC, per-session clipboard bridge),
  `limina-mock-mutter` (test double), `limina-config`.

### Crate roles and sizes (host, ~15.5k LOC excl. tests-in-files)

| Crate | LOC | Role |
|---|---|---|
| `limina` | ~5,900 | supervisor: `main.rs` 1362 (CLI + windowed spawn wiring), `window.rs` 1378 (+`window/input.rs` 585, `window/capture_tap.rs` 351), `gateway.rs` 917 (gvproxy lifecycle/death-pact/orphan sweep), `control.rs` 372 (control-plane host side), `supervisor.rs` 276 (spawn/monitor/reboot guard), `balloon_policy.rs` 216, `clipboard.rs` 201, `venus_env.rs` 91 |
| `limina-vmm` | ~1,600 | worker binary: `main.rs` 471 (CLI→`VmSpec`, disk flock), `krun/mod.rs` 611 (the **one** libkrun facade), `config.rs` 204 (limina's own vocabulary), `shutdown.rs` 57, `krun/console.rs` 155 |
| `limina-proto` | 578 | 16-byte header + CBOR control plane; shared host/guest |
| `limina-display` | 1,221 | libkrun display backends: PNG capture oracle (`lib.rs`) + IOSurface-ring window backend (`iosurface.rs` 868) |
| `limina-input` | 1,159 | kVK→KEY_* keymap + Cmd/Opt swap policy (host side) and virtio-input fd backends (worker side, feature-gated) |
| `limina-surfaceport` | 408 | Mach-port hand-off of non-global IOSurfaces (raw `mach_msg` FFI) |
| `limina-usbip` | ~1,500 | host USB/IP exporter: proto, transport-agnostic server, mock + libusb backends |
| `limina-test` | 2,280 lib + ~3,400 tests | L1/L2 harness driving the real shipped binaries |

### Communication fabric (one VM = five channels)

1. **vsock control plane** — guest agent connects out to `CID_HOST:CONTROL_PORT`
   (`b"LIMI"` as u32, `limina-proto/src/lib.rs:53`); the worker bridges it to a
   supervisor-owned unix socket (`krun/mod.rs:288-302`); the supervisor serves
   multi-peer HELLO/WELCOME/heartbeat/shutdown/clipboard/mempressure with
   per-capability routing (`control.rs:184-199`).
2. **Scanout channel** — worker→supervisor SOCK_STREAM socketpair carrying a newline
   text protocol (`surface`/`frame`/`cursor`/`cursormove`/`cursorhide`,
   `limina-display/src/iosurface.rs:9-13`, parsed at `window.rs:270-352`); the same fd
   carries supervisor→worker `shown <id>` acks (dup'd, `main.rs:585`).
3. **Surfaceport Mach ports** — non-global IOSurfaces crossed by
   `IOSurfaceCreateMachPort` → `bootstrap_register`-named receive port
   (`limina-surfaceport/src/lib.rs:135-230`), closing the `IOSurfaceLookup`
   screen-read hole; graceful global fallback both sides (`window.rs:617-622`,
   `iosurface.rs:136-144`).
4. **Input channels** — three SOCK_DGRAM socketpairs (kbd, abs pointer, rel pointer),
   one 8-byte evdev triple per datagram (`limina-input/src/lib.rs:18-27`),
   non-blocking supervisor send ends + 256 KiB buffers so a stalled worker can never
   beachball AppKit (`main.rs:527-544`).
5. **Side-control sockets** — dedicated unix listeners bound by the worker for display
   resize (`krun/mod.rs:453-487`) and balloon target/stats (`krun/mod.rs:508-583`);
   shutdown eventfd raised from a SIGTERM handler (`limina-vmm/src/shutdown.rs:23-32`).

### AppKit/Metal front-end wiring

`window::run` (`window.rs:401`) builds an NSWindow with a **layer-hosting** `CALayer`
whose `contents` is set directly to the guest IOSurface
(`setLayerContentsRedrawPolicy(Never)`, `window.rs:449-454`). Presents are
event-driven — the reader thread `dispatch_async_f`'s onto the main queue
(`window.rs:193-222`) — with a 60 Hz common-modes `NSTimer` as fallback/liveness
(`window.rs:849-852`). A `CATransaction` completion block emits the "shown" ack at the
true CA latch boundary, dispatched through a bounded channel to a dedicated sender
thread (`window.rs:1300-1305`, `window.rs:554-570`). Input rides a local `NSEvent`
monitor (`window.rs:859-902`) plus a session `CGEventTap` for pointer capture.
Note: this is display "model (A)" — the architecture doc still says milestone-1 chose
model (B) (window in the worker, `architecture.md:124-127`); the doc has drifted
behind the code.

## 2. Strengths

1. **The facade discipline is real, not aspirational.** All libkrun-internal coupling
   lives in `limina-vmm/src/krun/mod.rs` (one module translates `VmSpec`→`VmResources`,
   replicates `krun_start_enter`'s orchestration at `krun/mod.rs:371-447`), with
   limina's own vocabulary insulated in `config.rs:1-7`. An upstream rebase touches one
   file, exactly as D2.1 intended.
2. **Pure-core / injected-probe testability, everywhere.** Firmware resolution
   (`main.rs:845-857` with injected `exists`), port allocation (`gateway.rs:267-283`
   with injected `is_free`), orphan policy (`gateway.rs:602-604`), balloon policy
   `decide` (`balloon_policy.rs:125-160`), quit policy `should_initiate_quit`
   (`window.rs:388-398`), `bundle_venus_env` (`venus_env.rs:26`) — each a pure function
   with unit tests documenting a real production regression (e.g. the
   minimize-powers-off bug, `window.rs:1316-1344`).
3. **Protocol robustness rules are enforced in code.** Unknown message types decode to
   `Message::Unknown` and get `ERR_UNSUPPORTED`, never a dropped connection
   (`limina-proto/src/lib.rs:319-324`, `control.rs:345-347`); payload length is bounded
   pre-allocation (`lib.rs:363-369`, tested at `lib.rs:544-557`); oversized clipboard
   answers `ERR_TOO_LARGE` instead of a doomed frame (`clipboard.rs:117-128`).
4. **Teardown engineering is exceptional.** gvproxy has three documented layers:
   clean-exit cleanup, an EOF death-pact watcher (re-exec'd `limina __reap-gateway`,
   the macOS stand-in for `PR_SET_PDEATHSIG`, `gateway.rs:374-512`), and a
   pid-reuse-guarded startup orphan sweep (`gateway.rs:561-633`) — with the multi-VM
   safety invariant unit-tested (`gateway.rs:687-699`) and end-to-end tested
   (`tests/death_pact.rs`).
5. **The shutdown ladder is layered and opportunistic**: agent SHUTDOWN → GPIO power
   button via SIGTERM→eventfd → SIGKILL after grace (`supervisor.rs:165-218`), and a
   control plane that fails to start degrades rather than kills the VM
   (`main.rs:365-370`).
6. **macOS sharp edges are pre-empted and documented at the exact line**:
   `SO_NOSIGPIPE` everywhere (`main.rs:747-756`, `control.rs:360-372`), the
   dup-shares-O_NONBLOCK trap on the ack fd (`main.rs:583-585`), `NSCursor`
   hide/unhide refcount balancing (`window/input.rs:75-89`), `NSTimer` freezing in
   tracking mode (`window.rs:844-849`), the layer-hosting 0×0 trap
   (`window.rs:640-645`).
7. **Disk safety**: `flock(LOCK_EX|LOCK_NB)` against double-RW attach
   (`limina-vmm/src/main.rs:222-249`), `:create` idempotent and refuses to resize
   (`main.rs:1068-1090`), duplicate paths canonicalized and rejected
   (`main.rs:1003-1013`), qcow2 by magic sniff (`krun/mod.rs:333-341`).
8. **Comment quality is a genuine asset**: comments encode causality, falsified
   alternatives, and negative results (e.g. the LIMINA_PRESENT_LOCK "FAILED — kept as
   a documented negative result" note, `window.rs:511-520`; the surface-ring
   compositor-hold rationale, `iosurface.rs:95-103`).

## 3. Weaknesses / risks / tech debt

### Concurrency

- **Blocking socket writes under the peers mutex (`control.rs`).** `Peer::send` does a
  blocking `write_message` while `request_shutdown`/`broadcast_clipboard` hold
  `inner.peers.lock()` (`control.rs:184-199`, `208-220`). A wedged agent that stops
  draining its vsock buffer stalls the writer **while holding the registry lock**,
  which then blocks the accept path's registration (`control.rs:294`), the liveness
  sweep (`control.rs:154`), and — worst — `request_shutdown` called from the Ctrl-C
  monitor (`supervisor.rs:183-185`), stalling the entire shutdown ladder. No write
  timeouts are set anywhere on these streams. **The most consequential latent bug
  found in this review.**
- **Raw-fd-as-int lifecycle in `WorkerConn`.** Fds are published as `AtomicI32`s
  (`window.rs:137-186`) and the relaunch path closes the old ints after the swap
  (`main.rs:694-704`). A main-thread send that loaded a stale fd just before the swap
  can write into a **reused** fd number if anything opens an fd between close and
  send. `OwnedFd` + `arc-swap` (or an epoch/generation check) would close it.
- **`kill(-pid, SIGKILL)` with no pid guard** (`window.rs:768`, `window.rs:803`).
  `WorkerConn.pid()==0` is documented as "no current worker" (`window.rs:136`), and
  `kill(0, sig)` signals the **caller's own process group**. Unreachable today, one
  refactor away from the supervisor SIGKILLing itself.
- **Fd leaks on partial failure**: if `supervisor::spawn_worker` fails inside
  `spawn_windowed_worker` (`main.rs:570-573`), all eight socketpair ends leak.
- **Error swallowed into "clean exit"**: `supervisor::monitor(...).unwrap_or(0)` in
  the windowed monitor thread (`main.rs:661`) maps a monitoring *error* to exit code
  0, indistinguishable from an orderly power-off.

### Protocol fragility / proliferation

- **Four ad-hoc plaintext control planes coexist** beside the well-engineered CBOR
  one: scanout lines (`iosurface.rs:9-13`), `shown <id>` acks (same fd, opposite
  direction), `resize <w> <h>` (`krun/mod.rs:490-498`), and `target/stats` balloon
  lines (`krun/mod.rs:554-581`). None is versioned; the window reader silently drops
  unrecognized lines (`window.rs:343`) and half-parsed fields (`_id1` at
  `window.rs:283` is dead on arrival). The `frame <id>` message carries no scanout id
  — multi-display forces a flag-day unless it's threaded through beforehand.
- **Env vars as an in-process rendezvous**: the worker `std::env::set_var`s
  `LIMINA_SHOWN_ACK_FD` and `LIMINA_SURFACE_PORT_NAME` so code deep inside
  libkrun/virglrenderer can find them (`limina-vmm/src/main.rs:322-333`). Works (set
  pre-threads) but is hidden coupling, and `set_var` is `unsafe` in Rust 2024.
- **`WORKER_EXIT_REBOOT = 125` is hand-synced with libkrun** (`supervisor.rs:29-33`);
  an upstream rebase changing `FC_EXIT_CODE_REBOOT` silently turns every reboot into
  "VM stopped".
- **Clipboard serial/text non-atomicity**: `make_offer` bumps `host_serial` (atomic)
  then sets `host_text` under a separate mutex (`clipboard.rs:78-86`); a peer-thread
  `initial_offer` racing the poller can pair serial N with text N+1. Text-only and
  self-healing, but a protocol-correctness wart.

### Module size / cohesion

- **`window.rs` (1378) is three modules in one**: present pipeline + resize debounce,
  cursor management (host-shape adoption *and* capture compositing), quit/teardown
  policy, and ~200 lines of diagnostic capture machinery (`window.rs:464-525`,
  `916-970`). The 250-line timer block (`window.rs:762-841`) plus the 170-line
  `apply` closure (`window.rs:575-743`) hold intertwined `Cell` state.
- **`main.rs` (1362)**: the windowed spawn/relaunch machinery (`main.rs:491-717`) —
  raw fd plumbing, monitor thread, gateway recycling — is a lifecycle module hiding
  inside a CLI file. The 9-arg `run_windowed`/7-arg `window::run` signatures are the
  smell that a `WindowedSession` struct wants to exist.

### Single-VM-per-process assumptions

Process-wide statics are pervasive: `STOP` (`supervisor.rs:79`),
`GVPROXY_PID`/`GVPROXY_SOCK`/`WATCHER_PID`/`DEATHPACT_WFD` (`gateway.rs:63-71`),
`CLEANUP_PATH` (`control.rs:36`), `SHUTDOWN_FD` (`shutdown.rs:21`), `APPLY_HOOK`
thread-local (`window.rs:204-207`), plus `std::process::exit` sprinkled through the
timer (`window.rs:771`, `806`). Fine for the one-process-per-VM CLI; a
multi-window/multi-VM AppKit app in one process collides with every one of these.

### Feature-landing risks

- **Multi-display is architecturally accounted for but concretely single-display**:
  `MAX_TRACKED_SCANOUTS: usize = 1` (`limina-display/src/lib.rs:35`), `Shared` holds
  one geometry/cursor set (`window.rs:227-256`), the resize listener hardcodes display
  0 (`krun/mod.rs:479`), and the line protocol's `frame <id>` carries no scanout id.
  All four layers change together when multi-display lands.
- **Snapshots (M9)**: restore-as-fresh-worker fits the lifecycle model, but the
  relaunch loop's fd/reader/gateway choreography (`main.rs:653-717`) is exactly the
  code a `--restore` path must reuse; today it is not reusable outside `run_windowed`.
- **USB real-device** is well isolated (`limina-usbip` transport-agnostic with a mock
  backend), deliberately deferred to the privileged-helper design.

### Test coverage gaps

L0 (pure fn) and L1/L2 (real-binary boot) layers are unusually strong — 27 integration
test files including reboot, venus pixel replay, PSI balloon, clipboard round-trip.
Gaps: the AppKit present/timer/quit state machine is untested beyond its two pure
helpers (its interleavings caused past bugs: minimize-poweroff, black-on-resize);
`control.rs`'s multi-peer concurrency has no adversarial test with a non-draining
peer; `capture_tap.rs` is inherently untestable (needs TCC) but has no fake either.

## 4. Adherence to the stated tenets

**Mechanism in libkrun, policy in limina: genuinely upheld host-side.** Balloon
thresholds/hysteresis in `balloon_policy.rs:1-8` while the worker "is mechanism-only
and doesn't know the min" (`config.rs:180-182`); keymap/Cmd-Opt swap host-side
(`main.rs:186-206`); resize debounce/feedback-guard in the window timer
(`window.rs:600-626`). (See Part II/III for the two places the tenet is violated
inside the patch stack.)

**Two-tier guarantee: visible in code, granular, not a monolithic flag.** Capabilities
are per-feature strings negotiated in HELLO/WELCOME (`limina-proto/src/lib.rs:87-90`),
routed per-capability per-peer (`Peer::has_cap`, `control.rs:73-75`); multiple
concurrent peers with different cap sets are first-class (`control.rs:10-14`).
Degrade paths are opportunistic everywhere: control-plane failure warns and continues
(`main.rs:365-370`); no agent ⇒ power button ⇒ SIGKILL ladder
(`supervisor.rs:165-168`); venus init failure degrades to software-2D
(`krun/mod.rs:99-100`, `182-186`); surfaceport failure falls back to global surfaces
(`window.rs:617-622`); the guest agent reconnects forever and treats a missing host as
normal (`guest/limina-agent/src/main.rs:64-70`). One nuance: the *stock* GPIO
power-button path is honestly documented as unreliable on stock EFI guests
(`shutdown.rs:10-12`) — degrades to SIGKILL-after-grace.

**Doc drift**: `architecture.md` §2.4 still records display model (B) and a crate
layout (`limina-net`, `limina-config`, `agent/`) that doesn't match reality.

## 5. Top 10 recommendations (prioritized)

1. **Make control-plane writes non-blocking or timeboxed** — `set_write_timeout` on
   peer streams and/or per-peer outbound queue + writer thread
   (`control.rs:77-80`, `184-199`, `208-220`).
2. **Guard `kill(-pid, …)` against `pid <= 0`** (`window.rs:768`, `803`).
3. **Wrap worker fds in owned types** (`window.rs:137-186`); fix the leak on partial
   `spawn_windowed_worker` failure (`main.rs:509-595`).
4. **Extract a `WindowedSession`/lifecycle module from `main.rs`**
   (`main.rs:491-717`) — also the exact machinery M9 `--restore` needs.
5. **Split `window.rs`** into `present.rs`, `cursor.rs`, `lifecycle.rs` (+`diag.rs`).
6. **Consolidate/version the side protocols** (a `limina-wire` module with
   parse/format + tests); stop silently dropping unknown lines (`window.rs:343`);
   thread `scanout_id` through `surface`/`frame` now.
7. **Replace the env-var rendezvous** (`limina-vmm/src/main.rs:322-333`) with explicit
   plumbing through `WindowConfig`/renderer init.
8. **Share the reboot exit code with libkrun** instead of hand-syncing 125
   (`supervisor.rs:29-33`).
9. **Inventory and encapsulate process-wide singletons** behind a per-VM handle before
   the real AppKit app lands.
10. **Close the clipboard serial/text race** (`clipboard.rs:78-86`); update
    `architecture.md`'s model-B/§4 drift.

---

# Part II — patch triage: libkrun, imago, linux

Classes: **A** upstreamable as-is/near · **B** right mechanism, needs
rework/generalizing · **C** keep downstream · **D** obsolete/superseded.

## libkrun (40 patches on upstream `07a3f40`)

| # | What it does | ~LOC | Class |
|---|---|---|---|
| 0001 | Software-2D virtio-gpu scanout (host CPU shadow of CREATE_2D/…/FLUSH) + opt-in renderer-less device mode `set_gpu_software_2d` | ~430 | **B** — genuine feature for GL-less hosts, but large; needs a design conversation and splitting (2D shadow path vs the opt-in mode) |
| 0002 | PL011 drops TX bytes on WouldBlock instead of erroring per byte | 12 | **A** (upstream may prefer a bounded buffer over silent drop) |
| 0003 | `PortConfig::ConsoleInOut` (non-tty fds as a real hvcN console) + demote raw-mode ENOTTY to debug | 36 | **A** — explicitly "mechanism only" |
| 0004 | HVF: handle len=2 (halfword) MMIO writes instead of `panic!` | 1 | **A** — send first |
| 0005 | FDT: add `arm,primecell` to the PL011 node → real bidirectional ttyAMA0 | 2 | **A** (depends on 0004) |
| 0006 | virtio-mmio: queue marked ready with QueueNum unset snaps to max_size (matches QEMU; unblocks EDK2 VirtioGpuDxe) + regression test | 56 | **A** |
| 0007 | virtio-gpu: stop/join worker on device reset | 215 | **D** — superseded by 0022; squash 0007+0022 before upstreaming |
| 0008 | Implement the virtio-gpu cursor queue (UPDATE/MOVE_CURSOR → optional `set_cursor`/`move_cursor` vtable methods) | ~290 | **A** — additive ABI growth the header permits; "mechanism only" |
| 0009 | Log renderer-init failure instead of `.ok()` swallowing it | 8 | **A** |
| 0010 | Coexist: route Global-ring fences synchronously (venus can't fence ctx 0) + degrade to sw-2D on renderer-init failure | 87 | **B** — fence-routing half is generic; fallback half presumes 0001 |
| 0011 | Log `hv_vm_map` failures with alignment breakdown | 11 | **A** |
| 0012 | Blob map: drop stale `RUTABAGA_MEM_HANDLE_TYPE_APPLE` gate; `map_ptr()`/`virgl_renderer_resource_map` is the gate (virglrenderer 1.3.0) | 54 | **A** — check current upstream first; message defers a 16 KiB coherency follow-up |
| 0013 | `SET_SCANOUT_BLOB` as zero-copy IOSurface present (rutabaga iosurface_id plumbing + optional `present_surface` vtable method) | ~265 | **B** — right mechanism, macOS-specific, depends on the virgl fork's `virgl_renderer_resource_get_iosurface_id` export |
| 0014 | Don't panic the GPU worker on scanout-readback failure | 16 | **A** |
| 0015 | Promote cursor XRGB → ARGB (guest kernel hardcodes X format for dumb BOs; QEMU does the same) | 21 | **A** |
| 0016 | Log ctx lifecycle at info (with guest process name) and RESP_ERR at warn | 38 | **B** — log-policy taste |
| 0017 | Fence-accurate presents: park frames, inject present fence on reserved vkr ring 63; gated by `LIMINA_FENCE_PRESENT` env / `/tmp/limina-fence-present` marker | ~205 | **B/C** — real mechanism; env+tmpfile gating, hardcoded ring 63, fork coupling are downstream-shaped |
| 0018 | Hold guest flush fences until present + latch (`LIMINA_FENCE_LATCH_MS`, default 35 ms); pairs with linux 0001 | ~170 | **B/C** — same series; timing heuristic + env var |
| 0019 | Complete held fences on supervisor "shown" acks over `LIMINA_SHOWN_ACK_FD` | 152 | **C** — limina supervisor-protocol glue |
| 0020 | Demote per-frame FLUSHDBG/SET_SCANOUT_BLOB log lines to debug | 7 | **D** — fixup of earlier logging; squash |
| 0021 | Fence-present hardening: roll back leaked cookies; 500 ms unconditional-completion ceiling | 33 | **B** (as part of 0017/0018; the ceiling is an admitted safety net) |
| 0022 | Persist the renderer across device reset (virgl_renderer_init is process-global init-once); worker spawned once, activate/reset messaged | ~415 | **B** — real upstream bug class; squash with 0007 |
| 0023 | Decode PSCI SYSTEM_RESET as `VcpuExit::Reset` → `FC_EXIT_CODE_REBOOT` (125) | 25 | **A** — exit-code value needs upstream blessing |
| 0024 | Implement `transfer_read` (TRANSFER_FROM_HOST_3D) — was `panic!` | 37 | **A** |
| 0025 | Runtime display resize: `DisplayResizeHandle`, config-change interrupt, EDID regen | ~180 | **B** — near-A but embeds `/tmp/limina-readback-delay` + `/tmp/limina-dump-staging` DIAG hooks; strip first |
| 0026 | Expose `Vmm::gpu_resize_handle()` | 26 | **A** (companion to 0025) |
| 0027 | De-shear: present the SET_SCANOUT rect at the resource's own stride + unit tests | 109 | **A** — real latent bug, well tested |
| 0028 | HVF: unknown PSCI → `PSCI_RET_NOT_SUPPORTED`; other unhandled traps → clean teardown | 81 | **A** — spec-correct, RED-first tested |
| 0029 | Advertise only the capsets the renderer backs | 40 | **A** |
| 0030 | rutabaga: CTX_ATTACH/DETACH on missing context/resource = idempotent no-op (coexist) | 42 | **B** — upstream will want it gated on coexist mode |
| 0031 | `into_rust_result!`: map -2 → MethodNotSupported | 5 | **A** — clear macro bug |
| 0032 | Read the venus scanout IOSurface for the headless capture sink | 94 | **B/C** — needs 0031 + a fork export; consumer is limina's test sink |
| 0033 | Balloon FRQ: `MADV_FREE_REUSABLE` + 16 KiB-safe `ReclaimCoalescer` (MADV_DONTNEED reclaims nothing on macOS — spike-proven) | 194 | **A** — exactly what a macOS-first VMM needs; unit-tested |
| 0034 | Balloon inflate/deflate/target/actual, DEFLATE_ON_OOM, `BalloonControlHandle` | ~365 | **B** — handle API needs reshaping for libkrun's C ABI |
| 0035 | Drop leaked contexts/resources on dirty device reset (`reset_session_state`) | 35 | **A** (rides 0022; squash into lifecycle patch) |
| 0036 | Demote per-frame present DIAGs to trace | 4 | **D** — fixup; squash |
| 0037 | virtio-input: Inactive on reset() so re-activation happens (EDK2→kernel hand-off left input dead) | 8 | **A** |
| 0038 | virtio-blk serial = caller's block_id (stable `/dev/disk/by-id/virtio-<id>`); empty id → inode-derived fallback | 22 | **A** — additive, backward compatible |
| 0039 | Input worker: epoll blocks (-1) instead of a 1 s timeout | 5 | **A** |
| 0040 | HVF vtimer: multiply before dividing (u128) so the WFI timeout isn't ~1.6% short | 7 | **A** |

Census: **22 A, 12 B, 1 C (0019), 3 D (0007, 0020, 0036), 2 borderline B/C (0017/0018, 0032)**.

## imago (2 patches on pristine imago-0.2.2)

| # | What it does | ~LOC | Class |
|---|---|---|---|
| 0001 | Disable `try_discard_by_truncate` — tail discard punch-holes (`F_PUNCHHOLE`) instead of truncating (mkfs tail-discard shrank the file → ext4 unmountable after reboot) | 41 | **B** — the *bug report* is A; the patch hard-disables a function with limina-branded commentary. Upstream shape = caller-visible "preserve file size" option |
| 0002 | Pin vm-memory to `>=0.17,<0.18` to unify with libkrun's stack | 7 | **C** — build-graph pin |

## linux (3 patches, drm/virtio, applied to the enhanced-tier kernel)

| # | What it does | ~LOC | Class |
|---|---|---|---|
| 0001 | Attach a fence to host3d-blob primary-plane RESOURCE_FLUSH (same gate the dumb path has) | 6 | **A** — argued behavior-unchanged on QEMU/crosvm |
| 0002 | Accept ARGB8888 on the primary plane (compositor direct-scanout of alpha client buffers) | 1 | **B** — assumes host treats topmost scanout as opaque; dri-devel will interrogate; may want spec text |
| 0003 | Advertise `DRM_FORMAT_MOD_LINEAR` via the plane modifier list; drop `fb_modifiers_not_supported` | 15 | **A** |

`scripts/provision/f44/README.md:71` flags all three "may already be upstream in
F44's kernel" — **verify against drm-misc-next before sending.**

## Series-level notes (libkrun/imago/linux)

- `patches/libkrun/README.md` documents only ~12 of 40 patches.
- **Policy leakage — the mechanism/policy tenet is violated in exactly one libkrun
  cluster**: the fence-present chain (0017 env/tmpfile gate, 0018 `LIMINA_FENCE_LATCH_MS`,
  0019 `LIMINA_SHOWN_ACK_FD`), plus 0025's `/tmp/limina-*` DIAG hooks. Hard-coded
  constants: vkr ring 63, 35 ms latch, 150 ms ack fallback, 500 ms wedge ceiling,
  exit code 125.
- **Entangled chains** (move together or squash): reset lifecycle 0007→0022→0035
  (+0037); sw-2D/coexist 0001→0008→0010→0015→0027→0030; zero-copy present
  0013→0031→0032; fence-present 0017→0018→0019→0021→0036; balloon 0033→0034; resize
  0025→0026. Cross-repo: 0013/0032 need the virgl fork's IOSurface exports; 0017
  mirrors the fork's present-ring constant; 0018 pairs with **linux 0001**.
- **Rebase risk**: ~20 of 40 patches touch `virtio_gpu.rs`; the rutabaga patches
  (0013, 0030, 0032, 0035) modify libkrun's vendored rutabaga copy, which upstream may
  resync from crosvm wholesale.
- **Risks**: 0021's 500 ms ceiling and 0019's 150 ms fallback are timeout safety nets,
  not root-cause sync; 0018's 35 ms latch is empirically tuned; 0012 defers 16 KiB
  map coherency; 0030 masks genuine guest bugs on non-coexist configs; 0002 is lossy
  with no counter; exit code 125 is claimed unilaterally in Firecracker exit-code space.

---

# Part III — patch triage: virglrenderer, kosmickrisp, mesa, mutter, edk2

## virglrenderer (26 patches on upstream `2048dfb`, ~1.3.0)

| # | What it does | Size | Class |
|---|---|---|---|
| 0001 | Three-in-one: macOS `shm_open` O_CLOEXEC fix; strip host-unsupported device exts at `vkCreateDevice`; add `virgl_renderer_resource_get_map_ptr()` API | ~55 L | **B** (split: shm_open hunk **A**; ext-filter **B**; get_map_ptr API **B/C**) |
| 0002 | eventfd emulation via kqueue/EVFILT_USER + pass fence eventfd by value in same-process render-server mode | ~76 L | **A/B** (generic macOS portability; by-value pass needs upstream buy-in) |
| 0003 | `vkr_mtl_iosurface_alloc` helper: IOSurface-backed MTLTexture for zero-copy scanout | ~137 L | **B** (foundation of the macOS winsys topic; `kIOSurfaceIsGlobal` default walked back by 0022) |
| 0004 | Back guest "external" scanout VkImages with an IOSurface via `VkImportMetalTextureInfoEXT` (MoltenVK fix-A) | ~104 L | **D-leaning B** (MVK import path dead; the `vkr_image.mtl_iosurface` tracking it introduced is load-bearing for 0015) |
| 0005 | Strip DRM-format-modifier structs / normalize modifier tiling→OPTIMAL on external scanout images | ~22 L | **D-leaning B** (MVK-era; idea survives in 0015/0017) |
| 0006 | Ring idle: `cnd_timedwait(2ms)` so a missed notify can't deadlock (#30 mitigation) | ~24 L | **D** — superseded/reverted by 0024 |
| 0007 | IOSurface-backed *exportable* scanout memory (mtl_shm carrier), bind-time image→memory IOSurface link, `virgl_renderer_resource_get_iosurface_id()` API | ~120 L | **B/C** (mechanism generic to macOS venus hosting; API exists for libkrun's SET_SCANOUT_BLOB) |
| 0008 | Zero-init `virgl_context_blob` (stack-garbage iosurface id) | 5 L | fixup of 0007 — **squash** |
| 0009 | Thread `iosurface_id` through the render-server proxy protocol | ~21 L | **B** (inert off-macOS) |
| 0010 | Switch fix-A to MVK `useIOSurface` (`VkImportMetalIOSurfaceInfoEXT`) | ~73 L | **D** (pure MoltenVK) |
| 0011 | #28 fix: share HOST_VISIBLE memory by the driver's own `vkMapMemory` pointer (`map_ptr`) instead of a second shm mapping; proxy plumbing for fd-less replies | ~160 L | **A/B** (the load-bearing macOS coherency model, ports the krunkit/slp approach — core of any upstream "venus on macOS" story) |
| 0012 | Hide `index_type_uint8` from guests — MVK uint8→uint16 conversion corrupts quads | 7 L | **D?** (MVK bug workaround; gated on all of `__APPLE__` so it also hides the feature from KK guests — never re-validated on KK) |
| 0013 | Gate the 0012 hide on `__APPLE__` | ~14 L | **D?** (same topic) |
| 0014 | Documents the two root-caused MVK uint8 bugs — **and accidentally commits 262 binary `.cache/clangd/index/*.idx` files (~65,000 patch lines)** | 16 L code + blob | **D** + hygiene defect: re-export clean |
| 0015 | KosmicKrisp winsys enablement: IFP2 strip/synthesize, force-LINEAR scanout images pitch-matched to an IOSurface, host-pointer-import of the IOSurface base, MTLDevice fallback — plus `GKVM_KK_RTLOG` debug logging mixed in | ~248 L, 8 files | **B** (the KK winsys core; split the debug logging out; message admits a then-open bug) |
| 0016 | `GKVM_KK_RTLOG` per-pipeline dynamic-state/blend-mask logging | ~28 L | **C** (instrumentation; arguably move to a spike patch) |
| 0017 | Cross-context dmabuf import: compositor imports client window buffers via host-pointer import over IOSurface/map_ptr/SHM; proxy attach extension | ~263 L, 14 files | **B** (essential winsys mechanism, macOS-generic; large) |
| 0018 | Gate the IFP2 synthesize to KK hosts, keep MVK pre-synthesize behavior | 7 L | **D** (exists only to protect retired MoltenVK) |
| 0019 | Release backing IOSurface on device/context teardown sweep (leak fix, vmmap-verified) | ~28 L | **B** (folds into the winsys topic) |
| 0020 | `GKVM_RING_RELAX_US` env cap on ring poll backoff — A/B showed **no change** | ~24 L | **C/drop** (dead experiment knob by its own message) |
| 0021 | Present fences on reserved ring 63: barrier-then-zero-submit retire at true GPU completion (#8/#31) | ~248 L, 7 files | **C** (clean mechanism; trigger/consumer is limina's present model) |
| 0022 | Make `LIMINA_GLOBAL_SCANOUT` additive: Mach surface-port publish always, global flag only for iosdump | ~180 L | **C** (limina process-model glue) |
| 0023 | `virgl_renderer_resource_read_iosurface()` CPU readback for the headless capture sink | ~78 L | **C** (limina test-oracle plumbing) |
| 0024 | **seq_cst idle-check tail load; revert to blocking `cnd_wait`** — fixes the store-buffer race behind #30 at root; ~75–150→~2–4 wakeups/s | ~58 L | **A** (genuine upstream concurrency bug, well documented; supersedes 0006) |
| 0025 | **vrend: WebRender/Firefox tile tear** — unsynchronized offset-0 refill treated as orphan → `GL_MAP_INVALIDATE_BUFFER_BIT` | ~25 L | **A** (generic vrend correctness) |
| 0026 | Wire the existing no-GBM surfaceless EGL winsys branch into the dispatcher | 11 L | **A/near** |

## kosmickrisp (6 patches on Mesa main `178a3d73968` — KK is in-tree Mesa; fastest-moving base carried)

| # | What it does | Size | Class |
|---|---|---|---|
| 0001 | Monolithic recovered working-tree delta: xfb NIR lowering, draw-path +484 L, encoder/render-pass/bind cache, queries, external-memory host side, Metal bridge, zink driconf guard, **plus a stray guest `vn_wsi.c` hunk** mirroring `patches/mesa/0009` | 21 files, ~1,344 L | **B** (README's own TODO says split; most content is genuine KK feature work upstream wants) |
| 0002 | Lay out DRM-format-modifier *attachment* images tiled/heap-backed (Metal rejects buffer-backed textures as render attachments) | 14 L | **B** (correct, but premised on guest-venus force-advertised modifiers — see the mesa-0010 coupling below) |
| 0003 | Clamp attachment-less render-pass target size/samples to ≥1 (0×0 renderArea → nil encoder → assert) | ~16 L | **A** (spec-conformance crash fix) |
| 0004 | Give heap-less host-pointer-imported *tiled* image planes their own private heap-backed bo | ~30 L | **A/B** (real `external_memory_host` correctness fix) |
| 0005 | Advertise `VK_EXT_custom_border_color` (impl in tree, never exposed) | 7 L | **A** (subject overclaims "lift zink to 3.2" — reword before sending) |
| 0006 | Advertise `VK_EXT_depth_clip_enable` + honor decoupled clip/clamp — lifts zink-on-KK GL 3.1→3.2 core | ~19 L | **A** |

## mesa (guest-side pool of raw diffs — no commit messages, three different bases)

| # | What it does | Size | Class |
|---|---|---|---|
| 0001 | Backport of Mesa MR !37115: zink nullDescriptor emulation | 405 L | **A** (becomes **D** when the MR lands — track it) |
| 0002 | `do_discard_framebuffer()` NULL `pipe_resource` guard (gnome-shell swap crash) | 40 L | **A** |
| 0003 | zink: gate dmabuf-semaphore *import* on `have_KHR_external_semaphore_fd` | 39 L | **A** |
| 0004 | Same for *export* | 38 L | **A** |
| 0006 | kopper: guard missing surface extensions on the surfaceless/no-WSI path | 51 L | **A/B** |
| 0009 | venus WSI present fix: INVALID-modifier→LINEAR, modifier swapchain image→OPTIMAL + strip pNext, `VkPresentRegionKHR` deep-copy (vs `mesa-26.1.0`) | 199 L | **B** (deep-copy piece is A standalone; rest encodes macOS-host winsys decisions) |
| 0010 | venus: native LINEAR modifier reporting + dmabuf-on-opaque-fd + force-advertise `EXT_image_drm_format_modifier`/`queue_family_foreign` | 282 L | **B** (force-advertise is policy upstream venus must weigh; a "nomod" variant lives in spikes) |
| 0011 | venus/wayland WSI: drop 16-bit-unorm swapchain formats (matches lavapipe; wgpu ghost-ui fix) | 26 L | **A** |

Retired numbers 0005/0007/0008 leave holes. Consumers apply different subsets against
three bases (`build-mesa-zink.sh` vs main `3515c52`; `build-venus.sh` 0009+0010 vs
`mesa-26.1.0`; `scripts/provision/f44/build-mesa-rpm.sh` 0001+0009+0010+0011 vs the
F44 26.1.3 SRPM). `venus-dmabuf-patch.py` re-encodes 0010 a second way, and a third
"nomod" variant lives in `spikes/venus-draw-probe/` — a live fork in a load-bearing
patch.

## mutter (3 raw diffs vs tag 49.5)

| # | What it does | Size | Class |
|---|---|---|---|
| 0001 | #32: cogl stencil-clip degrade when the framebuffer has no stencil + clipped-redraw degrade | 203 L, 6 files | **A** (README: "Upstream MR candidate"; commit message must be authored) |
| 0002 | Guard NULL return of `meta_frame_launch_client` (compositor SIGSEGV on first X11 client) | 14 L | **A** (trivially mergeable) |
| 0003 | Implement `ext-data-control-v1` onto MetaSelection (clipboard bridge for `limina-agent-session`) | 890 L | **C — permanently** (GNOME rejected data-control on privacy grounds, mutter#524; deliberate limina policy for a single-user VM). Budget the per-GNOME-version rebase |

## edk2 (vendor overlay + scripts, not a format-patch series)

| Item | What it does | Class |
|---|---|---|
| `OvmfPkg/VirtioKeyboardDxe/` | Verbatim tianocore files (pinned `edk2-stable202505`) the slp base predates | **D-shaped** — nothing to upstream; evaporates if `slp/edk2@krun-support` rebases past Dec 2024 |
| `apply-virtio-keyboard.py` | Wires the driver into `ArmVirtKrun.dsc/.fdf`, `PlatformBm.c` ConIn callback | **B** for slp's fork |
| (in `build-krun-efi.sh`) | BDS-hang fix, VirtioSerial TPL fix, PlatformBm ConOut patch as in-place sed/string edits | **B** for slp's fork; structurally fragile carrier |

## Series-level notes (graphics)

- **virglrenderer is a chronological history, not a topic series**: the MoltenVK era
  (0003–0005, 0010, 0012–0014, 0018) interleaved with the KK era (0015, 0017); fixups
  as separate patches (0008 fixes 0007; 0024 *reverts* 0006); debug oracles (0016,
  0020, 0023) inline with load-bearing fixes. The macOS/venus enablement — the topic
  upstream could take — is scattered across 0001, 0002, 0007, 0009, 0011, 0015, 0017,
  0019. Naming drift: `gkvm:` subjects, `gkvm/*` branch instructions in the README,
  `GKVM_*` vs `LIMINA_*` env split.
- **kosmickrisp**: base is Mesa **main** and KK is young and active — the biggest
  rebase liability, hence the highest strategic upstreaming priority. 0002–0006 are
  exemplary; 0001 is the debt (split it; evict the stray `vn_wsi.c` hunk). The README
  documents a live KK bug (mutter-50 `kk_encoder.c:299` render-pass-restart tracking)
  fixed nowhere in the series (note: the 2026-06-29 F44 validation never hit it).
- **The one place the mechanism/policy tenet is inverted across repos**: guest
  `mesa/0010` force-advertises dmabuf/modifier support that doesn't exist, and host
  `kk/0002` exists to survive the images that lie produces. Neither is upstreamable
  without the other's context; the pairing already broke once (F44/mutter-50 render
  targets).
- **Fragile carriers**: edk2's in-place string/py patching, and mesa's
  context-diff-pool vs three bases (the mesa README records a fuzzy `patch -F5`
  silently double-applying retired 0007 into a build failure).

## Upstreaming strategy (for the dedicated session)

Destinations: libkrun → github.com/containers/libkrun · virglrenderer →
gitlab.freedesktop.org/virgl/virglrenderer · KK + guest mesa → Mesa upstream ·
mutter → GNOME GitLab · linux → dri-devel · imago → hreitz (gitlab.com/hreitz/imago)
· edk2 bits → `slp/edk2@krun-support`.

**Wave 1 — send now, near-zero friction (~20 patches):**
- libkrun: 0004, 0040, 0039, 0031, 0014+0024, 0037, 0009/0011, 0005, 0006.
- virglrenderer: 0024 (with 0006 dropped, not stacked), 0025, 0026, split-out
  shm_open hunk of 0001 + kqueue-eventfd half of 0002.
- mesa guest: 0002, 0003, 0004, 0006, 0011; track MR !37115 to drop 0001.
- **KosmicKrisp 0003–0006 as individual MRs — strategically most urgent** (fastest
  base, active reviewers; every landed patch is one fewer conflict per Mesa bump).
- mutter 0001/0002 (author commit messages).
- linux 0003 and 0001 to dri-devel — after checking they aren't already upstream.
- imago as a bug report + reproducer (from `spikes/m10-disk-durability/`), let hreitz
  pick the shape.

**Wave 2 — consolidate, then propose:**
- Squash libkrun 0007+0022+0035 into one "persistent GPU worker / renderer is an
  init-once singleton" patch; 0023 (let upstream pick the exit code); 0028, 0029,
  0027, 0015, 0038, 0002/0003, 0033 (`MADV_FREE_REUSABLE` reclaim), 0008;
  0025+0026 after stripping the `/tmp/limina-*` hooks.
- Split kk/0001 into its README-listed components; upstream the generic feature work
  (xfb, queries, external_memory_host) piecewise.

**Wave 3 — design conversations:**
- virglrenderer "venus on macOS" topic: rewrite 0001(rest)/0002(fd-by-value)/0007/
  0008/0009/0011/0015/0017/0019 into a coherent ~6-patch RFC, dropping the MVK arc,
  renaming `GKVM_*`→neutral. Likely objections: no macOS CI upstream, `.m` sources,
  new public APIs need a non-limina consumer — **krunkit/slp is the natural
  co-champion** (0011 explicitly ports their model). Even if it stalls, the
  consolidation halves limina's rebase surface.
- Zero-copy IOSurface present: joint libkrun PR (0013/0032) + virglrenderer MR
  (the exports), cross-referencing.
- Fence-accurate present: libkrun 0017/0018/0021 + linux 0001 as one cross-stack
  "honest flip pacing for blob scanouts" proposal — replace env gating with config
  API, negotiate the reserved ring instead of hard-coding 63.
- libkrun 0001 software-2D as a "renderer-less virtio-gpu mode" feature proposal.
- linux 0002 needs the "host treats topmost scanout as opaque" question answered for
  QEMU/crosvm, possibly as virtio-gpu spec text.
- balloon 0034: generalize `BalloonControlHandle` into a `krun_*` C-API knob first.

**Keep downstream forever:** libkrun 0019 + log-taste patches; imago 0002; virgl
0021–0023 (limina present/oracle plumbing); mutter 0003; the edk2 overlay (best
outcome: slp rebases past edk2-stable202505, deleting the vendored driver).

**Cross-repo couplings to respect when sequencing**: libkrun 0013/0032 ↔ the virgl
fork's IOSurface exports (matching revisions); libkrun 0017 ↔ virgl 0021's ring 63;
libkrun 0018 ↔ linux 0001.

---

# Part IV — future plans & docs state

## Status snapshot

M1 (boot), M2/M2.5 (display/input/console), M3 NAT (bridged deferred), M6 (dynamic
memory), M8 polish (fullscreen/swap/capture/resize), M10 (disks/qcow2/ISO) — shipped.
M4 venus and M5 control-plane/clipboard/virtiofs — substantially done with open
residue. M7 USB — mock end-to-end; real-device deferred to the privileged helper.
M9 — designed, not started. Audio, x86 (FEX), multi-display — not started. M11
productization — barely scoped. Proposals on file: multi-VM networking phases 0–3,
privileged helper, Tailscale.

## Plan-of-record highlights

- **M9 suspend/resume**: host-side VMM snapshot primary (pause vCPUs → serialize
  vCPU+device+GIC+RAM → kill worker → `--restore` fresh worker); guest S4 demoted
  (two libkrun HVF gaps: PSCI CPU_OFF, OSDLR_EL1 — sidestepped). GPU = Strategy A
  (guest re-init; serialization infeasible on venus-on-Metal). **Gated on the undone
  M9.0 spike #2: HVF full vCPU+GIC state round-trip.** Venus resume needs a
  Mesa-venus retain-and-replay that is unsolved upstream — the heaviest piece; the
  virgl tier ships first. Also: no HVF dirty-page log → stop-the-world RAM dump
  (multi-second stall for live snapshots) — surface honestly in UX.
- **USB real-device**: root capture proven (Solo 2); one shared `limina-privhelperd`
  broker (SCM_RIGHTS, death-pact, SMAppService) serves USB + future vmnet. IPC
  protocol deferred until built; not CI-testable.
- **Multi-VM/bridged networking**: phases 0–3 in scope (fd backend, orphan fix,
  `Network` abstraction, `limina-networkd`); vmnet phases 4–6 deferred.
  `com.apple.vm.networking` for BRIDGED may be unattainable → root-helper fallback.
- **Multi-display**: thinnest plan among named features — one roadmap line, no design
  doc, no guest-side analysis. **Audio**: native in-VMM virtio-snd → CoreAudio; the
  GAPS §1.8 claim underpinning "must be native" is still unverified. **x86**: FEX in
  the guest via binfmt_misc; one paragraph; no guest-tools delivery statement.

## Stale docs (fix list)

1. Roadmap M5 still specifies the **rejected** sysext delivery (~lines 495–521) —
   shipped reality is RPM-replace-at-/usr (`tiers.md`).
2. Roadmap M8 lists runtime resize + IOSurface scoping as future — both shipped
   2026-06-23.
3. GAPS §3.1/§4.8 describe pre-pivot M9 (hybrid S4-primary) — pivoted 2026-06-28.
4. hardening-backlog says "F44 enhanced tier blocked (GNOME 49→50 regression)" —
   falsified; validated end-to-end 2026-06-29.
5. Roadmap M4 open item 1 vs item 3 contradict on enhanced delivery being TODO/shipped.
6. `docs/README.md` design index lists 1 of 13 design docs; M1 framed as current.
7. `architecture.md` §2.4 records display model (B) and a stale crate layout.

## Missing plans (largest planned-nowhere areas)

- **Distribution/signing/notarization/updates** (M11 "not yet scoped"; GAPS §3.4):
  Developer-ID + hypervisor entitlement + hardened runtime, helper signing, update
  channel.
- **VM definition/persistence** (GAPS §3.5): per-VM config (disks, networks, memory
  range, MAC, snapshots) — silently presumed by the Network abstraction, M9 named
  snapshots, and the M10 disk manifest.
- In-app guest-tools delivery + version-manifest check; Parallels import tooling;
  image resize/TRIM-policy/compaction; CI (self-hosted Apple-Silicon runner); a
  deliberate deferred-ledger for camera/printing/drag-and-drop/coherence.

## Two-tier floor cracks (documented, unscheduled until now)

- **Stock-tier guest Vulkan fails at the venus ICD instead of degrading to lavapipe**
  (hardening-backlog §M4 residue) — violates the graceful-degradation floor.
- **No keyboard at GRUB / dracut emergency shell** — fix = virtio-input EFI driver in
  the GOP firmware (the driver is already vendored under `patches/edk2/`).

## Risk ranking (vs "replace Parallels")

1. Venus resume is research-flavored and unsolved upstream (flagship-tier suspend).
2. **Patch-stack rebase burden** — mitigated by upstreaming, but only if tracked; no
   doc measures upstreaming progress (the dedicated session should create one).
3. Single-host / macOS-churn exposure (KK/Metal essentially one machine; HVF/madvise
   semantics vary by release).
4. Apple entitlement gatekeeping (bridged vmnet; USB already forced the root path).
5. Product-surface gap: limina is still a CLI (no VM manager, persistence,
   distribution, audio, x86).
6. Two-tier floor cracks (stock Vulkan; GRUB keyboard).
7. Snapshot stop-the-world physics — set UX expectations.

---

## Follow-up status (as agreed 2026-07-01)

All recommendations are being executed in this effort **except upstreaming (item 3),
deferred to a dedicated session** that should start from the Wave 1/2/3 plan and
per-patch tables above, and create the upstreaming tracker doc.
