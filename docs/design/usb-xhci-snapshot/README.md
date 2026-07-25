# xHCI USB state across VM snapshot-suspend/resume (M14 follow-up)

**Status:** OPEN — root-caused, decided, NOT YET IMPLEMENTED. Picking this up in a fresh
session. This directory is the handoff: the diagnosis, the trace evidence, the design decision,
and the concrete fix plan. Author date: 2026-07-25.

## TL;DR

Making the USB controller + FIDO + fingerprint reader **on by default** (like snd/battery)
exposed a **real, deterministic bug**: a managed VM (or any snapshot-suspending VM) with USB
attached **wedges on snapshot-resume** — the guest's USB stack dies and the guest can't quiesce
for the next suspend. The decided fix (user's call) is to **serialize the xHCI controller's
host-side state into the VM snapshot** so the fresh restore-worker rebuilds it and USB devices
survive suspend/resume transparently (no re-enumeration).

The default-on change itself is written, unit-tested, and validated in isolation; it is
**parked** (uncommitted working tree + `default-on.patch` here) because it must not ship until
this fix lands — otherwise every default-on managed VM breaks on suspend/resume.

## 1. What was done (the default-on change — parked, not shipped)

Mirroring `snd`/`battery`: `[hardware] usb` and `fingerprint` now default **true**; flat CLI
gained `--no-usb`/`--no-fingerprint` opt-outs (kept `--usb`/`--fingerprint`, `overrides_with`
last-wins, like `swap_cmd_opt`); `run_vm` treats USB as the master switch
(`fingerprint = usb && cli.fingerprint_enabled()`); `cli_from_definition` sets both pos+neg
fields per hardware bool. Wiring: `crates/limina/src/vmlib/schema.rs` (`default_true` + Default
impl), `crates/limina/src/main.rs` (Cli fields `no_usb`/`no_fingerprint`, `usb_enabled()` /
`fingerprint_enabled()` helpers, run_vm consumers, 2 new unit tests). 148 unit tests green.

Answering the original dogfood question ("`lsusb` empty in the guest, do we need control-center
changes?"): **no code change was ever needed** — the control center just runs `limina start
<bundle>` → `cli_from_definition` reads `cfg.hardware`. The empty `lsusb` was purely the old
default-off. (A **GUI toggle** for usb/fingerprint, like snd/mic/battery which are vm.toml-only
today, is a deferred nicety.)

**The change is captured in `default-on.patch` (in this dir) and also lives in the working tree
uncommitted.** Re-apply with `git apply docs/design/usb-xhci-snapshot/default-on.patch` if the
tree gets cleaned. Do NOT `cargo xtask app` for dogfood from a tree with default-on active until
the fix below lands — managed-VM suspend/resume will break.

The `limina → Limina` app-support dir rename is a SEPARATE, already-committed change (`1078fe1`),
unaffected by any of this.

## 2. The failures (full HVF suite after default-on)

4 tests failed. Two root causes:

| Test | Verdict | Cause |
|---|---|---|
| `l1_xhci_fingerprint_reader` (usb.rs:196) | **test artifact** | probe race (see §6) |
| `stock_guest_survives_inplace_s2idle_with_correct_clock` | **FLAKY** | passed on isolated re-run; load-induced timeout, not a hang |
| `seated_gnome_session_survives_snapshot_restore` (venus_session_preserved.rs:504) | **REAL, deterministic** | the xHCI-snapshot bug |
| `managed_vm_suspends_and_resumes` (vmdef.rs:464) | **REAL, deterministic** | same bug (its `limina start` child has `stderr(Stdio::null())`, so worker logs are invisible, but it goes through the managed default-on path → USB attached) |

Determinism confirmed by isolated re-runs: the two snapshot-based tests fail every time (312s /
261s); in-place s2idle passed alone. The in-place path (same worker, no teardown) is at most
flaky and is NOT the target of this fix.

## 3. Root cause (register-trace ground truth)

Instrumented the xHCI register writes (env-free `log::warn!("xhci-pm: …")` in `handle_usbcmd`
and the CRCR_LO / ERSTBA_LO write arms — see §7 to re-add) and ran the venus snapshot test at
default log level. The sequence:

- **Cold boot:** guest HCRST → programs `CRCR=0x84004000` + `ERSTBA=0x84008000` → `USBCMD.RS=1`.
- **Gen-1 suspend:** guest clears RS (stop), re-writes CRCR (same value), then issues
  **`USBCMD.CSS` (Controller Save State, bit 8)** — the guest genuinely uses the xHCI
  save/restore feature and expects the controller to preserve its state. Then it quiesces;
  limina snapshots and **tears the worker down**.
- **Gen-1 restore (FRESH worker, `usbsts=0x801` = CNR|HCH):** the guest does a **light resume** —
  writes `USBCMD=0x0` then `USBCMD=0x4` (INTE only). It does **NOT** re-program CRCR/ERSTBA/DCBAAP,
  does **NOT** issue HCRST, does **NOT** issue CRS. It assumes the controller kept everything. But
  the fresh worker's `crcr=0`, so the first command doorbell makes the engine walk guest address 0
  → **`xhci: command ring walk error: BadAccess(0)` looping every ~5s forever** (Linux's 5s command
  watchdog + retry) → `xhci_hc_died` → USB wedged → the guest can't quiesce for the gen-2 suspend →
  the 120s bracket timeout.

**Precise root cause — an impedance mismatch:** the guest sees **s2idle** (state-preserving) and
light-resumes assuming the controller retained its state; limina actually does **hibernate**
(worker torn down, controller reborn blank). Nothing in the guest's light-resume path
re-establishes controller state, so fable's "set SRE on CRS" idea can't fire — **the guest never
issues CRS on this path** (confirmed by the trace).

## 4. Fable review (key corrections to the initial diagnosis)

A fable subagent verified against libkrun source + Linux v6.12 xhci. Corrections it caught:

- The initial "guest polls a status bit we never settle" theory was **wrong**: every xHCI
  handshake polls for a bit to be clear/already-satisfied, so suspend *entry* works (gen-1
  suspend succeeds).
- It reconstructed the resume kill chain (event-ring desync on an ERSTBA rewrite → lost command
  completion → abort → CRCR-abort misparse → BadAccess loop). The live trace then showed the
  guest doesn't even do the register-restore dance on this path — it light-resumes — which is an
  even simpler and more fundamental mismatch than the register-desync chain, but the endpoint
  (BadAccess loop on a blank `crcr`) is the same.
- It flagged a real product-parity gap: `fingerprint` has `--no-fingerprint` but **FIDO has no
  `--no-fido`** (FIDO rides `--usb` + a live Secure Enclave). File separately; don't couple.

## 5. DECISION: serialize xHCI host-side state into the snapshot

User chose **transparent resume** (devices survive suspend/resume, no re-enumeration) over the
alternatives:

- **Rejected — detach-then-reattach** (post USB disconnects before the freeze, re-cold-plug on
  restore → guest re-enumerates). Less work, mirrors real-hardware hibernate, but devices blink
  out/in across resume.
- **Rejected — scope default-on** (managed VMs keep USB off until the fix). Interim only.
- **CHOSEN — serialize the controller state.** Best UX.

### Fix plan (all in libkrun `usb/xhci/` + the snapshot infra)

The snapshot mechanism (M9.3, `third_party/libkrun/src/vmm/src/snapshot.rs`): `SnapshotHead`
already carries per-device state (`DeviceTransportState` for virtio-mmio, `gpio`, `gpu`) restored
onto the fresh worker before the guest resumes; virtio deliberately *derives* what it can from
drained guest RAM (e.g. `next_avail`/`next_used` are NOT carried — they come from the restored
rings). Apply the same philosophy to xHCI: **carry the small host-only register file; derive the
rest from restored guest RAM.**

1. **Capture (at quiesce):** a new snapshot section holding the xHCI **register file** —
   `usbcmd, usbsts, dnctrl, crcr, dcbaap, config, ports[], iman, imod, erstsz, erstba, erdp,
   next_address` (all small; `device.rs` `XhciDevice`). These are host-side registers not present
   in guest RAM, so they must be carried. Plus the **event-ring producer** (`EventRing.enqueue_idx`
   + `pcs`, `trb.rs`) IF the ring isn't guaranteed drained at quiesce — prefer to **drain the event
   ring at quiesce** (guest has consumed all events before it froze) so the producer is derivable
   from `erdp` + cycle, mirroring the virtio drain trick. Decide during implementation which is
   simpler/robust.

2. **Restore (fresh worker, before guest resumes):** write the captured registers back into the
   freshly-built `XhciDevice`, then **rebuild the derived state from restored guest RAM**:
   - `cmd_ring` (RingWalker) from `crcr` (+ RCS).
   - `event_ring` (EventRing) from `erstba` (+ restored/derived producer).
   - `slots` (`Vec<Option<SlotCtx>>`): walk the **DCBAA** (`dcbaap` → guest RAM), and for each
     configured slot read its device context + endpoint contexts (`context.rs`: `ep_tr_dequeue`,
     `ep_state`, `set_slot_address`, `slot_context_addr`) to rebuild `SlotCtx` and the per-endpoint
     ring walkers. Generation counters reset to 0 (fine — no in-flight completions survive a
     teardown). Re-associate each slot's `Arc<dyn UsbDeviceModel>` with the **re-cold-plugged**
     `port_models` by port index (the fresh worker re-cold-plugs fido→port1, moc→port2 at startup;
     match slot→port→model).
   - `CNR` clear (the restored `usbsts` should reflect ready) so a guest that *does* poll CNR
     doesn't stall 10s.

3. **Restore hook:** parallel to how `gpio`/virtio state is threaded through
   `builder.rs`/`device_manager/hvf/mmio.rs` (`register_mmio_usb`) into the device after
   creation, before vCPUs go live. Follow the existing GPIO restore path as the template.

4. **Defense-in-depth (do regardless, small + upstreamable):**
   - CRCR write with **CS (bit1) / CA (bit2)** set must NOT rebase the ring (the pointer field is 0
     in such a write) — today it zeros `cmd_ring` → walk address 0 → brick. Treat as stop/abort.
   - `process_command_ring` guard: if `crcr_ptr()==0` and no live `cmd_ring`, skip the walk (never
     walk address 0). Turns any future hiccup from "bricked controller" into a no-op.
   - Idempotent `scan_ports_on_run`: skip ports already `CCS` (no spurious re-latch/re-enum churn).

### RED-first oracle (suspend-free, fast — write this first)

An L0 unit test in `device.rs`'s existing test module: program a command ring, then write the
abort value `0x4` to `CRCR_LO` (what Linux's `xhci_abort_cmd_ring` does), ring DB0 → today the
engine walks address 0 (`BadAccess(0)`); after the CRCR-abort fix it must not. This captures the
brick half without a boot. The full transparent-restore behavior is validated by the two HVF
tests going RED→GREEN (`seated_gnome_session_survives_snapshot_restore`,
`managed_vm_suspends_and_resumes`).

## 6. Also fix: the L1 probe race (independent, small)

`l1_xhci_fingerprint_reader` fails only because default-on now also attaches the **FIDO** gadget
(`fido_store` is `Some` whenever a real Secure Enclave exists — this dev Mac has one — regardless
of the fingerprint knob), so the guest enumerates **two** USB devices. The probe
(`guest/limina-init/src/main.rs`, `run_xhci_probe`) polls up to 2s but **`break`s on the first
device seen**, so FIDO (port 1) wins and `04f3:0c7d` (port 2, +120ms) misses. Fix the probe:
scan the full window, emit each newly-seen VID:PID once (dedupe set), stop only after ~1s with no
new device (or run the full window). ~10 lines. Same latent race now affects
`l1_xhci_mock_device`/`l1_xhci_hid_echo` on a SEP host — the probe fix covers all.

## 7. Reproduce / instrument (for the next session)

- **Run a single HVF test:** `scripts/test-boot.sh` forwards trailing args as the cargo-test
  name filter, e.g. `scripts/test-boot.sh debug seated_gnome_session_survives_snapshot_restore`.
  Needs `dangerouslyDisableSandbox`. **Do NOT pass `RUST_LOG=limina_vmm=debug`** for the PM trace
  — it suppresses the `krun_devices` warnings; use the default level (unset) so `krun_devices`
  warns (BadAccess + the `xhci-pm:` trace) show. Note: `managed_vm_suspends_and_resumes` nulls its
  child's stderr, so use `venus_session_preserved` as the log-capturing vehicle.
- **Re-add the PM trace** (temporary, reverted from the tree): in
  `third_party/libkrun/src/devices/src/usb/xhci/device.rs` add `const CMD_CSS: u32 = 1 << 8;
  const CMD_CRS: u32 = 1 << 9;`, a `log::warn!("xhci-pm: USBCMD …")` at the top of `handle_usbcmd`
  logging RS/HCRST/CSS/CRS/INTE + `self.usbsts`, and `log::warn!` in the `CRCR_LO` and `ERSTBA_LO`
  write arms. (These are gitignored vendored sources; the change won't show in `git status`.)
- The captured trace from 2026-07-25 is in §3.

## 8. Test status snapshot (2026-07-25)

Baseline (USB opt-in, before default-on): full HVF suite green. With default-on: 4 red
(2 real snapshot-suspend, 1 flaky s2idle, 1 probe race). Unit tests: 148 green with default-on.

## 9. Validation plan for the fix

1. RED-first L0 CRCR-abort test (device.rs) → implement CRCR/guard fixes → green.
2. Implement the serialize/restore path → the two snapshot HVF tests RED→GREEN with debug trace
   confirming the restored controller resumes without BadAccess.
3. Fix the probe race → `l1_xhci_fingerprint_reader` green.
4. Re-apply `default-on.patch`; run the **full** `cargo xtask test` suite green.
5. fable review of the implementation (per the established workflow).
6. Re-export the libkrun patch series (`patches/libkrun/`, branch `limina/usb-xhci`) — see
   `patches/libkrun/README.md`. Refresh docs (this dir → shipped feature docs) + memory.
7. Only then commit default-on as shippable + rebuild the dogfood app.

## Related

`docs/design/usb-xhci.md` (controller), `docs/design/usb-moc-fingerprint.md` (fingerprint),
`docs/fido-authenticator.md`, `docs/design/host-sleep-s2idle.md` + M9 suspend design (the snapshot
bracket). Memory: `limina-usb-xhci`, `limina-m9-suspend-resume`, `limina-host-sleep-s2idle`.
