# xHCI across suspend/resume — corrected diagnosis + implementation plan

**Status:** SHIPPED 2026-07-25 — kept as the implementation record (the Linux-source citations and
the phase → test mapping). `README.md` is the current summary; read that first. Two things this
plan got wrong, corrected there: bug **D**'s effect (Linux resuscitates the device rather than
re-enumerating it, so the in-place path has no user-visible failure — measured, §4), and the
`PLC` latch rule (narrowed to the →U0 transition after review, so a bus *suspend* does not raise a
port event).

Author date: 2026-07-25. Sources read for this plan (ground truth, not memory):
`drivers/usb/host/xhci.c` and `xhci-hub.c` at **v7.1** (fetched from kernel.org; the enhanced tier
runs 7.1.4 and stock F44 is close enough — the PM paths are unchanged since 6.12, checked), plus
`third_party/libkrun/src/devices/src/usb/xhci/{device,engine,trb,context}.rs` and
`src/vmm/src/{snapshot,lib,builder}.rs`.

## 1. What the guest actually does (Linux v7.1)

### Suspend — `xhci_bus_suspend` then `xhci_suspend`

`xhci_bus_suspend` (xhci-hub.c:1688), for each **enabled port in U0**:

1. `xhci_stop_device(slot_id, 1)` — a Stop Endpoint command per live endpoint.
2. writes `PORTSC` with `PORT_LINK_STROBE | XDEV_U3` (LWS + PLS=U3) and records the port in
   `bus_state->bus_suspended`.

`xhci_suspend` (xhci.c:958):

3. clears `USBCMD.RS`, handshakes `USBSTS.HCH` → 1.
4. `xhci_clear_command_ring` — zeroes the command-ring TRBs, resets its *software* enqueue/dequeue
   to the segment base with `cycle_state = 1`, and writes `CRCR = base | 1`.
5. `xhci_save_registers` — reads USBCMD, DNCTRL, DCBAAP, CONFIG and, per interrupter, ERSTSZ,
   ERSTBA, ERDP, IMAN, IMOD into `xhci->s3`.
6. writes `USBCMD |= CMD_CSS` (bit 8) and handshakes `USBSTS.SSS` (bit 8) → 0, then checks
   `USBSTS.SRE` (bit 10) — a *warning only* here. (`broken_suspend = 1` is set only in the
   `XHCI_SNPS_BROKEN_SUSPEND`-quirk timeout branch, xhci.c:1027-1052. `SRE` forces the destructive
   path at **resume**, not suspend.)

### Resume — `xhci_resume` then `xhci_bus_resume`

`xhci_resume` (xhci.c:1081) with `power_lost == false` (our case — nothing tells the guest power
was lost):

1. **handshakes `USBSTS.CNR` (bit 11) → 0, with a 10-SECOND timeout.** On timeout it logs
   `"Controller not ready at resume"` and **returns an error — the whole USB stack is dead from
   here on.**
2. `xhci_restore_registers` — writes back USBCMD, DNCTRL, DCBAAP, CONFIG, ERSTSZ, ERSTBA, ERDP,
   IMAN, IMOD.
3. `xhci_set_cmd_ring_deq` — writes `CRCR = deq | cycle`.
4. writes `USBCMD |= CMD_CRS` (bit 9), handshakes `USBSTS.RSS` (bit 9) → 0 (100 ms), then reads
   `USBSTS`: **if `SRE` or `HCE` is set it falls into `reset_registers`** — halt, reset, free every
   virt device, `xhci_init`, `xhci_run`: a full re-enumeration of everything.
5. writes `USBCMD |= CMD_RUN`, handshakes `HCH` → 0.

`xhci_bus_resume` (xhci-hub.c:1844), USB2 root hub:

6. disables interrupter 0 (`IMAN.IE = 0`) for the duration.
7. for each `bus_suspended` port: **if its PLS is not U3 it drops the port from `bus_suspended`
   and does nothing further for it**; otherwise writes `LWS | XDEV_RESUME`.
8. sleeps `USB_RESUME_TIMEOUT` (40 ms), clears `PORTSC.PLC`, writes `LWS | XDEV_U0`.
9. **handshakes `PORTSC.PLC` (bit 22) → 1 with a 10 ms timeout.** On timeout: `"port %d-%d resume
   PLC timeout"` and `continue` — which **skips `xhci_ring_device(slot_id)`**, the doorbell ring
   that restarts every endpoint the Stop Endpoint in step 1 stopped.
10. re-enables interrupter 0.

## 2. Corrected root cause — five independent bugs, not one

The `README.md` trace said the guest "light-resumes without reprogramming anything". The source
says it *does* reprogram — so the trace stopped early, and the reason it stopped is bug **(A)**.
Everything else in that trace (the `BadAccess(0)` loop) is bug **(B)** firing downstream.

| # | Bug | Effect | Path |
|---|---|---|---|
| **A** | `XhciDevice::new()` sets `USBSTS.CNR`, and only `HCRST` clears it. A restore worker's controller is therefore born `CNR=1`. | `xhci_resume` step 1 spins **10 s** and then fails. The USB stack is dead before it can restore a single register. | snapshot only |
| **B** | `CRCR_LO` write stores the value and rebases the walker unconditionally. Linux's `xhci_abort_cmd_ring` writes `CRCR = 0 \| CMD_RING_ABORT` (the pointer field reads back as 0 per spec, so it writes 0 in the pointer bits). | `crcr_ptr() == 0` → the next doorbell walks guest address 0 → `command ring walk error: BadAccess(0)` every 5 s forever. **A brick reachable from any command timeout**, not just resume. | both |
| **C** | An `ERSTBA` write always sets `event_ring = None`, so the producer is rebuilt at `enqueue_idx = 0, pcs = 1`. `xhci_restore_registers` rewrites ERSTBA with **the same value** while the guest's ERDP/CCS sit mid-ring. | Producer/consumer desync: events written into already-consumed slots are invisible to the guest until the producer walks back around to its ERDP. Lost completions → command timeouts → (B). | both |
| **D** | `scan_ports_on_run` re-latches `CCS \| CSC` and forces `PLS = Polling` on every populated port at each `USBCMD.RS` 0→1 edge — including the one in `xhci_resume` step 5. | The guest's `bus_resume` (step 7) sees PLS=Polling, not U3 → gives up on resuming the port; and `CSC` makes the hub thread treat it as a **fresh connect → disconnect + re-enumerate**. Devices blink out/in on *every* resume. | both |
| **E** | `write_portsc` honours LWS (it stores the new PLS) but never sets `PLC` and never posts a Port Status Change Event. | `bus_resume` step 9 always times out → `xhci_ring_device` is **never** called, so endpoints stopped in step 1 are not restarted, plus a 10 ms stall per port. | both |

Note the column: only **(A)** is snapshot-specific. **B–E are equally wrong on the in-place
s2idle path** — which is why that path "passes" its test (the guest re-enumerates the device
behind our back and the test only asserts the clock) rather than actually preserving USB. This
plan fixes the shared semantics first and the snapshot carry second.

`SRE`/`HCE` are never set by our controller, so steps 6/4 above take the good branch — worth
keeping that way: setting `SRE` is the switch that turns a resume into a full re-enumeration.

## 3. The design invariant

> **After restore, the fresh controller must be indistinguishable from the in-place one.**

Every guest-visible difference between "same worker, state retained" and "fresh worker, state
restored" is a bug. That reduces the snapshot work to a mechanical question — *which fields of
`XhciDevice` are not reconstructible from guest RAM or from the fresh worker's own construction?*
— and makes the in-place path the reference implementation. Concretely, `XhciDevice` has exactly
three classes of field:

1. **Carried** (host-only, guest-visible): `usbcmd usbsts dnctrl crcr dcbaap config ports[]
   iman imod erstsz erstba erdp`, the `cmd_ring` walker, the `event_ring` producer, `slots`,
   `next_address`, and the pending `work` queue.
2. **Re-established by the fresh worker**: `intc`, `irq_line`, `interrupt_evt`, `worker_kick`,
   `port_models`.
3. Nothing else. (A `#[deny]`-style comment on the struct plus a round-trip test keeps that
   honest when a field is added.)

Per-slot state to carry, explicitly: `SlotCtx { port, address, state, config_value, ep0_ring:
Option<(u64, bool)> }` plus, per endpoint, `{ dci, ring: (u64, bool), dir_in, ep_type, state,
generation }` (`device.rs:186-217`); and the event ring needs **all four** of `{ seg_base,
seg_size, enqueue_idx, pcs }` (`trb.rs:234-239`) — re-reading the segment size from the guest's
ERST would be exactly the RAM parse we are avoiding.

The plan deliberately does **not** re-derive `slots` from the DCBAA, unlike the virtio-transport
precedent. The reasons that actually hold up: `config_value` is not in guest RAM at all; the
carried bytes are host-authoritative, whereas a DCBAA walk is a parse of hostile guest RAM whose
failure modes are all silent (a bad pointer yields a plausible-looking empty slot); and it is less
code. (Two weaker arguments to *not* lean on: the per-endpoint `generation` could safely reset to 0
— the restore worker is a new process, so no stale completion closure can exist; and the output EP
contexts *are* reliably truthful at quiesce, since Stop Endpoint publishes every data-EP dequeue
at `engine.rs:293-295` and every completed control TD publishes EP0's at `engine.rs:846-848`.) The
"derive it" argument that applied to virtqueues — where the rings genuinely *are* the truth in RAM
— doesn't transfer.

**In-flight and queued gadget state is not carried.** A gadget-held transfer (the FIDO/fingerprint
held IN), a queued host→guest report (`report_pipe.rs:54` `PipeState.queued`) and a queued STALL
marker (`bulk_pipe.rs:55-69` `InEndpoint.events`) all live in the worker's gadget objects and die
with them. This is *mostly* sound: `xhci_bus_suspend` issues Stop Endpoint on every live endpoint
before the freeze, which bumps our generation counter and drops such completions, and the class
drivers kill + resubmit their URBs across a system sleep, so the endpoint restarts from a fresh TD
once the guest's `xhci_ring_device` rings it (bug **E**'s fix). The residual, accepted loss: a
reply already queued at capture is gone, and a URB that survives suspend *un-killed* (the usbfs
edge) has no TD left on our side to complete — our walker commits past a dispatched TD
(`engine.rs:911-914`), so a re-ring finds nothing to fetch. That operation hangs until the client
retries. Not a wedge, and out of scope; the capture logs the counts so it is diagnosable. Note the
consequence for Phase 3: an identity-only oracle cannot see this, so both guards need a real
post-resume **data-path** exchange, not just `devnum` equality.

**Gadget-side protocol state survives independently:** the elanmoc engine + template store and the
FIDO authenticator + passkey store live in the **supervisor**, which is not torn down; both serve
loops already reconnect to the fresh worker's socket (`crates/limina/src/moc/usb.rs:50`,
`crates/limina/src/fido/usb.rs`). Nothing to do there.

**Port identity is validated, not assumed.** The fresh worker cold-plugs gadgets in a fixed order
(`crates/limina-vmm/src/krun/mod.rs:190`), but each `build()` can fail independently, which would
*shift* the remaining gadgets down a port and silently bind slot→wrong device. So the capture
records each populated port's `(idVendor, idProduct)` (bytes 8..12 of the model's device
descriptor — no trait change needed) and the restore compares. On mismatch for a port: log loudly,
restore that `PORTSC` as the full disconnected value (`PORTSC_DEFAULT | CSC` — powered, RxDetect,
connect-change latched), queue a Port Status Change Event, and drop the slots bound to it. The
guest's hub thread then sees `CSC` with `CCS == 0` → a clean `usb_disconnect`; its teardown
commands against a now-`None` slot all complete SUCCESS today (`engine.rs:246-253`, `282-319`), so
dropping the slot cannot wedge it. And the *shifted* gadget recovers for free through fix **D**:
that port is restored `CCS == 0` but still has a model, so the guest's resume `RS` edge latches a
fresh connect on it → disconnect-old, enumerate-new. (Which is why fix **D** must skip ports by
`CCS`, *not* by "was this port reconciled".)

## 4. Work plan

Each phase is a self-contained commit with its own RED-first test. Phases 1 and 2 are separate
libkrun patches (`patches/libkrun/`, branch `limina/usb-xhci`).

### Phase 1 — PM-correct register semantics (bugs B–E) + (A)

All five are register-file behaviour, so all five get **L0 unit tests in `device.rs`** — no boot,
sub-second, and each fails before the fix:

| Test | RED behaviour today |
|---|---|
| `crcr_abort_write_does_not_rebase_the_ring` | program CRCR, ring DB0 (ring walks fine), write `CRCR_LO = 0x4` (CA), ring DB0 → walker rebased to 0, `crcr_ptr() == 0` |
| `command_ring_is_never_walked_at_address_zero` | with `crcr == 0`, a DB0 doorbell walks address 0 |
| `command_ring_abort_posts_a_command_ring_stopped_event` | no event → guest leaves the ring ABORTED forever |
| `erstba_rewrite_with_the_same_base_keeps_the_producer` | producer resets to index 0 mid-ring |
| `run_edge_does_not_relatch_an_already_connected_port` | `CSC` re-latched + PLS forced to Polling |
| `link_state_write_to_u0_latches_plc_and_posts_a_port_event` | no `PLC`, no queued port event |
| `link_state_write_to_u3_does_not_latch_plc` | (guards the narrowed rule below) |
| `css_and_crs_are_self_clearing` | both stay set in `USBCMD` forever |
| `a_fresh_controller_is_ready_after_state_restore` | (Phase 2) `CNR` still set |

Fixes:

- **B**: `CRCR` write — if `CS (bit 1)` or `CA (bit 2)` is set, treat it as stop/abort: leave
  `crcr` and `cmd_ring` alone. Otherwise store + rebase (the guest only ever writes the pointer to
  rebase to the segment base, so unconditional rebase on a pointer write is right). **The gate must
  cover both dword halves**: Linux writes CRCR with a 64-bit `xhci_write_64`, which our
  `BusDevice::write` splits into a lo-dword then a hi-dword `write_reg32` (`device.rs:756-764`), and
  the CA/CS bits live only in the lo dword — gating just the lo arm would let the hi arm (value 0)
  clobber the high half and re-null the walker anyway. Independently, `process_command_ring` refuses
  to walk when `crcr_ptr() == 0` and there is no live walker.
  Plus, **required, not optional**: an abort (`CA`) must post a **Command Ring Stopped**
  (`cc = 24`) Command Completion Event. Without it `xhci_handle_stopped_cmd_ring` never runs,
  `cmd_ring_state` stays `ABORTED`, and *every* subsequent command is refused — so fixing only the
  pointer silences the `BadAccess` spam while leaving USB just as dead. Needs a `work.cmd_abort`
  flag so the worker (which has guest memory) posts the event, not the vcpu thread.
- **C**: rebuild the event ring only when the masked `erstba` actually changes. (Safe against the
  two-halves write only because the ring is built lazily at the next worker pass,
  `engine.rs:105,132`, so a transient half-programmed value never builds one — worth a comment.)
- **D**: `scan_ports_on_run` skips ports already `CCS`.
- **E**: **narrow** — an LWS write that lands the port in `U0` from a non-`U0` state latches `PLC`
  and queues a Port Status Change Event (QEMU's semantics). *Not* "any PLS change": the broad rule
  would also fire on `xhci_bus_suspend`'s `LWS | U3` write (`xhci-hub.c:1760,1804`), latching a
  PORTSC change bit and posting a port event exactly as the system suspends — which can wake the
  hub thread, and makes `xhci_pending_portevent` true on every resume. The narrow rule is all the
  guest needs, because `bus_resume` explicitly pre-clears `PLC` before writing `U0`
  (`xhci-hub.c:1929-1932`). The `PLC` latch itself must happen **synchronously in `write_portsc` on
  the vcpu thread** — the guest handshake polls PORTSC for only 10 ms, so a worker-deferred latch
  could lose the race; only the *event* goes through `work.port_events`.
- **A**: fixed by Phase 2's restore (the captured `usbsts` has `CNR` clear); the restore path
  additionally force-clears `CNR` so a future capture that somehow holds it can't strand the
  guest for 10 s. Leaving `CNR` set at cold power-on stays as-is — it is spec-faithful and
  `xhci_reset`'s handshake wants the transition.
- **CSS/CRS**: add both to the self-clearing set (`CMD_STORE_MASK &= !(CMD_CSS | CMD_CRS)`), as
  `HCRST`/`LHCRST` already are.

### Phase 2 — carry the controller state in the snapshot

1. `device.rs`: `pub struct XhciState` (registers + `Option<(u64, bool)>` cmd walker +
   `Option<EventRingState>` + `Vec<Option<SlotState>>` + `next_address` + work queue + per-port
   `Option<(u16, u16)>` identity), with `XhciDevice::save_state()` / `restore_state()`.
   `restore_state` rebuilds the `RingWalker`s and `EventRing` from the carried scalars, force-clears
   `CNR`, and does the port-identity reconciliation of §3.
2. `snapshot.rs`: `VERSION 6 → 7`; `SnapshotHead.usb: Option<XhciState>`; presence-byte-prefixed
   section after the v5 GPU section (same shape as `gpio`), encode/decode + the existing
   fail-closed stance. Round-trip unit test alongside `snapshot_file_round_trips`, plus an
   empty-section test (the `usb: None` common case must not desync the stream).
3. `device_manager/hvf/mmio.rs`: keep the `Arc<Mutex<XhciDevice>>` on the manager (mirroring
   `gpio`) so save/restore can reach it. **Required** — today only the bus holds an `Arc`
   (`mmio.rs:339-341`) and `save_snapshot` has no handle at all.
4. `lib.rs::save_snapshot`: `let usb = self.mmio_device_manager.xhci.as_ref().map(|x|
   x.lock().unwrap().save_state());` next to `gpio`, and log a one-line summary (populated ports,
   live slots, endpoints).
5. `builder.rs`: `if let Some(usb) = &snap.head.usb { vmm.restore_xhci_state(usb); }` in the same
   pre-`gate.open()` block as `restore_gpio_state`, and `Vmm::restore_xhci_state` beside
   `restore_gpio_state`. Ordering, all verified: the xhci worker thread is spawned at registration
   (`mmio.rs:337`) long before this; guest RAM is applied at `builder.rs:1222-1236`; the vCPUs are
   created and parked on the `RestoreGate`. Two things to pin: it must run **after**
   `hvf::restore_gic_state` (`builder.rs:1285`) — which placing it beside `restore_gpio_state`
   achieves, but say so, since a worker-posted event pulses an SPI into whatever GIC state exists —
   and the reconciliation port events are **queued without kicking the worker**: the guest's own
   resume register writes kick it, and posting events while the restored controller is still halted
   (`RS == 0`) is a spec deviation available for free.

### Phase 3 — guards that would have caught this

- **`inplace_s2idle`**: extend the existing test (it already has a clean seam — in-guest suspend +
  `SIGWINCH` wake, no host sleep) with `--usb --fingerprint` + `LIMINA_FP_TEST_APPROVE=1` and a
  *sharp* survival oracle: the device's `/sys/bus/usb/devices/*/devnum` and the count of
  `"new full-speed USB device number"` lines in `dmesg` must be **unchanged** across the cycle.
  `lsusb` alone is not an oracle — a device that silently re-enumerates still shows up (bug D).
  Expected RED today.
- **`managed_vm_suspends_and_resumes`** (vmdef.rs): the same before/after assertion around the
  snapshot suspend/restore, plus "no `xhci_hcd` `HC died` / `command ring` error in dmesg". This
  is the primary RED→GREEN for Phase 2.
- **Both guards additionally need a post-resume DATA-PATH exchange**, because identity survival
  (`devnum`) says nothing about whether transfers still flow (§3, held/queued loss). Cheapest
  dependency-free option: a small `USBDEVFS_CONTROL` probe (python3, present on Fedora) that issues
  a real `GET_DESCRIPTOR(device)` against `/dev/bus/usb/001/00N` and checks the returned
  `idVendor:idProduct` — one live Setup/Data/Status TD through the restored controller, event ring,
  doorbell and slot state. Kept in the repo as a helper next to the test, not inlined ad hoc.
- **probe race** (`guest/limina-init/src/main.rs`, `run_xhci_probe`): scan the full window and emit
  each newly-seen `VID:PID` once (dedupe set) instead of `break`ing on the first device, so
  `l1_xhci_fingerprint_reader` stops depending on which gadget enumerates first.

### Phase 4 — ship

Re-apply `default-on.patch`, full `cargo xtask test`, fable review of the implementation, re-export
the libkrun series, fold this file + the corrected diagnosis into the shipped docs, update the
`limina-usb-xhci` memory, then commit default-on and rebuild the dogfood app. File the `--no-fido`
parity gap separately (do not couple).

## 5. Risks / open questions

- **PLC on every LWS write** could in principle perturb cold-boot enumeration. Reviewed: nothing
  on the enumeration path writes PLS via LWS (connect sets PLS internally, reset goes through
  `PORTSC.PR`), so the new event only fires in bus suspend/resume. Guarded by the existing L1
  enumeration tests.
- **`XDEV_RESUME` (15) is a real link state we now store.** The guest writes U3 → RESUME → U0 and
  polls only `PLC`, never the PLS value, so storing RESUME verbatim is fine. We do *not* need to
  auto-transition RESUME → U0.
- **A restored `work` queue could replay a doorbell.** Harmless: the command/EP walkers are
  restored to the same positions, and a spurious pass finds an empty ring. `run_started` is safe
  once **D** lands.
- **Snapshot version bump breaks old snapshots** — by design (fail-closed, no cross-version
  migration), and one start more painful than it sounds: `take_pending_resume` has already renamed
  the file to `.consumed` and passed `--restore` by the time `snapshot::read` rejects the version,
  so **that start fails outright**; the *next* start finds nothing at the canonical path and cold
  boots. Same as every previous bump.
- **Not covered:** USB hot-plug across a suspend (the device set changing *while* suspended) beyond
  the identity reconciliation above; and a gadget-held transfer surviving the teardown (declared
  out of scope, logged when it happens).
