# libkrun patch series

limina vendors libkrun under `third_party/libkrun` (gitignored — a from-source checkout we
build against via its internal Rust crates, decision D2.1). Our changes to libkrun live
here as a `git format-patch` series so they survive a re-clone and stay reviewable across
rebases onto upstream.

- **`UPSTREAM_BASE`** — the upstream libkrun commit the series applies onto.
- **`NNNN-*.patch`** — our patches, in order. Author with `git format-patch` from the
  vendored checkout (commit on a `limina/*` branch first), output here.

## Apply onto a fresh checkout

```sh
scripts/apply-libkrun-patches.sh
```

This checks out `third_party/libkrun` at `UPSTREAM_BASE` and `git am`s the series.

## Add / update a patch

1. Edit `third_party/libkrun` directly, commit on a `limina/*` branch (one logical change
   per commit) with a `Co-Authored-By` trailer.
2. Re-export: `git -C third_party/libkrun format-patch <base>.. -o "$PWD/patches/libkrun"`.
3. Commit the regenerated `.patch` files to the limina repo.

## The series (121 patches) — by theme

Patches are listed in series order within each theme. Full rationale lives in each
patch's commit message; this is the map.

### HVF / vCPU correctness

- **0004 — HVF: support 16-bit (halfword) MMIO writes.** The aarch64 data-abort write
  path matched only len 1/4/8 and `panic!`ed on len=2, killing the vCPU thread. The PL011
  driver uses 16-bit `writew` — the missing case looked like a guest "deadlock".
- **0023 — distinguish guest reboot (PSCI SYSTEM_RESET) from power-off.** HVF collapsed
  both into one `Shutdown` exit, so a guest reboot looked like a clean power-off and the
  supervisor tore the VM down. Decode SYSTEM_RESET → `FC_EXIT_CODE_REBOOT` (125) so limina
  relaunches the worker while keeping host-side resources (gvproxy, control plane) alive.
- **0028 — HVF: don't panic the worker on unhandled guest traps.** Unknown PSCI/SMC
  functions now return `PSCI_RET_NOT_SUPPORTED` (spec behavior; the guest degrades
  gracefully); every other unhandled trap logs and tears down cleanly instead of SIGABRT.
  Load-bearing for the stock-guest compatibility floor.
- **0040 — hvf vtimer: multiply before dividing so the WFI timeout isn't ~1.6% short.**
  The tick→ns conversion floored 1e9/cntfrq first, so every timed WFI woke early and the
  guest re-WFI'd — two host wakeups per guest timer deadline. u128 math, idle-wakeup fix.

### Serial & console

- **0002 — PL011 drops serial output on WouldBlock instead of erroring.** A non-blocking
  sink (pty with no reader) returned WouldBlock per byte, flooding the log from the vCPU
  thread. Serial output is lossy when nothing drains it; the vCPU must not stall on it.
- **0003 — virtio-console `ConsoleInOut` port + quiet raw-mode ENOTTY.** A `PortConfig`
  that wires non-tty fds (file + FIFO) as a real *console* port (guest sees `hvcN`, not a
  `/dev/vport` data port) — how the test harness drives a bidirectional console. Also
  demotes the raw-mode ENOTTY on a non-tty fd from error to debug.
- **0005 — FDT: mark the PL011 serial node `arm,primecell`.** Lets the guest AMBA layer
  bind `amba-pl011` and expose a real bidirectional `/dev/ttyAMA0` (the interactive serial
  debug console) instead of an output-only earlycon. Safe given 0004.

### virtio-mmio transport

- **0006 — default a ready queue to max_size when QueueNum is unset.** EDK2's
  VirtioGpuDxe marks its queue ready without ever programming QueueNum, so our size-0
  init made `pop()` ignore the avail ring and the GOP firmware hung in BDS. Match QEMU's
  tolerance: snap a ready-but-unsized queue to max_size. Unblocked the graphical boot
  console; compliant drivers unaffected.

### virtio-gpu: software-2D scanout + coexist

- **0001 — software 2D virtio-gpu scanout for GL-less hosts (no renderer init).** Upstream
  maps `RESOURCE_CREATE_2D` onto a virgl GL render target, which has no host context on
  macOS. Shadow 2D resources in host CPU memory instead (create/attach-backing/transfer/
  set-scanout/flush) — the Tier-1 display floor (fbcon, EFI GOP, simpledrm) — plus an
  opt-in renderer-less device mode (`set_gpu_software_2d`) that advertises a plain 2D GPU.
- **0008 — hardware cursor (service the cursor queue).** Upstream dropped the cursor queue
  and `panic!`ed on UPDATE/MOVE_CURSOR, forcing guests to composite the cursor into the
  scanout (flicker on every pointer move). Adds optional `set_cursor`/`move_cursor`
  krun-display vtable methods; limina renders the cursor as a separate overlay layer.
- **0010 — coexist: route fences by ring + graceful renderer fallback.** Global-ring
  fences (2D/software-2D, incl. the firmware GOP) complete synchronously — a venus-only
  rutabaga can't fence ctx 0, and routing them there wedged boot at the firmware GOP. On
  renderer-init failure degrade to software-2D (rutabaga = None) instead of panicking.
- **0015 — treat cursor resource pixels as alpha-carrying despite XRGB dumb format.** The
  guest kernel hardcodes XRGB for dumb BOs while GNOME writes real ARGB into the cursor;
  promote X formats (as QEMU does) so the overlay isn't an opaque black rectangle.
- **0024 — implement `transfer_read` (TRANSFER_FROM_HOST_3D).** Was a `panic!` stub —
  venus never reads back, but the coexist device drives vrend for stock 4 KiB guests and
  vrend's copy model does (`glReadPixels`, `glxinfo`, WebGL). The panic killed the GPU
  worker and hung the whole guest. Delegate to rutabaga like `transfer_write`; never panic.
- **0027 — present the scanout rect at the resource's stride (de-shear).** The scanout
  resource can be wider than the visible rect (mutter pads its framebuffer); a flat-blob
  copy sheared the desktop into diagonal stripes on non-stride-aligned window resizes.
  Extract just the SET_SCANOUT rect at the resource's own stride; unit-tested.
- **0029 — advertise only the capsets the renderer backs.** The device hardcoded
  num_capsets=5 while rutabaga registered all nine; guests probed capsets we can't serve
  (noisy EINVALs). Derive the capset mask from virgl_flags — venus-only configs now
  advertise exactly one capset.
- **0030 — rutabaga: tolerate coexist 2D-resource attach/detach on a 3D context.** In
  coexist mode the 2D scanout resources live in the software-2D path, but the guest still
  issues CTX_ATTACH/DETACH_RESOURCE for them against its 3D context. Treat the
  missing-endpoint case as an idempotent no-op — zero virtio_gpu dmesg errors at boot.

### virtio-gpu: zero-copy IOSurface present (venus)

- **0012 — map blob via `map_ptr` for upstream virglrenderer 1.3.0.** The blob-map path
  gated on the old slp bottle's `APPLE` fd type, which upstream 1.3.0 doesn't produce, so
  venus host-visible blobs were never hv_vm_map'd into the guest. `map_ptr()` is itself
  the gate; this is what lets the venus feedback/scanout blobs reach the guest.
- **0013 — implement SET_SCANOUT_BLOB as zero-copy IOSurface present.** Previously a
  panic — mutter's first page-flip killed the worker. Plumbs a cached iosurface_id on
  RutabagaResource (from the virgl fork's `virgl_renderer_resource_get_iosurface_id`
  export) and adds an optional `present_surface` vtable method; falls back to readback
  for backends without zero-copy.
- **0014 — don't panic the GPU worker on scanout-readback failure (#30).**
  `read_2d_resource` unwrap'd `transfer_read`, which EINVALs on a blob/venus scanout;
  mutter's secondary-GPU copy fallback hit exactly that and wedged the guest. Propagate
  the error — `flush_resource` already handles it gracefully.
- **0031 — map `KRUN_DISPLAY_ERR_METHOD_UNSUPPORTED` (-2) in `into_rust_result!`.** The
  macro let -2 fall through to the catch-all, so `MethodNotSupported` was misreported as
  `InternalError` and callers never took their fallback (present_surface → readback).
- **0032 — read the venus scanout IOSurface for the headless capture sink.** Venus blobs
  are host-visible zero-copy, so rutabaga's `transfer_read` EINVALs and captured frames
  were blank. New `Rutabaga::read_iosurface` (over the virgl fork's
  `virgl_renderer_resource_read_iosurface` export) feeds the readback path for
  IOSurface-backed scanouts. Needs 0031 and virglrenderer's export.
- **0041 — rutabaga: balance the eager macOS blob map on unref_resource.** The macOS
  `get_map_ptr` path mmap'd every host-visible blob at create time but nothing ever
  munmap'd it, leaking ~2 VM regions per venus context (ring + reply shmem) until the
  worker's address space was exhausted — the session-collapse ENOMEM. Pairs with
  virglrenderer 0022.

### virtio-gpu: fence-accurate present chain (#8/#31)

- **0017 — fence-accurate scanout presents (`LIMINA_FENCE_PRESENT`).** A zero-copy flush
  presented at mutter's *submit* time, before the GPU executed the repaint, so Core
  Animation could sample stale content (#31's convicted race). Park the frame and inject
  a fence on the context's reserved present ring (vkr ring 63 — see the virglrenderer
  fork); present only on true GPU completion. Frames are latency-shifted, never dropped.
- **0018 — hold guest flush fences until present + latch (#8, host half).** With the
  patched guest kernel (`patches/linux/0001`) fencing blob-scanout flushes, hold the
  virtio fence until the parked frame actually presented plus a latch delay
  (`LIMINA_FENCE_LATCH_MS`, default 35 ms) — the guest's commit completes only when its
  frame is on glass. Unpatched kernels send unfenced flushes: behavior unchanged.
- **0019 — complete held flush fences on supervisor shown-acks.** The open-loop 35 ms
  latch serialized mutter to ~17 fps. The supervisor acks `shown <id>` from a
  CATransaction completion block (fd rendezvous'd via `LIMINA_SHOWN_ACK_FD`); the device
  completes the held fence immediately on confirmation, with a 150 ms fallback deadline.
- **0021 — fence-present must never wedge the guest scanout pipeline.** Two hardenings:
  the injection-failure path leaked a parked cookie that hard-wedged the guest's display
  fence (roll it all the way back), and a 500 ms unconditional-completion ceiling — a
  display fence held that long is pathological, but killing the guest's display over it
  is never right.
- **0110 — default fence-accurate presents ON when the shown-ack channel exists.** The
  chain above shipped opt-in and the arming env only ever lived in spike scripts — the
  productized windowed supervisor never set it, so deployed VMs presented
  fire-and-forget (guest flip events pace against pure emulated vblank; see limina
  `spikes/present-miss/`). Policy: unset → on iff `LIMINA_SHOWN_ACK_FD` (windowed
  workers only); `LIMINA_FENCE_PRESENT=0`/`off` forces off; any other value forces on.
  Ack-less sinks (headless capture, GTK) keep the off default — deferred presents are
  unsupported there and the no-ack fallback serializes compositors. **0112** swaps the
  live A/B marker for the default-on world: `touch /tmp/disable-limina-fence-present`
  force-OFFs at runtime (rm to re-arm); the old force-on marker is gone. **0113** moves
  the marker check off the flush path — a 500 ms poller thread mirrors it into an
  atomic (per-flush stat = a sync-I/O stall source on the commit-critical present
  path); toggle latency ≤500 ms.

### virtio-gpu: device reset lifecycle

- **0007 — stop the worker thread on device reset.** The worker had no stop path and the
  device no `reset()`, so a guest re-init (EFI→kernel hand-off, rebind, reboot) left a
  stale worker busy-looping on the freed firmware-era ring. Stop+join on reset.
  **Superseded by 0022** (kept for history — see the squash plan below).
- **0022 — persist the renderer across device reset (singleton fix).**
  `virgl_renderer_init` is process-global, init-once, thread-bound; 0007's stop+join
  dropped the renderer, and the next activate hit `AlreadyInUse` → software-2D — every
  guest-driven reset killed venus. Spawn the worker ONCE for the process lifetime;
  activate/reset message it to bind/unbind the per-activation transport, keeping
  rutabaga + the display backend alive.
- **0035 — drop leaked contexts/resources on a dirty device reset.** A guest that resets
  without clean teardown (compositor crash mid-teardown) leaves contexts/resources in
  the process-global renderer; the recovering session's fresh ids then collide
  (CTX_CREATE → InvalidContextId cascade-crashed gdm, nautilus, ptyxis). New
  `Rutabaga::reset_session_state()`, called from reset_session; a no-op on clean resets.

### Display resize

- **0025 — runtime display resize (config-change mechanism).** A cloneable, thread-safe
  `DisplayResizeHandle` pushes a new display size into the live device: the worker
  applies it, sets `VIRTIO_GPU_EVENT_DISPLAY`, and raises a config-change interrupt so
  the guest re-reads GET_DISPLAY_INFO and re-modesets; EDID regenerates. Mechanism only —
  policy (when/what size) lives in limina. See `docs/design/runtime-display-resize.md`.
- **0026 — expose the GPU `DisplayResizeHandle` to the host VMM.** Carry the handle on
  `Vmm` (`gpu_resize_handle()`) so limina-vmm reaches it without downcasting bus devices.
- **0119 — stable EDID identity, real mode list and monitor range limits.** The generated
  EDID had an anonymous identity (`RHT`/product 1/serial 1/`krun-display`) shared by every
  display of every VM, and one detailed timing built from the current size — enough to
  modeset, not enough for a guest compositor to recognize a monitor or key its remembered
  per-monitor config on it. `EdidParams` gains an optional identity, standard-timing list,
  second detailed timing and range descriptor (emitted "range limits only" + the
  continuous-frequency bit, the only form Linux accepts for a refresh range). Defaults are
  byte-identical to before. Fixes two bugs found on the way: standard timings wrote the
  aspect code unshifted into the refresh field (1600x900 was advertised as 1600x1000 @
  63 Hz), and the detailed-timing pixel clock could wrap its 16-bit field (3024x1964 @
  120 Hz decoded back as 30 Hz — it now saturates and warns). Unit-tested against an
  independent decoder. See `docs/design/stable-edid-hotplug.md`.
- **0120 — runtime display updates carry EDID and connection state.** The pending-resize
  slot becomes a queue of `DisplayUpdate {size, edid, connected}`. `GET_DISPLAY_INFO`'s
  `enabled` flag was hardcoded true; the guest turns it straight into connector status, so
  driving it is a genuine unplug. Consecutive mode/EDID updates merge (a window drag still
  costs one modeset); connectivity changes never merge and the worker applies one update
  per wake, so a disconnect→reconnect reaches the guest as two events rather than
  collapsing into "nothing happened".
- **0121 — log when descriptor slots overflow the base EDID block.** Four descriptors fit;
  a fifth (typically the serial string) is dropped by priority. Say so.

### Balloon / dynamic memory (M6)

- **0033 — reclaim free pages with `MADV_FREE_REUSABLE` + 16 KiB-safe coalescing.**
  `MADV_DONTNEED` returns *nothing* to a macOS host (spike-proven), so free-page
  reporting freed no memory. `MADV_FREE_REUSABLE` debits phys_footprint, coalesced to
  whole host pages with the invariant that a 16 KiB host page is reclaimed only when
  every constituent 4 KiB guest page was reported free.
- **0034 — inflate/deflate, target/actual, DEFLATE_ON_OOM, `BalloonControlHandle`.**
  Implements the stubbed inflate/deflate queue handlers (persisted inflate coalescer,
  safe across heads because inflated pages stay balloon-owned) and a host-driven target
  via a config-change interrupt, so limina can cap effective guest RAM at runtime.

### virtio-input

- **0037 — return to Inactive on `reset()` so the device can re-activate.** `reset()`
  never cleared device_state, so the transport skipped re-activation and the guest got
  zero input events whenever the device was driven twice — which the EDK2
  VirtioKeyboardDxe→kernel hand-off does on every GRUB boot. Same lifecycle family as
  0007/0022.
- **0039 — worker blocks (epoll -1) instead of a pointless 1 s timeout.** Every fd it
  needs is event-driven and it did no work on expiry — the timeout was a pure ~1 Hz idle
  wakeup. Shutdown still rides the stop eventfd.

### virtio-blk

- **0038 — serial = block_id (stable `/dev/disk/by-id/virtio-<id>`).** The GET_ID serial
  derived from host st_dev/st_ino, which changes across an APFS clone or image move —
  exactly the snapshot-clone path. Build it from the caller-supplied block_id instead;
  empty id falls back to the inode-derived serial, so stock boot is unaffected.

### virtio-i2c

- **0042 — new device with an emulated SBS smart battery slave.** virtio-i2c adapter
  (device ID 34) whose only slave is an SBS battery at 0x0b, backed by a host-supplied
  `BatteryProvider` callback (`VmResources::battery_provider`); the FDT child node
  (`virtio,device22` → `sbs,sbs-battery@b`) makes the guest's stock i2c-virtio +
  sbs-battery modules expose it as a native power_supply — the host battery mirrors
  into the guest desktop with zero guest-side components.

### USB (xHCI)

- **0095 — usb/xhci: emulated xHCI controller bring-up.** A native platform xHCI
  controller (`compatible = "generic-xhci"`), feature-gated `usb` and off by default, so
  a stock Fedora guest binds it with its own `xhci-plat` driver and brings the HCD up with
  the root hub registered. Wave 1 of `docs/design/usb-xhci.md`: a functional register file
  (capability / operational / runtime / doorbell regs + one USB 2.0 Supported Protocol
  extended cap) with a stub data path — no ring/TRB processing yet. `DeviceType::Xhci` +
  `create_xhci_node` (FDT, one edge SPI, dma-coherent) + a 64 KiB MMIO window via
  `register_mmio_xhci` in `device_manager/{hvf,kvm}/mmio.rs` + `VmResources::usb`.
- **0096 — usb/xhci: Stage B1 — rings, command set, EP0 control transfers, gadgets.**
  The data path: command/event/transfer ring walkers (cycle-bit + Link-TRB, hostile-input
  bounded), 32-byte contexts, the command set (enable/disable slot, address device,
  configure/evaluate endpoint, stop/reset/set-TR-dequeue, reset device), the EP0
  control-transfer state machine, port enumeration, and the `UsbDeviceModel` trait
  (mechanism seam) with a ring worker thread that calls gadgets **with the controller lock
  released** (deferred completion from any thread). A stock guest now *enumerates* an
  attached device model.
- **0097 — usb/xhci: Stage B2 — non-EP0 data flow + mock HID echo gadget.** Interrupt/bulk
  endpoints: Configure Endpoint stands up per-DCI transfer rings the worker walks on their
  doorbells; IN transfers with no data are *held* (the NAK analogue) until the gadget
  completes them, OUT transfers deliver the guest's bytes. A per-endpoint **generation
  counter** captured in each completion closure makes cancellation safe (Stop / Reset
  Endpoint, Set TR Dequeue, Disable Slot / Reset Device bump/tear-down it; a stale
  completion is dropped) — the QEMU "transfers in flight" rough edge, done right.
  Class/vendor GET_DESCRIPTOR now forwards to the gadget. Adds `hid.rs`, a full-speed HID
  echo gadget (0x1d6b:0x0f11) exercising held-IN + deferred completion + both directions.
- **0098 — usb/xhci: generic HID report-pipe gadget (Stage C mechanism).** `report_pipe.rs`:
  `HidReportPipe`, a full-speed HID gadget whose fixed-size IN/OUT reports are shuttled
  verbatim over a caller-supplied `ReportSink` (guest→host) + `push_in` (host→guest), with no
  knowledge of what the frames mean. `HidMockDevice` generalised into reusable mechanism —
  limina wires it to the CTAPHID/Secure-Enclave authenticator (policy) to present the
  stock-tier FIDO key. Held set stays bounded (one outstanding IN; a new IN supersedes a stale
  hold, `reset()` drops it) across the hidraw open/close churn FIDO clients cause.
- **0099 — usb/xhci: generic bulk-pipe gadget (MOC fingerprint mechanism).** `bulk_pipe.rs`:
  a variable-length, multi-IN-endpoint bulk gadget shuttling opaque byte frames over a
  caller-supplied sink — the mechanism limina gives the impersonated Elan match-on-chip
  fingerprint reader's identity and protocol (policy).
- **0100 — usb/xhci: handle Immediate Data (IDT) on Normal TRBs.** Linux packs SMALL bulk-OUT
  payloads INLINE in the TRB parameter field (xHCI §4.11.7) rather than at a DMA pointer.
  `read_data_td` treated the parameter as a guest address unconditionally, so the 2–4 byte
  elanmoc commands read garbage from wherever those bytes happened to address and the engine
  stalled. FIDO/HID never hit it — their transfers are large enough to use DMA buffers.
- **0101 — usb/xhci: bound work TRBs per data TD (hostile-ring DoS guard).** A TD whose Chain
  bit never clears would grow the collected buffers without bound; refuse after `MAX_TD_TRBS`.
- **0102 — usb/xhci: PM-correct register semantics (suspend/resume).** Seven register-file bugs
  that between them broke a guest's USB stack across a system suspend, found by reading Linux's
  `xhci_suspend`/`xhci_resume` and `xhci_bus_suspend`/`xhci_bus_resume` against ours: `CRCR`
  stop/abort must not rebase the ring (Linux's watchdog writes `0x4`, and `CRCR` reads back as 0,
  so rebasing pointed the walker at guest address 0 — a permanent brick reachable from any
  command timeout) and must be acknowledged with a Command Ring Stopped event; never walk a null
  command ring; rebuild the event ring only when `ERSTBA`'s base actually changes (a resume
  rewrites the same value, and resetting the producer desynced it from the guest's mid-ring
  consumer); an idempotent run-edge port scan; `PORTSC.PLC` latched when the link reaches U0 (the
  guest polls it for 10 ms and skips `xhci_ring_device` on timeout); `USBCMD.CSS`/`CRS` self-clear.
- **0103 — usb/xhci: carry controller state across a VM snapshot.** A snapshot-suspend tears the
  worker down, so the controller is reborn blank — while the guest suspended through xHCI's own
  `USBCMD.CSS` and light-resumes assuming its state survived; its first step is a `USBSTS.CNR`
  handshake with a **10-second** timeout that a fresh controller can never satisfy, so the HCD is
  declared dead and USB is gone for the session. Adds `devices::usb_state::XhciState` (registers,
  ring positions, slots/endpoints, per-port gadget identity) and a snapshot v7 section restored
  into the fresh controller before the guest resumes. Design invariant: after restore the fresh
  controller must be indistinguishable from the in-place one.
- **0104 — usb/xhci: trace the PM register writes (suspend/resume oracle).** Unconditional debug
  logging of USBCMD and PORTSC writes. Load-bearing rather than cosmetic: Linux is defensive
  enough that a mishandled port resume produces no guest-side signal at all (same dmesg, same
  devnum, working device), so this trace is the only thing that distinguishes broken from fixed —
  and the L2 guard asserts on it. Read with `RUST_LOG=krun_devices=debug`.
- **0105 — usb/xhci: an abort cancels a command doorbell queued before it.** `run_worker_pass` took
  `cmd_abort` but left `cmd_doorbell` set, so one pass could post Command Ring Stopped and then
  still execute the aborted commands. `xhci_handle_stopped_cmd_ring` rewrites each of them to a
  no-op and only then re-rings, so running them ran commands the guest had cancelled. Also
  strengthens the save/restore round-trip so every carried field is individually pinned — the
  capture now holds an in-flight `CRCR` stop, an undrained work queue, and both ring cycle-bit
  polarities (a fresh ring starts at `true`, so without a `false` one a restore that hardcoded the
  default would round-trip clean). 46 RED cases via `scripts/xhci-red-check.py`, which also gained a
  baseline pre-pass so a renamed guard test can no longer report RED while guarding nothing.
### Observability / logging

- **0009 — log renderer-init failure instead of swallowing it.** `create_rutabaga` used
  `.build(...).ok()`, discarding the actual RutabagaError behind a generic fallback line.
- **0011 — log `hv_vm_map` failures with alignment breakdown.** The wrapper discarded
  HV_BAD_ARGUMENT; logging the operands and which is misaligned is how we diagnosed the
  4 KiB-guest-blob / 16 KiB-host mapping mismatch.
- **0016 — log context lifecycle and error responses at visible levels.** CTX_CREATE/
  DESTROY at info with the guest process name, RESP_ERR at warn with the precise rutabaga
  error — a guest whose context creation fails otherwise degrades with no host trace.
- **0020 — demote per-frame FLUSHDBG/SET_SCANOUT_BLOB lines to debug.** Both fired ~60/s
  and dominated the worker log at Info. Log-level fixup of earlier patches (squash — see
  below).
- **0036 — demote per-frame present DIAGs (`[FLUSH2]`/`[FENCEPRESENT]`) to trace.** Keeps
  the flicker-hunt oracles in-tree without the spam; recover them with `RUST_LOG=trace`.
  Log-level fixup (squash — see below).

## Upstreaming & planned squashes

From the 2026-07-01 full review (`docs/reviews/2026-07-01-full-review.md` Part II, which
holds the per-patch **A/B/C/D upstreamability triage** — A upstreamable as-is/near, B needs
rework, C keep downstream, D obsolete/superseded; census: 22 A, 12 B, 1 C, 3 D, 2 B/C):

- **0007 is superseded by 0022** (stop+join → persistent worker) and is kept only for
  history. At the next series restructure / upstream submission, **squash 0007 + 0022 +
  0035 into one reset-lifecycle patch**.
- **0020 and 0036 are log-level fixups** of earlier patches in this same series — squash
  each into its parent at the same time.
- **The fence-present chain (0017/0018/0019/0021)** carries limina-shaped gating —
  `LIMINA_FENCE_PRESENT` env / `/tmp/limina-fence-present` marker, `LIMINA_FENCE_LATCH_MS`,
  `LIMINA_SHOWN_ACK_FD` — that must be replaced with a proper config API before
  upstreaming. **0019 stays downstream** (it is limina supervisor-protocol glue).
- **Cross-repo couplings:** 0013/0032 pair with the virglrenderer fork's IOSurface exports
  (`virgl_renderer_resource_get_iosurface_id` / `_read_iosurface`); 0017 mirrors the
  fork's reserved present-ring constant (vkr ring 63); 0018 pairs with
  `patches/linux/0001` (the guest-kernel flush fence).
