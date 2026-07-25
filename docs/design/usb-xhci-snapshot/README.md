# xHCI USB across suspend/resume

**Status:** SHIPPED 2026-07-25. This directory is the design + diagnosis record; `PLAN.md` is the
implementation plan it was built from (kept for its Linux-source citations and the phase/test
mapping), and `default-on.patch` is the default-on change this work unblocked (now applied).

## TL;DR

Making the USB controller + FIDO + fingerprint reader **on by default** exposed a real,
deterministic bug: a managed VM with USB attached **wedged on snapshot-resume** — the guest's USB
stack died and it could no longer quiesce for the next suspend. Root cause: a snapshot-suspend
tears the VMM worker down, so the emulated controller is reborn blank, while the guest suspended
through xHCI's *own* save/restore feature and light-resumes expecting its state intact.

The fix carries the controller's host-side state in the snapshot, plus eight register-semantics
corrections found by reading Linux's PM paths against our implementation. Both are in the libkrun
fork (`patches/libkrun/0102`–`0105`).

## 1. What the guest actually does (Linux v7.1, verified against source)

`drivers/usb/host/xhci.c` and `xhci-hub.c` — read, not recalled; every claim below was re-checked
against upstream source, and one round of this work shipped a fix for a path that a *remembered*
bitmask said existed and the real macro said did not (see §3, "Reviewed and deliberately not
changed"). **Suspend** — `xhci_bus_suspend` issues Stop Endpoint
on every live endpoint and parks each connected port at `PLS = U3`; `xhci_suspend` then clears
`USBCMD.RS`, calls `xhci_clear_command_ring` (which rewrites `CRCR` at the segment base), saves
USBCMD/DNCTRL/DCBAAP/CONFIG + the interrupter's ERSTSZ/ERSTBA/ERDP/IMAN/IMOD, and sets
`USBCMD.CSS`.

**Resume** — `xhci_resume` with `power_lost == false` (our case; nothing tells the guest otherwise):

1. handshakes `USBSTS.CNR → 0` **with a 10-second timeout**, and on expiry logs
   `"Controller not ready at resume"` and **returns an error — the USB stack is dead from here on**;
2. `xhci_restore_registers` writes everything back; `xhci_set_cmd_ring_deq` rewrites `CRCR`;
3. sets `USBCMD.CRS`, handshakes `USBSTS.RSS → 0`, then checks `SRE`/`HCE` — **either one set turns
   the resume into a full reset + re-enumeration of everything**;
4. sets `USBCMD.RS`.

Then `xhci_bus_resume`, for each port it suspended: only if it still reads `PLS == U3` does it
drive `U3 → RESUME → U0`, and it then **polls `PORTSC.PLC` for 10 ms** — on timeout it logs
`"port N-M resume PLC timeout"` and skips `xhci_ring_device`, the doorbell ring that restarts every
endpoint the bus suspend stopped.

## 2. The bugs

| # | Bug | Effect | Path |
|---|---|---|---|
| **A** | `XhciDevice::new()` sets `USBSTS.CNR` and only `HCRST` clears it, so a restore worker's controller is born `CNR = 1`. | `xhci_resume` step 1 spins **10 s** and fails. The USB stack is dead before a single register is restored. | snapshot |
| **B** | A `CRCR` write stored the value and rebased the walker unconditionally. Linux's command watchdog writes `CRCR = readl(CRCR) \| CMD_RING_ABORT`, and `CRCR` reads back as 0 per spec — so the value written is `0x4`, pointer bits zero. | `crcr_ptr() == 0` → the next doorbell walks guest address 0 → `command ring walk error: BadAccess(0)` every 5 s forever. A brick reachable from **any** command timeout. | both |
| **B2** | Nothing acknowledged the abort. | `xhci_handle_stopped_cmd_ring` never runs, `cmd_ring_state` stays `ABORTED`, every later command is refused. Fixing only B silences the log spam and leaves USB just as dead. | both |
| **C** | An `ERSTBA` write always dropped the event ring, rebuilding the producer at index 0 / PCS 1. `xhci_restore_registers` rewrites ERSTBA with the **same** value on every resume, while the guest's consumer (which it does not reset on this path) sits mid-ring. | Producer/consumer desync: events land in already-consumed slots and stay invisible until the producer wraps around. Lost completions → command timeouts → (B). | both |
| **D** | `scan_ports_on_run` re-latched `CCS \| CSC` and forced `PLS = Polling` on every populated port at each `USBCMD.RS` 0→1 edge — including `xhci_resume`'s. | `xhci_bus_resume` reads a port no longer in U3, drops it from `bus_suspended`, and never drives the link resume or reaches `xhci_ring_device`. | both |
| **E** | `write_portsc` honoured LWS but never latched `PLC`. | `bus_resume`'s 10 ms handshake always timed out → same skipped `xhci_ring_device`, plus a stall per port per resume. | both |
| **F** | `USBCMD.CSS`/`CRS` were stored as ordinary RW bits. | Sticky strobes ride back into the guest's `USBCMD` reads on resume. | both |
| **G** | A command doorbell queued before an abort was still executed in the same worker pass, after the Stopped event. | `xhci_handle_stopped_cmd_ring` rewrites every aborted command to a no-op and *then* re-rings the doorbell — so we would run commands the guest had already cancelled. Narrow race; one line. | both |

**Honest scoping of D and E.** They are equally wrong on the in-place s2idle path, but **Linux
survives them there**: the hub thread finds the port still enabled and resuscitates the device with
no re-enumeration, nothing in dmesg and the same `devnum`, and the class drivers re-submit their
URBs. Measured, not assumed — see §4. So on that path they are a correctness and fragility floor
(a mishandled port resume, a 10 ms stall, and breakage waiting for any driver that holds a URB
across suspend), not a reproduction of a user-visible failure. The user-visible failure is **A**,
on the snapshot path, which these same semantics feed.

`SRE`/`HCE` are never set by our controller, which matters: setting either is the switch that turns
a resume into a full re-enumeration.

## 3. The fix

### Design invariant

> **After restore, the fresh controller must be indistinguishable from the in-place one.**

That reduces the snapshot work to a mechanical question — which fields of `XhciDevice` are neither
reconstructible from guest RAM nor re-established by the fresh worker? — and makes the in-place
path the reference implementation. `devices::usb_state::XhciState` carries: the register file, the
command-ring walker, the event-ring producer (all four of base/size/index/PCS), every slot (with
`config_value`, which exists nowhere in guest RAM) and its EP0 + data-endpoint ring positions,
`next_address`, and any undrained worker work. Not carried, because the fresh worker re-establishes
them: `intc`, `irq_line`, `interrupt_evt`, `worker_kick`, `port_models`.

Deliberately **not** re-derived from the DCBAA the way virtio transport state is derived from its
rings: the carried bytes are host-authoritative, whereas a DCBAA walk is a parse of hostile guest
RAM whose failure modes are all silent (a bad pointer yields a plausible-looking empty slot).

### Port identity

The restore worker cold-plugs gadgets in a fixed order, but each `build()` can fail independently,
which would **shift** the rest onto other ports and bind a slot to the wrong device. So each
populated port's `(idVendor, idProduct)` rides along and the restore compares. On a mismatch the
port is presented as a real unplug (`PORTSC_DEFAULT | CSC` + a queued port-change event) and its
slots are dropped — the guest cleanly disconnects what went away, and because `CCS` stays clear,
fix **D**'s scan enumerates whatever is there now.

### What is deliberately lost

Gadget-held transfers and queued host→guest frames live in the worker's gadget objects and die with
them. Sound because `xhci_bus_suspend` stops every live endpoint first (bumping our generation
counter, which drops those completions) and the class drivers kill + resubmit their URBs. Residual,
accepted: a reply already queued at capture is gone, and a URB that survived suspend un-killed has
no TD left on our side (our walker commits past a dispatched TD). Not a wedge; that operation
retries.

Gadget **protocol** state is unaffected — the elanmoc engine + template store and the FIDO
authenticator + passkey store live in the *supervisor*, which is never torn down, and both serve
loops already reconnect to the fresh worker's socket.

### Reviewed and deliberately not changed

- **Latching `PORTSC.PEC` when a `PED` write disables a port** — proposed in review to stop
  `xhci_bus_resume` silently killing the port of a device that was already runtime-suspended at U3,
  then **withdrawn against the source**. `PORT_RWC_BITS` *includes* `PORT_PE` (xhci-hub.c:21-22), so
  the resume write-back **clears** PE rather than asserting it — the path does not exist. Worse, the
  only two places Linux writes `PED = 1` are `xhci_disable_port` and the `SS_DISABLED` link-state
  write, both *deliberate* disables, and the latter sets `PORT_PEC` in the very same word "so that
  we get a new connection event" (xhci-hub.c:1338). Latching a change there would fight the driver:
  the hub thread would see `PED = 0` with `CCS` still set and re-enable a port the guest had just
  disabled. The lesson is the older one — a mask's contents are worth reading, not recalling.
- **The Command Ring Stopped event's dequeue pointer may point at a Link TRB.** Harmless:
  `handle_cmd_completion` (`xhci-ring.c`) tests `COMP_COMMAND_RING_STOPPED` *first* and returns
  before it computes `cmd_dequeue_dma` — the pointer we report is never read on that path.
- **Restoring a snapshot that has USB state into a worker started `--no-usb`.** The restore warns
  and continues, leaving the guest's USB stack pointing at an unbacked MMIO window. Failing the
  resume outright would be worse: it turns a user's config edit into an unresumable VM. Config drift
  degrades one device; it does not lose the machine.

## 4. Evidence

Everything below was RED-verified by breaking the fix and re-running, not by inspection.

- **L0, `device.rs`/`engine.rs`** — the register-semantics tests plus a lived-in-controller
  save/restore round-trip. Every carried field was individually broken and confirmed to fail the
  round-trip (`scripts/xhci-red-check.py`). For that claim to be literal the capture has to *hold*
  a non-default value for each one, so it carries a latched `PLC`, an in-flight `CRCR` stop, and an
  undrained work queue (doorbell + abort + an EP doorbell + a port event) as well as the registers.
- **L2 snapshot path, `managed_vm_suspends_and_resumes`** — the primary oracle. With the carry
  disabled: `USBPROBE: fail control transfer to 04f3:0c7d: [Errno 19] No such device`. With it:
  same `devnum`, live control transfer, no controller errors in the guest's dmesg.
- **L2 in-place path, `usb_device_survives_inplace_s2idle`** — new. Every *guest-side* signal is
  identical with and without fixes D/E (same dmesg, same devnum, working device), so the oracle is
  the **host** PM register trace:

  ```text
  broken: USBCMD <- 0x5 [RS INTE] / PORTSC[1] <- 0x6e1 was 0x206e3   PLS=Polling, CSC latched
                                                                     -> port abandoned
  fixed:  USBCMD <- 0x5 [RS INTE] / PORTSC[1] <- 0x661 was 0x663     PLS=U3 preserved
                                    PORTSC[1] <- 0x107e1 LWS pls=15  XDEV_RESUME
                                    PORTSC[1] <- 0x10601 LWS pls=0   U0
                                    PORTSC[1] <- 0x400601 was 0x400603  PLC latched -> the
                                                                     guest's 10ms handshake won
  ```

- **L1, `l1_xhci_fingerprint_reader`** — the probe race: with two gadgets attached the guest's
  `run_xhci_probe` reported whichever enumerated first and dropped the other. Fixed to scan the
  full window with a dedupe set.

### Reproducing / instrumenting

The PM trace is permanent and unconditional at debug level. Read it with
`RUST_LOG=krun_devices=debug` — **not** `limina_vmm=debug`, which suppresses the `krun_devices`
logger entirely. `scripts/xhci-red-check.py` re-verifies every L0 guard by reverting each fix in
the vendored tree and requiring the corresponding test to fail.

## 5. Also in this work

- `--no-usb` / `--no-fingerprint` and `[hardware] usb`/`fingerprint` defaulting to true
  (`default-on.patch`, now applied). The control center needed no change: it runs `limina start`,
  and `cli_from_definition` reads `cfg.hardware`. A GUI toggle stays deferred.
- Drive-by: `VmResources`' test initializer was missing the `usb` fields, so the vmm crate's unit
  tests did not build with `--features usb` at all.

The `--no-fido` parity gap this work surfaced (FIDO had no opt-out while USB and the fingerprint
reader did) was closed straight after, deliberately as a separate change: `--no-fido` /
`[hardware] fido`, cutting the passkey store so **both** transports go with it. See
`docs/fido-authenticator.md` §"Turning it off".

## Related

`docs/design/usb-xhci.md` (controller), `docs/design/usb-moc-fingerprint.md`,
`docs/fido-authenticator.md`, `docs/design/host-sleep-s2idle.md`, `docs/design/m9-suspend-resume.md`.
Memory: `limina-usb-xhci`, `limina-m9-suspend-resume`, `limina-host-sleep-s2idle`.
