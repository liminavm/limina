# Snapshot/restore of a Vulkan-compositor desktop

Four faults, all ours, all found on `Fedora-Workstation-44.enhanced.synoik.raw` with a seated
synoik session. Three are fixed; the fourth is fixed for the resume path a user actually takes
and open for one that a test harness takes.

## What the snapshot could not see, and what it does now

**The desktop came back black.** A venus scanout's pixels live in host-side memory the guest
image is bound to, so guest RAM — which the snapshot carries whole — holds none of them. Payload
v8 carries the presented scanout frame itself and re-presents it at restore.

**Then the session never updated again.** Replay leaves holes; the guest names an object that is
not there; `vkr_cs_decoder_lookup_object` misses, the ring goes FATAL and stops consuming, and
the guest spins in `vn_relax` inside its next allocate forever
(`wait_ring_seqno STUCK ctx 2 want=801848 head=801368 tail=801992`). A restored context now
treats a lookup miss as a soft error: it trades the FATAL detector for liveness, for its whole
life, deliberately.

**Client windows came back blank until they repainted.** Those buffers are a client's
dedicated, exported image memory, and on KosmicKrisp that memory is an MTLTEXTURE (or
host-pointer) import of an IOSurface — `vkMapMemory` answers `VK_ERROR_MEMORY_MAP_FAILED`
because the storage is not the driver's to map. Capture reads the IOSurface instead. Never the
memory's `mtl_shm` carrier: those bytes are never the pixels, and reading them would put
garbage in the snapshot silently.

Measured on a ghost + synoik session, 2026-08-29: capture went from six refusals
(4 × 14221312 client, 2 × 14745600 compositor) to **zero**, 32 memory contents / 159 MiB. After
the resume, a `notify-send` — a full recomposite that ghost took no part in, no input, no frame
callback — showed ghost's window with every landmark line intact (`probe/m-pre.png`,
`probe/m-post.png`).

**The boot text flashed on every resume.** Replaying the journal's `SET_SCANOUT` ops flushed
each resource as it re-bound it, and on a Vulkan desktop the last classic scanout in that journal
is the early-boot console. It is presented for the moment before the saved frame lands. Bind them
all, present only where no saved frame follows. This was never the cold console of a fresh
worker: it appears on a click resume in the same supervisor too.

## The fifth fault: a restore cycled the connector under a live compositor

A VM resumed into a **fresh supervisor** left its compositor alive, sleeping, and never
compositing again — while the in-process click resume was healthy end to end. The difference is
one display push:

| fresh-supervisor restore | the guest's log | result |
| --- | --- | --- |
| connector cycle (the default) | `disconnecting connector "Virtual-1"` -> `missing surface in vblank callback` | 2 applies, none after; no repaint revives it |
| in place | `device changed` -> `driving saved mode 2560x1440` | composites on demand, correct desktop |

The chain is ours from end to end. A fresh window starts its display table in the **firmware**
phase although the restored guest's driver has been up since before the suspend. The restore's
re-probe nudge produces a handover, the handover forgets what the guest was told — correct after a
cold boot, where a virtio driver's probe reset can eat an EDID pushed before it — and the next tick
therefore re-announces the identity as a MIGRATION, which cycles the connector. The unplug reaches
a compositor holding a live CRTC.

A restore has nothing to cycle: same panel, same identity, and the in-place push carries that
identity anyway, which is what the in-process resume has always done. The exemption is take-once,
so every later migration is still a real one. **mutter survives the cycle** — which is why this
lived behind a Vulkan compositor, and the general rule it leaves: a path that only a stock guest
exercises is a path whose faults only an enhanced guest will report.

## Facts worth keeping

- **P2 classic content is byte-complete** — 194 exported / 0 skipped, 194 restored / 0 dropped
  across 7 contexts. Phase C's guest-shadow re-upload refuses 61–97 transfers per restore (244 on
  the dogfood Mac) and is superseded by P2; that loop wants deleting.
- **Every replay drop is FREE-class.** A partial batch free is keyed to its pool, so it outlives
  the allocate that was pruned when its ids all died, and is dropped harmlessly. `free`=124 with
  everything else at 0 is a healthy restore, not a lossy one.
- **The dogfood Mac drops in classes the local runs never hit** — `recording`=41, `noted`=9
  alongside `free`. Both are logged per entry only at `debug`, so a dogfood repro under
  `RUST_LOG=...krun_devices=debug` would name which state was lost.
- **A still workload legitimately presents nothing.** A desktop that is as still as it will ever
  be reaches the host as a handful of frames a minute, so a capture cadence tuned for a live
  session can end a whole probe with no frame — `LIMINA_WINDOW_CAPTURE_INTERVAL_MS` exists for that (the capture cadence is timed, not counted in applies).
- **A `SIGSTOP`ped Vulkan client appears to hold out the suspend quiesce.** One uncontrolled A/B:
  the worker never reached exit 126 within 120 s where the unfrozen run suspended in seconds.
  Worth an explicit repro; it is the obvious suspect for "this guest will not suspend" where a
  GPU client is stopped mid-frame.
- **`limina suspend <disk>` leaves the supervisor alive** after the worker snapshots and exits.
  It keeps its gvproxy and SSH port, and the next `limina suspend` on that disk refuses with
  "multiple limina supervisors match".

## The oracles this left behind

`l2_synoik_restore_landmarks` asserts zero content losses, counting the `vkMapMemory REFUSED`
refusal line among them, and zero ring stalls — a wedged ring holds the last correct frame, so a
landmark diff passes on a dead session and only the stall line sees it. The vkstill asset takes
`VKSTILL_IDLE_AFTER=<n>`: a client that presents every frame repaints itself and hides the fault
the gate exists to catch.
