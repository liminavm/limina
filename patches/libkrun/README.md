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

## Current patches

- **0001 — software 2D virtio-gpu scanout for GL-less hosts (no renderer init).** libkrun
  maps `RESOURCE_CREATE_2D` onto a virgl GL render target, which has no host context on
  macOS, so 2D resource creation fails and nothing reaches the display. Shadows 2D
  resources in host CPU memory (create/attach-backing/transfer/set-scanout/flush) without
  touching rutabaga — a working software scanout baseline (fbcon, EFI GOP, simpledrm).
  Adds an opt-in software-2D-only device mode (`set_gpu_software_2d`) that skips renderer
  init entirely: `rutabaga` stays `None` (renderer-backed 3D/blob/context/capset/fence
  commands degrade to ERR_UNSPEC), and the device advertises a plain 2D virtio-gpu (no
  VIRGL/BLOB/CONTEXT_INIT, 0 capsets) so the guest never issues 3D. This removes the
  virglrenderer/Metal dependency (and its init cost/hangs) from limina's Tier-1 display
  floor. With the mode off, renderer init and the accelerated Venus/blob/3D path are
  unchanged.
- **0002 — PL011 drops serial output on WouldBlock instead of erroring.** The serial TX
  path runs on the vCPU thread; a non-blocking sink (e.g. a pty with no reader) returns
  WouldBlock on a full buffer. Drop the byte rather than stall the vCPU or error per byte.
- **0003 — virtio-console `ConsoleInOut` port + quiet raw-mode ENOTTY.** Adds a
  `PortConfig::ConsoleInOut` that wires non-tty fds (a file + a FIFO) as a *console* port
  (so the guest exposes it as `hvc0`, not a `/dev/vport` data port) with no `isatty`
  gating; downgrades the terminal raw-mode ENOTTY on a non-tty fd from error to debug.
- **0004 — HVF support 16-bit (halfword) MMIO writes.** The aarch64 data-abort write path
  matched only len 1/4/8 and `panic!`ed on len=2, killing the vCPU thread. The ARM PL011
  driver uses 16-bit `writew`/`readw`; the read side already handled len=2. Add the write
  case. (Was the real cause of the apparent "PL011 amba-probe deadlock".)
- **0005 — FDT mark PL011 serial node as arm,primecell.** Lets the guest's AMBA layer bind
  `amba-pl011` and expose a real bidirectional `/dev/ttyAMA0` (the interactive serial debug
  console), instead of PL011 being an earlycon/output-only stdout-path. Safe given 0004.
- **0006 — virtio-mmio default ready queue to max_size when QueueNum unset.** EDK2's
  VirtioGpuDxe over virtio-mmio marks its control queue ready (reg 0x44) without ever
  programming QueueNum (reg 0x38), so our `size`-0 init made `actual_size()`/`pop()` ignore
  the avail ring and the GOP firmware hung in BDS. QEMU tolerates this (vring.num defaults to
  max); we snap a ready-but-unsized queue to `max_size` (the ring the driver allocated from
  QueueNumMax). Compliant drivers (blk/rng/net/console) program QueueNum and are unaffected.
  Unblocked the Track B GOP graphical boot console (VirtioGpuDxe now produces a 1280x800 GOP).
- **0007 — virtio-gpu stop the worker thread on device reset.** The gpu worker ran an
  unbounded blocking loop on the control eventfd with no stop path, and the device had no
  `reset()`. On guest re-init (EFI→kernel hand-off, driver rebind, reboot) the stale worker
  kept running on the freed firmware-era ring, busy-looping `pop()`→None on garbage and
  pinning a CPU so the kernel's fresh virtio-gpu driver never presented. Adopt the
  block/input/fs pattern: worker epolls control + a stop eventfd and exits when signalled;
  the device holds the JoinHandle + stop fd and `reset()` signals+joins then goes Inactive
  (returns true), so re-activation spawns a clean worker on the new rings. With 0006+0007 the
  GOP boot hands off to the kernel and the live console renders (verified: 157 frames).
  **Superseded by 0022** for the reset lifecycle: stop+join dropped the renderer, which
  can't be re-init'd (singleton), so the worker now persists instead of being joined.
- **0022 — virtio-gpu persist the renderer across device reset (singleton fix).**
  `virgl_renderer_init` is a process-global, init-once, thread-bound singleton (a successful
  init leaves a static `INIT_ONCE` set forever; `VirglRenderer::drop` runs
  `virgl_renderer_cleanup` but never clears it). 0007's stop+join dropped `VirtioGpu` →
  `Rutabaga` → cleanup AND left `INIT_ONCE` set, so the next `activate()` re-ran init →
  `AlreadyInUse` → "degrading to software-2D". Every guest-driven reset (EFI→kernel hand-off,
  driver unbind/rebind, reboot) therefore killed venus → llvmpipe — the blocker for the GOP
  boot console and the EFI-booted enhanced tier. Now the gpu worker is spawned ONCE (first
  activate) and lives for the whole VMM process (renderer init'd once, on one thread, never
  dropped); `activate()`/`reset()` message it (`WorkerCmd::Activate/Deactivate/Shutdown` over
  an mpsc channel, woken via the stop eventfd) to bind/unbind the per-activation transport.
  The long-lived fence handler reaches the current queue/mem/interrupt through a shared
  `Arc<Mutex<Option<GpuActivation>>>` swapped on activate and cleared on reset; reset drops
  the session bookkeeping (resources/sw2d/scanouts + fence descriptors indexing the freed
  queue) but keeps rutabaga + the display backend; `Gpu::drop` joins the worker. Verified:
  limina `venus_reset` (unbind/rebind → venus still enumerates) + the full boot suite green.
- **0023 — distinguish guest reboot (PSCI SYSTEM_RESET) from power-off.** HVF collapsed
  `SYSTEM_OFF` and `SYSTEM_RESET` into one `VcpuExit::Shutdown`, so a guest reboot exited the
  worker with `FC_EXIT_CODE_OK` — indistinguishable from a clean power-off, so limina's supervisor
  tore the VM down on reboot. Decode `SYSTEM_RESET` into a distinct `VcpuExit::Reset` →
  `VcpuEmulation::Rebooted` → exit with a new `FC_EXIT_CODE_REBOOT` (125). libkrun stays
  single-shot (a reboot still exits the process); the distinct code lets the supervisor relaunch
  the worker for a fresh boot while keeping the VM's host-side resources (gvproxy, control plane)
  alive. Verified: limina `reboot::guest_reboot_relaunches_the_worker` (a guest `systemctl reboot`
  comes back with a fresh boot id over the same NAT) + the full boot suite green.
- **0024 — implement virtio-gpu `transfer_read` (`TRANSFER_FROM_HOST_3D`).** The readback path
  was a `panic!("unimplemented")` stub: venus (zero-copy host-visible blobs) never reads back, so
  upstream never needed it. But the coexist device drives virgl/vrend for stock 4 KiB guests, and
  vrend's copy model **does** read back — any `glReadPixels` / `glxinfo` / WebGL readback issues
  `TRANSFER_FROM_HOST_3D`. The panic killed the GPU worker thread, after which every later
  virtio-gpu command blocked on a fence that never completed and the whole guest appeared to hang.
  Delegate to rutabaga's `transfer_read` exactly as `transfer_write` does (the scanout 2D readback
  path already calls it); `buf` is `None` on the common path (rutabaga copies the host resource
  into the resource's attached guest iovecs), a provided `VolatileSlice` becomes an `IoSliceMut`;
  software-2D resources already mirror the guest backing so readback is a no-op. Never panic.
  Sibling to 0014 (the same fix for the scanout readback path). Verified: a stock F44 guest runs
  `glxinfo` (renderer `virgl (zink … KosmicKrisp)`) with zero GPU-worker panics — pre-fix it
  wedged the GPU on the first readback.
