# Hardening backlog

The open loose ends on shipped work, grouped by subsystem. Each entry says what is wrong or
unmeasured now, what is established, and the next step or fix shape. Milestone status and new
features live in `docs/roadmap.md`; the render/present stack in `docs/graphics.md`; windows and
input in `docs/input-and-windows.md`.

An entry leaves this file when it closes. The commit that closes it carries the story; a lesson
that generalises goes into **Rules these items taught** at the end, as a rule rather than a story.
Dates attach to measurements only.

---

## Display & windows

### Variable-refresh screens (ProMotion) get no host vblank grid: VRR support is owed
A guest window on a screen whose refresh rate varies (`NSScreen.minimumRefreshInterval` ≠
`maximumRefreshInterval`) tells the worker its slot has no grid (`vsync id=N off`,
`window/vsync.rs`). The guest keeps its stock timer at the mode's rate, and fenced flips are paced
by the host's release (the copy's blit, ~1 ms). That keeps the guest running. It is not VRR support.

Established on the dev Mac's built-in panel (ProMotion, 8.3–41.7 ms, BenQ 60 Hz attached as main;
`spikes/promotion-link`, measured 2026-10-14):
- An app's display link there runs at ~60 Hz while its `duration` says 8.333 ms. Requesting 120
  (`preferredFrameRateRange`), drawing a new frame every callback, native fullscreen, and the
  screen's link instead of the window's all change nothing. A CVDisplayLink delivered no callbacks.
  Before the variable-rate check, every burst was dropped as off-nominal and the slot simply never
  got a grid.
- The 60 Hz phase wandered by 1.5–4 ms (p50–p90) within a few seconds in two runs of three: there is
  no stable grid to phase-lock a guest vblank to.
- The guest is told the mode is 120 Hz: 3024x1896@120 overflows an EDID detailed timing, and the
  DisplayID block carries the true timing, which the guest runs. Glass got 60.0 presented fps, every
  interval 16.7 ms. The guest's flips on that output completed mostly at release (1333 against 498
  at the tick, on the stock 120 Hz timer). Its copy ring grew to 5 surfaces, against 3 on the 60 Hz
  BenQ.

Not established: why an app gets 60 Hz. The candidates are the external display attached as main,
or a policy for unbundled processes (the probe was a bare binary). Re-run `linkprobe` with the BenQ
unplugged and from inside the signed bundle first; that decides whether to advertise what macOS
will actually drive (60) or to chase 120.

Fix shape, once the rate question is answered:
- Advertise a VRR range to the guest: a DisplayID adaptive-sync range, and `vrr_capable` on the
  connector in the module. Honour `VRR_ENABLED` so a VRR-aware compositor (mutter, synoik) runs
  unthrottled within the range.
- Under VRR the flip completes at release, never waiting on a tick, and the event is stamped with
  the present time rather than a deadline.
- The supervisor commits each frame as it arrives, with the panel following content. Measure that
  with the composite-deadline release oracle (`spikes/composite-deadline`) on the ProMotion panel.

### The captured cursor can go undrawn on the display the user is looking at
Seen once on the dogfood Mac (2026-08-24) in a cold-booted two-display session: the pointer moved and GNOME's
overview fired, but nothing was drawn while captured; the uncaptured pointer wore the guest's shape
throughout, and `Ctrl-Alt-F1`/`F2` fixed it for good. Excluded: the IOSurface path (0
`building guest cursor from IOSurface … failed`), a stale per-slot `cursor.id` across a plane
migration, a transient lookup miss (`update_capture_cursor` re-reads every tick).
What remains is slot disagreement: the captured layer draws the window's own slot and hides when
that slot's `cursor.visible` is false, while the worn shape takes any slot with a plane
(`cursor::shape_slot`). A guest cursor plane on the other CRTC — normal while absfit is still
learning shares on a fresh two-display boot — hides the user's window. `cursor::undrawn_fault`
logs that state once per episode ("the guest has its cursor plane on [..], not on the captured
slot N").

A guest also keeps a neighbouring slot's plane lit a few pixels outside its own scanout while the
pointer sits at the far edge of the next one (a dogfood single-session log, 2026-08-24: `other
slots also showing a cursor: [(1, (-10, 582))]`, 31 times). The echo already discounts such a
plane (`CursorEcho::shows_pointer`, `window/echo.rs`: a lit plane whose pointer position lies off
its scanout is a neighbour's sprite, not the pointer). `cursor::shape_slot` and
`cursor::undrawn_fault` do not: both are fed the raw `cursor.visible` flags (`window/mod.rs`), so
such a plane still counts as a second cursor for the worn shape and still feeds the fault.

Next: give `shape_slot` and `undrawn_fault` the echo's test, so an off-scanout plane counts for
neither; then, when the fault fires, read it against `[CURSOR] slot=N hide` and decide whether the
captured path gets `shape_slot`'s tolerance or the fix belongs upstream in the position.

### The mapping probe places its steps in union space, not per display
`absfit::PROBE_SWEEP` keeps `v` within `0.30..0.70` of the union. On a display covering only part of
the union's height that band can include the display's top edge (slot 1 on the two-panel rig starts
172 logical px down), and at `u = 0.05` a clamped step lands on a top-left corner — GNOME
Activities. The sweep's guard test checks device space, which proves nothing about where a step
lands on a display. The first pass is also blind: with no lines yet the 10 steps are fixed whatever
the union's division, so each slot's sample count is luck (6/4 on the rig). Once one line exists,
place the remaining steps inside each slot deliberately, which also removes the corner hazard.
Latent: ten rig sweeps landed no step near a corner.

### A pointer cannot be drawn for the first ~350 ms of a Space-switch animation (parked)
A three-finger Space switch animates for about 530 ms; `isOnActiveSpace`, key status and
app-active all change at commit, so a captured pointer stays hidden and parked for the whole
animation while macOS draws its cursor throughout (measured 2026-08-22 on six flicks). The only public signal
that leads the commit, `NSWindow.occlusionState` losing `.Visible`, does so 170–201 ms early —
releasing on it would restore the pointer only for the last third. Trackpad gesture events
(`NSEventType` 29) start ~530 ms early but fire for every gesture and miss Ctrl-arrow and Mission
Control. The remaining lever is a private CGS space-change callback, unpriced and probably
commit-timed too. A known limitation, deliberately parked.

### Secondary fullscreen cover animates guest content into place
Fullscreen covers a panel with `toggleFullScreen(None)` on a small centred window; AppKit's zoom
stretches the current surface across the intermediate frames, so the content visibly scales until
the guest re-modesets. Polish: pre-size the window and layer to the panel before the toggle, or
curtain the layer until the transition settles and the guest's mode matches (`window/windows.rs`
restyle and refit).

### A guest reboot drops a fullscreen secondary
During the firmware phase the pool collapses to slot 0 (`DisplayTable::wanted` /
`reset_to_firmware`), so the table dismisses the other slots, and nothing mirrors the console onto
other panels. Keeping every panel fullscreen across a reboot is a mirroring feature, not a lifetime
fix.

### A fullscreen-everywhere held-button drag follows AppKit's mouseDown-window routing
With every panel fullscreen, a drag that starts on one window and crosses to another stays routed
to the window that got the mouseDown. Unverified since the captured-cursor rework — reproduce
before designing a fix.

---

## Input

### Clicks that neither take nor stand down the grab
Reported on dogfood (2026-08-22, again 2026-10-03: mid-screen clicks and trackpad motion in a
focused fullscreen window intermittently not grabbing; the clicks still reach the guest). The
known causes are the next three entries; a sighting that fits none of them needs the facts the
per-click `info` line leaves out. That line (`pointer capture: click at (x,y) — grabbed=…`)
carries fullscreen/key/space/latch, but not `menu_open`, the screen-capture verdict or the window
the hit test found, so a click refused by a stuck `MENU_OPEN` looks like any other
`grabbed=false`. Next sighting: `limina debug <vm> log 'warn,limina::window=info'` and
`limina debug <vm> lever edge-trace on` on the running VM, then read `[CLICK]`/`[HITTEST]`
before forming a theory. Ruled out for the 2026-10-03 report: a system-disabled tap (no
`the system disabled our event tap` line in any dogfood log).

### The at-rest dwell re-grab is unverified on hardware
The tick re-asks the dwell for a resting pointer (`capture_tap::regrab_at_rest`); the policy is
unit-tested, the live behaviour is not. Owed: on a fullscreen guest, release the grab at an edge,
then make one trackpad stroke that ends deep inside and lift the finger at once — the grab must
be taken about a quarter second later with no further motion (`pointer capture: taken — the
pointer came to rest …` at `info`).

### A click on the notch strip: the hit-test fix is unverified on hardware
`guest_is_topmost_at` counts each slot's up strip as a guest window (`input::guest_hit`). Owed, on
a notched panel under `notch = extend`, fullscreen, after an edge release: a click in the band
beside the housing must take the grab (`pointer capture: taken — click on guest content`, and
`[HITTEST] … guest=true` with edge-trace on), not latch it out; with the menu bar revealed, a click
on it must still stand the grab down.

### Needs repro: the released pointer can come back invisible
Seen once on the two-panel rig (2026-08-23), fullscreen on both panels after a click had promoted the grab:
releasing a hard grab with Ctrl-Opt left no cursor drawn; the pointer was live, and pushing it up to
reveal the chrome brought the image back. The per-tick blank-wear check and the unhide fix
(`WearState` in `crates/limina/src/window/input.rs`) are both in, so this is a path that skips both or a macOS unhide that did not take.
Catch it with the poke-VM trace env on.

### Needs repro: the menu-bar reveal and macOS's own menu bar disagree
Two sightings around the reveal, neither reproduced; catch each before changing anything.
- **The reveal drops while a macOS menu is still open.** Observed on the dogfood Mac (2026-08-22):
  a small downward move with a menu open releases the ask and the chrome retracts under the open
  menu. The ask is slaved to `NSMenu::menuBarVisible` (`InputState::menubar_observed`) and released
  by `reveal_step`; which of the two lets go first has not been measured.
- **`notch = extend` may leave the band covering the bar when the reveal triggers ungrabbed.**
  While macOS has the menu bar out, the strip overlay must not cover its band. Uncaptured, with the
  pointer at a panel's top, `InputState::menubar_observed` (run every tick) grants the reveal and
  `reconcile` stands the overlay down. A reveal with the pointer elsewhere (a Space switch, an
  app-initiated reveal) skips the grant by design. Which ungrabbed trigger leaves the band over the
  bar is not established.

### Needs repro: pointer not shown right after logout/login under synoik
Intermittent on dogfood (sighted 2026-08-22) under synoik, not under mutter — a mutter "cannot reproduce" is a false
negative here. Chase it on a clone of `Fedora-Workstation-44.enhanced.synoik.raw`, in a loop: plane
visibility is readable from the `[CURSOR] … visible=` trace, so many cycles can be checked
automatically, and the guest synoik session can be asked what it saw. One clean cycle did not
reproduce it.

### Captured-pointer re-pin fights a remote-desktop client
While captured, the tap re-pins the hidden host cursor to a park point inside the window on every
motion event (`capture_tap.rs`, the NOTE at the re-pin; `window/warp.rs`). When another agent also
moves the macOS cursor — notably a remote-desktop client operating this Mac — the re-pin fights it
and reads as jitter or snapping; an edge-only-warp variant did not clearly help. Established: on
macOS 26 `CGAssociateMouseAndMouseCursorPosition(false)` does not freeze the cursor; the session
CGEventTap never disables under load; with the warp removed the relative deltas are clean. Before
redesigning, research how VNC/RDP servers solve capture (a real cursor-freeze API,
`CGDisplayHideCursor` + associate semantics, an `IOHIDEventSystem` relative tap, or coexisting with
an upstream RD capture). Overlaps the tap-free capture ladder (`docs/input-and-windows.md` §5b),
which hinges on the same disassociate-freeze question.

### The captured range does not follow a pointer the guest moves on its own
While captured, `follow_guest_echo` re-bases the fit estimate (`capture_pos`) onto the guest's
cursor echo, but the position we send (`capture_range`) keeps its running value. When the guest
moves its pointer itself — mutter putting a pointer that is on no monitor in the middle of the
primary, or an app warping it — the two disagree (about 1000 px on the dogfood, 2026-10-19) and
the next absolute send yanks the guest's pointer back to the stale value. Our own sends no
longer leave the desktop (`fit::held_range_step`, `Desktop::wire`), which removed the cause seen
there. A general echo→range re-base has to wait out echo lag the way the fit's `EchoGate` does,
or it drags the pointer back a step behind every crossing; design that before building it.

### Per-key aux-key settings, and the Accessibility cliff in the UI
The aux-key buckets in `crates/limina-input/src/auxkey.rs` (`Media`/`Volume` hard-grab only,
`Brightness`/`Other` host-only) are meant to become per-key runtime settings: shape the config as
`nx_key -> Option<GrabMode>` with buckets as defaults. The settings UI must show these toggles
disabled, with a "requires Accessibility" note, when the tap is not installed (`TAP_PORT` null):
aux keys reach only a CGEventTap, never a local NSEvent monitor, so without the grant they are inert
while ordinary keys work, which reads as a limina bug — and the UI is the only place that can say so.
fn+F3–F6 (Mission Control, Spotlight, Dictation, Do Not Disturb) are ordinary keyDowns
(0xA0/0xB1/0xB0/0xB2, Globe 0xB3), not aux keys: promoting one is a `keymap.rs` entry, and
`macos_special_action_keycodes_have_no_guest_mapping` fails at that moment so the routing gets
decided deliberately (`spikes/fn-key-probe/RESULTS.md`).

### CapsLock/NumLock LED parity
The guest's LED state never reaches the host: libkrun's virtio-input status queue is a no-op
(`devices/src/virtio/input/worker.rs`, "would be used for things like setting LEDs"). Surface it to
the supervisor and mirror it on the host keyboard (roadmap M8).

---

## Guest sessions

### A fronted session with no helper leaves the host on the previous session's arrangement
The virtio-gpu wire does not change across a seat switch (same scanouts, modes and resources, no
event), so the host learns which session is on screen only from `limina-agent-session`. The helper
(0.1.4) reports only while its uid holds seat0 (`ACTIVE_UID=` in logind's seat file,
`guest/limina-agent-session/src/layout_gate.rs`), and re-sends after any seat change it sees
between two polls, so neither a seatless session of the same user nor a session that fronts only
briefly leaves a foreign report behind. What remains:
- **A fronted session with no helper.** `limina-agent-session.service` is
  `WantedBy=graphical-session.target`, which a compositor started with
  `systemd-run --uid=… synoik --session` never reaches. Nothing then claims the report and the host keeps the last one it got.
- **A held report outranks everything.** `absfit::mapping` returns the report whenever one is held
  and consults the fitted lines only without one, and nothing demotes a report the cursor echo
  keeps refuting; `desktop_in_range` and `range_shares` also build the captured confinement and
  seam shares from it.
- **The echo-mismatch warning does not name the cause**: it says where the cursor went, not that
  the send was placed through a report (`window/echo.rs`).

Measured 2026-08-24 on the dogfood guest (helpers gated on the user's session state): sends landed 2452 px off, the echo warned `we sent the pointer to slot 0 …
the guest shows its cursor on [(1, …)] and none on slot 0`, and the captured pointer was clamped
inside the other session's desktop for exactly the windows where the other session owned the
screen.

Next: demote a report the echo keeps refuting, host-side (absfit's `CONTRADICTIONS` pattern), so
it covers the stock tier and a helper-less session alike; name the tier behind each send (report,
fit or identity) in the echo-mismatch warning; on the enhanced tier, have root `limina-agent`
report logind's seat changes, which covers a session with no helper.

### A `SUBMIT3D` error storm across a guest DRM-master handoff
Measured 2026-08-24 on the dogfood Mac with the C renderer: about 20 s after a guest `chvt` between two seat
sessions, `ctx 25 submit_command -> Err("ErrRutabaga(ComponentError(22))")` repeated at ~4 KiB per
command for the length of the handoff — consistent with a compositor that keeps submitting after
losing DRM master. Owed on virglrs: reproduce with the multi-session steps above; trace the EINVAL to
its emission site before reasoning from the message; check what the worker does with a context whose
submits fail in a sustained run (log volume, poison, recovery).

---

## Lifecycle & supervisor

### Take the control-plane socket off its `$TMPDIR` path
The worker's own listeners (balloon, display control, the FIDO and fingerprint gadgets) have no
path: each spawn hands the worker a link socketpair and the supervisor connects by passing it one
end of a fresh stream (`crates/limina-launch/src/connect.rs`; `l1_no_socket_paths` pins it). One
supervisor-owned socket is left at a predictable path, `$TMPDIR/limina-ctrl-<pid>.sock`
(`srwxr-xr-x`), where any same-user process can pose as the guest agent. It stays because libkrun's
vsock proxy **connects to a path** for every guest connection (`--vsock-socket`), so the fix is a
libkrun change: let a vsock port take a connector (an inherited link fd) instead of a path, with the
same accept acknowledgement (`spikes/scm-rights-inflight/`). A bootstrap name would not help: a name
in `gui/<uid>` is as reachable as a path. `control::cleanup` removes the socket at every
`process::exit`; a SIGKILLed supervisor leaves a stray one that the next bind unlinks. **Do not add a
startup sweep that reaps sockets whose embedded pid is dead** — pids are recycled.

### Movable VM library and per-VM placement
Design: `docs/design/vm-definitions.md` §8. The library can be moved by hand: `[library] path` in
`~/Library/Application Support/Limina/config.toml` (env > config > default, re-read per call), and
creating a VM refuses when the library's `/Volumes/<name>` is not mounted. Owed: (2) a "Change VM
Library Location…" picker that repoints without migrating; (3) per-VM placement via
symlink-as-registration, showing dangling links greyed out as "volume not mounted", and the center's
banner for an unmounted library in place of the empty-library state.

### UEFI variable writes are crash-safe except reclaim, and only as far as writeback is ordered
The variable store is a file libkrun maps `MAP_SHARED` into the guest; Cardea keeps it in edk2's
emulated-NV format (`third_party/cardea/crates/varstore`). An update is crash-safe in edk2's order
(the old record marked in deleted transition, the new one written data first, then header, then
`VAR_ADDED`, then the old one deleted), each store write fenced (`poke`), and a crash sweep cuts
create, replace, append and delete at every write and checks what survives (`crash_sweep` in
`crates/varstore/src/tests.rs`). Two gaps remain:
- **Reclaim is not crash-safe**, nor is the full-store fallback that reuses the old record's space
  (`reclaim_in_place` and the tail of `write` in `crates/varstore/src/lib.rs`; edk2's emulated-NV
  reclaim is not either). Either runs when a write does not fit at the end of the store.
- **The order holds in the page cache, not on disk.** The fences order the firmware's stores into
  the mapping, which is all a worker crash can see. A host power loss sees what writeback reached
  the file, page by page, in no promised order, so an update spanning pages can persist its later
  step without its earlier one.

The cost today is a shim fallback boot; once Secure Boot keeps enrolled keys there it is the keys.
Settle both before Secure Boot ships.

### A store Cardea cannot parse leaves the VM on RAM variables for good
When the VMM's window holds a store that no longer parses, Cardea leaves it untouched and runs
that boot on a RAM store (`open_window` in `third_party/cardea/firmware/src/vars.rs`: wiping it
would turn one bad byte into a lost BootOrder). Nothing ever repairs it, so every later boot is
RAM-only too, with a shim fallback boot each time (with a TPM, fallback's reset makes that a loop:
`docs/design/efi-vars.md`), and the only trace is a line on the serial console. There is no lever
to reset the store: the Configure sheet resets the TPM (`crates/limina/src/center/controller.rs`,
`reset_tpm`) and nothing touches `efi.vars`. Owed: say so where the user will see it, and a reset
beside the TPM's (move the file aside; the next boot formats a fresh store).

### Inventory the firmware features the guest probes and gets `NOT_SUPPORTED`
Each declined feature is guest behaviour inherited by default instead of chosen. PSCI 1.0 +
`SYSTEM_SUSPEND` are offered (libkrun `hvf/src/lib.rs`, gated by `LIMINA_PSCI_SYSTEM_SUSPEND`).
Owed: the rest of PSCI 1.x (`SYSTEM_RESET2`, `CPU_FREEZE`, `CPU_SUSPEND` idle states, the full
`PSCI_FEATURES` answer set), SMCCC/`ARCH_FEATURES`, and anything else a guest probes — decide
offer or decline for each and record it. (The ACPI machine already carries a PPTT: one package of
every vCPU, libkrun `devices/src/acpi.rs`, `build_pptt`.)

### Cardea preserves x18 with entry shims instead of never using it
x18 is the AArch64 platform register: Linux keeps its shadow call stack there and checks that every
EFI runtime call returns it unchanged (`[Firmware Bug]: register x18 corrupted by EFI …`). Rust on
`aarch64-unknown-none` allocates x18 like any other register, and so does the precompiled `core`.
Cardea's runtime services table therefore points at naked entries that save x18 around each
service (`firmware/src/runtime.rs`, `mod entry`); boot services carry no such guard, which is fine
while no boot-time caller relies on x18. Reserving the register outright needs the nightly-only
`-Zfixed-x18` (`-Ctarget-feature=+reserve-x18` is deprecated in its favour) plus a `core` rebuilt
with it (`-Zbuild-std`, also unstable). Fix shape, once both are stable: build with x18 reserved,
check the disassembly has no x18 use, and drop the entry shims. The hostile loader's
`runtime_services_keep_x18` case stays as the witness.

### Show the serial line in the window, and read it from the CLI
Debugging a guest that does not come up (no agent, no network, sometimes no picture) needs what it
printed, read by a person and by a tool, without a guest change; the agent cannot stand in, being
unreachable in exactly those cases. The guest half exists: on the ACPI machine
`serial_console = "both"` has the firmware add `console=` arguments, giving the serial line the
kernel's console and a getty while the display keeps its own and the splash; the device-tree
machine always has both. Owed is the host half: record every managed VM's serial line rather than
only a `--console` run's, read it programmatically (the debug port or the CLI), and let the window
switch between the display and a terminal view of the serial line, as QEMU's monitor views do.

---

## Storage

### A full host disk reaches the guest as an I/O error, and its file system goes read-only
The virtio-blk worker answers a failed host `flush`/`sync` with `RequestError::FlushingToDisk`
(libkrun `devices/src/virtio/block/worker.rs`, the `VIRTIO_BLK_T_FLUSH` arm), and every request
error reaches the guest as `VIRTIO_BLK_S_IOERR`; nothing in the block device or the worker treats
ENOSPC differently or pauses the VM. So ENOSPC on the host volume, which a large build on the same
disk can cause, aborts the guest's btrfs transaction and forces it read-only until a reboot;
writes since the last commit are lost. Seen on a dev guest: `FlushingToDisk(… "No space left on
device")` in the worker log, `forced readonly` in the guest. Fix shape: pause the VM on ENOSPC and
resume once space returns (QEMU's `werror=enospc` default), and say so in the window, rather than
failing the request.

---

## Suspend/restore

### The worker's quiesce budget plus a slow save can outrun the supervisor's bracket timeout
The worker waits up to 45 s for the guest to quiesce (`QUIESCE_TIMEOUT`,
`crates/limina-vmm/src/krun/mod.rs`) and then writes the snapshot; the supervisor abandons the
bracket at 60 s (`SUSPEND_BRACKET_TIMEOUT`, `supervisor.rs`). A guest that quiesces late and a
large RAM dump can together pass 60 s, so the supervisor reports the suspend abandoned while the
worker completes it. Fix: have the supervisor's bound start at the worker's quiesce verdict (or
extend while the worker reports a save in progress) rather than at the request.

### The supervisor abandons the suspend bracket while the worker may still be saving
When the bracket times out the supervisor clears `SUSPEND` and carries on as if the VM kept
running (`supervisor.rs`, after `SUSPEND_BRACKET_TIMEOUT`), but the worker may still be writing the
snapshot (previous entry). The vCPU policy runs throughout: the bracket re-onlines every vCPU first
(`restore_all_vcpus`), because the online set is not in the snapshot and an offline vCPU restores
as online (the comment at the bracket, task #41), yet nothing stops `on_pressure` or the QGA tick
from answering the next report with a shrink before the worker snapshots, or while it is still
saving after the timeout. Next: freeze the vCPU policy from the bracket's start until the worker
answers (saved, or resumed); an L1 test with a deliberately slow save that pins both halves.

### `limina ls` and the control center show a parked VM as running
A supervisor parked behind the play button after a window-menu suspend holds the run lock, so
`limina ls` reads the VM as running, never suspended, although its `[suspended]` record is
written and the runtime socket reports `parked`. The control center derives its row the same way
(`crates/limina/src/center/model.rs`, `snapshot`: `suspended` only when not running), so it shows
the same running VM. Fix: one shared VM-state helper that consults the runtime socket's parked
state (or the record) before the lock, used by both.

### Some device threads can still write guest RAM while a snapshot dumps it
`save_snapshot` holds every device with a `DumpGate` (`VirtioDevice::dump_gate`) from the GPU
capture until it returns: the block and virtio-net workers, the GPU worker and the GPU fence handler,
which renderer threads and the present latch thread also call. Not held, and still able to tear a
raw-path dump: the vsock muxer, the console port threads, virtio-snd, virtio-input and virtio-fs
workers, and the queue handlers the event loop dispatches (rng, balloon, vsock, console control),
which with the vCPUs parked only run for a kick that landed before. On the s2idle production path
the guest has reset all of those devices and their workers have exited; the xHCI worker is quiet by
protocol after the guest's controller save. Fix for the raw path: give each of those devices a gate,
one section per wake, as the block worker has.

### A restored guest can go idle and never wake
`l1_restore_vtimer_offset` fails intermittently under host CPU load (about one run in five, none
on a quiet host) at "the restored guest never ran": both vCPUs resume, settle in HVF's in-kernel
WFI wait at the guest's idle PC, and the guest's heartbeat task is never woken again. The timer
state at restore is sane (unmasked, enabled, deadlines already due) and HVF's vtimer offset matches
libkrun's record, so the wake lost is the heartbeat task's: the overdue hrtimer's expiry or the IPI
behind it. It predates libkrun `9c3cb1e8` (it fails the same way at `bf5fb254`). Measurements,
what is ruled out and the diagnostic patches: `spikes/restore-idle-stall/`. Next: the in-kernel
GIC's restore and any SGI in flight at the snapshot.

### Restored hardware decode freezes instead of resyncing seamlessly
After a restore, virglrs re-creates journaled video codecs/targets and gates the stream until the
next keyframe (`vrend/video/mod.rs`, `Gate::AwaitingKey`; guard `l2_video_vaapi_restore.rs`),
freezing the first dropped frame's picture for the resync window — 81–134 frames at 30 fps (3–5 s)
in the dogfood clips. Seamless resync means giving the re-created codec its reference pictures back:
keep the bitstream since the last keyframe per live codec, carry it as a versioned per-codec record
in the renderer snapshot payload, and feed it to the re-created codec with delivery suppressed
before the guest's next frame. virglrs keeps no such unit log, so it has to be built; cost is one
keyframe interval of bitstream per live codec, and the AV1 serializer's held-frame state has to
travel with it.

---

## Memory

The planned structural fix for most of this section is `docs/design/guest-ram-objects.md`: guest RAM
built from owned, ledger-tagged memory objects released by replacing whole objects (the shape of
Apple's `hv_vm_allocate`), plus a virtio-mem device for the elastic range. It is gated on the
measurements in its §4. The entries below say which of its parts address them.

### Host anonymous memory outlives a worker that died abnormally, until reboot
Measured 2026-09-05 on the dev Mac after a day of crash arms: swap grew from ~8 GiB to 68.7 GiB. At rest, with
no VM running, all processes summed to 10.5 GiB of footprint while the compressor held 12 GiB of RAM
and swap 68.7 GiB, frozen; 39 IOSurfaces system-wide, so not the scanout ring. Left to accumulate it
panicked the host (`watchdog timeout: no checkins from watchdogd in 91 seconds`, `100% of segments
limit (BAD) with 86 swapfiles`). Only a reboot reclaims it. The strongest lead is the post-device-loss
allocator runaway (each dying WebGL-MSAA arm stranded ~100k compressor pages; the KK lost-device
refusal cut that to ~48k), but that does not explain memory surviving its owning process — the kernel
frees a dead task's anonymous memory unconditionally. Candidates: guest pages handed to Metal as
no-copy buffers, driver mappings that outlive the owner, or compressor accounting wrong at this
scale. **Discriminating arm:** record `vm.swapusage` and `vm_stat`, boot a VM, SIGKILL the worker,
re-read after the supervisor exits; repeat with SIGABRT and with a clean exit. A difference between
clean and abnormal exits is ours. The full matrix, which also varies the GPU tier and which
processes survive, is `docs/design/guest-ram-objects.md` §4.2. What is known from xnu: compressor
slots are freed only when their object dies, when MADV_FREE/REUSABLE kills a non-resident page, on
decompress, and on recompress over an existing slot; a task's address map is torn down eagerly at
death, so only a reference to the object itself can keep its memory alive. No outside report
matches: the closest, ClickHouse #126844, gets its memory back when its process restarts.
Owned-object guest RAM makes every release an object death but does not stop an outside holder.
Until settled, anyone running repeated crash arms should watch `vm_stat` compressor pages and
reboot before the segment limit.

### The io give-back's `MemAvailable` guard is scaled by the balloon it gates
`GIVEBACK_AVAIL_CEILING_PCT` (`balloon_policy.rs`, 50) compares `mem_available_kib` against
`mem_total_kib`, the guest-visible total, which the balloon controls. In a restic ladder (2026-08-14),
balloon and total summed to the VM max:

| balloon | guest total | decline threshold = total/2 |
|---|---|---|
| 16.06 G | 7.65 G | 3.8 G |
| 8.56 G | 15.15 G | 7.6 G |

So the guard is strictest exactly when the balloon is largest and most likely the cause — an
actuator-coupled sensor. The coupling is also what makes it self-limiting (each give-back raises
availability toward the ceiling and ends the ladder); decoupling to `max_pages` or an absolute figure
loses that and, replayed against the same traces, is more permissive at rest. **Decide the
denominator against a stress-test trace** with many episodes at different balloon fills. The first
give-back of any episode always fires: the guard bounds a ladder but does not prevent one starting.

### Re-justify or retire the `MemFree` half of the io give-back guard
The doc comment on `GIVEBACK_FREE_CEILING` / `GIVEBACK_AVAIL_CEILING_PCT` says the free ceiling
catches md5sum as an "accumulating" reader. Replaying the md5sum trace (2026-08-13) contradicts it: `MemFree`
stayed at 461–615 MiB through nearly every step of the ladder (under the ceiling, `free_ok = true`)
and spiked to 6.7 GiB only after the ladder ended — the comment took the endpoint for the
trajectory. Of 103 give-backs in that trace the free guard alone fires 39 and the combined guard 17;
on restic 17 → 17 → 5. `MemAvailable` does essentially all the work, and md5sum and restic have one
shape. Either show on evidence that the free half binds, or remove it and correct the comment.

### `inelastic` hold does not distinguish converged from stranded
The `inelastic` verdict fires whenever inflating would dig into page cache, which covers a
well-ballooned guest near max (the designed terminal state) and a guest whose balloon was emptied
while the host still bills a large footprint and the balloon cannot refill because everything is
cache. Measured 2026-08-13: a 1.12 G balloon with the host billing 36.95 G, 24.91 G compressed; it
recovered in ~5 min only because the footprint pushed the host to `warn` and triggered the trickle
dig — host alarm is a poor trigger. Today the discriminator is `actual_bytes` on the trace row. Give
the stranded case its own verdict or label (as `AllowanceBand` and `shortfall` were split out).
Inelastic runs are not downstream of give-backs (0 of 27 long runs had one in the preceding 120 s),
and the io give-back's MemFree/MemAvailable gate narrows the main path into the stranded state.
virtio-mem (`docs/design/guest-ram-objects.md` §3) makes the plugged size an exact contract, which
removes the ambiguity for the range it manages.

### Cadence settle sweeps keep running at near-zero yield on a settled idle guest
On an idle dogfood guest overnight (2026-08-14) the cadence sweep ran every ~30 min and debited 44–54 MiB per
run, against 1,893–3,042 MiB for demand sweeps during activity. Only the demand path judges yield
(`DemandHoldoff`); the cadence arm in `balloon_policy.rs` (`sweep_due(...)`, trace label `cadence`)
sends unconditionally. Cheapest fix: a cadence sweep that yields under `DEMAND_SWEEP_MIN_YIELD`
pushes its own next-due time out. Low priority — each sweep is cheap, and on a 16 KiB host the walk
is short.

### Settle sweeps need a guest pressure report, so agent-less guests keep the 2x footprint
The ledger settle sweep's triggers depend on reports from limina-agent, so a stock guest never
sweeps and keeps the ~2x Activity Monitor inflation (every disk-fed guest page billed twice).
Degraded, not broken. Possible follow-up: a worker-side fallback timer so agent-less guests also
settle now and then. Owned-object guest RAM (`docs/design/guest-ram-objects.md` §2) would bill these
pages once on both tiers and retire the sweep, if its §4.1 shows HVF honours the accounting.

### Post-episode warm-read tax (~1.6x) after a deep balloon dig
After a deep dig and give-back, a fully recovered guest (cache re-warmed, kswapd idle, io-some <1%,
no swap) reads its own page cache at ~16 GB/s against ~26 GB/s pristine (192 vs 118 ms/pass on the
S3 bench vehicle, measured 2026-08-12). Cause unidentified; deflate pacing is ruled out as a lever.
Leading hypothesis: page-cache folio-order collapse, where scattered 4 KiB inflates fragment the
buddy lists and the re-warm under duress rebuilds the cache as order-0 folios. Probes:
`/proc/buddyinfo` and folio-order stats across an episode; in the recovered state `drop_caches` then
a calm re-read (back to ~118 means build conditions, still ~192 means persistent fragmentation). A
real fix probably needs a host-page-aware batched-inflate balloon. Low priority: it bites only after
a real host-pressure episode. virtio-mem unplug compacts in the guest before it hands memory back
(`docs/design/guest-ram-objects.md` §3), which is the batched, host-page-aware path this needs.

---

## vCPU & power

Background for this section — why idle guests need a real-time band, what it costs, and what
ships — is in `docs/design/vcpu-scheduling-band.md`.

### The Game Mode clamp: what launching the worker through launchd leaves open
While a games-category app is fullscreen and frontmost (Moonlight is one: `LSSupportsGameMode=true`),
`gamepolicyd` turns Game Mode on and clamps every process in an app's process tree to priority 4:
timers ~100 ms late, CPU denied, CoreAudio callbacks stalled for up to a second
(`spikes/game-mode-throttle/RESULTS.md`). The worker is outside that tree (launchd starts it,
`supervisor.rs` `launch_through_launchd`; `l1_worker_lineage`). Open:
- **Confirm it on the dogfood Mac.** The suite cannot turn Game Mode on. The oracle is `ps -M -p
  <worker>` reading 31 (and `97R` for the banded vCPU) while `gamepolicyd` logs `Game mode status is
  now on`, with smooth guest video and audio beside a fullscreen Moonlight.
- **The supervisor stays clamped** (it owns the window), so its present path into the window still
  runs late under Game Mode. Measure what that alone costs.
- **Identity.** TCC attributes mic and camera use to the process responsible for it, now the
  launcher job rather than Limina.app. The worker is signed as `eti.noronha.limina` with the app's
  Info.plist, so grants should land on the same identity; recheck audio capture from a
  Dock-launched app.
- **Strict timers on the present and audio paths**: the `gpu latch` thread's `thread::sleep` to its
  35 ms deadline (`virtio_gpu.rs`, `LIMINA_FENCE_LATCH_MS`), and anything else there. Still worth it
  for the supervisor's side, which stays clamped.
- **Not owed: Mach channels.** The control line and input datagrams are unnamed socketpairs carried
  as fileports, unreachable by anything else; Mach would mean `EVFILT_MACHPORT` in libkrun's loop
  for whole-message writes and fewer fds, nothing more.
- **Detect and explain**: our own threads at priority 4 are a direct oracle. Tell the user, and
  name the game-side levers (the menu-bar toggle, which held across re-fullscreens;
  `LSSupportsGameMode=false` in apps they build). This also covers the fallback: when launchd
  cannot be used, or `LIMINA_WORKER_LAUNCH=spawn` asks, the worker is the supervisor's child again
  and is clamped with it.

### Explain the parked P-clusters behind the band panic
A host panic on 2026-09-21 (`watchdog timeout: no checkins from watchdogd in 94 seconds`, panicked task
`limina-vmm`) showed four vCPU threads at priority 97 running on `CORE 0-3 [EACC0]` with both
performance clusters offline. xnu does run time-constraint threads on efficiency cores
(`spikes/rt-ecore-placement/`), so that half is expected; what is unexplained is why the
performance clusters were parked. On an M1 Max a saturated RT thread always moved to P and no
non-root signal (`kern.sched_recommended_cores`, per-processor `running`, tick deltas) ever showed a
core offline, so reproducing it needs the M4 Pro shape and a validated parking oracle;
`kern.suspend_cluster_powerdown` is uninvestigated. Worth a Radar independent of our mitigation —
any unprivileged process can panic macOS by banding enough threads and saturating them — but
filing is the user's call.

### Revisit the band arm cap, which costs about 12% of `gl-replay-venus`
`arm_cap()` is half the efficiency cluster, floored at 1 (1 on an M1 Max, 2 on an M4 Pro). Every
vCPU runs in latency-QoS tier 0 by default (`88b29e8b`), and with that the shipped default scores
50.0 on `gl-replay-venus` against 56.6 (`perf/2026-10-16-venus-regression.md`, which attributes
the gap to the cap): the static all-banded arm restored 56.2–56.9 in `perf/2026-09-21-remeasure.md`
and is not re-run: it is the configuration that panicked a host. The cap stays for safety, by decision. A revisit must account for: the two
band-sensitive instruments disagree in sign (banding all four vCPUs restores `gl-replay-venus` but
cost ~12% on `vk-replay-venus-headless`, `perf/2026-09-21-remeasure.md`);
a replacement rule must stay bounded by what survives an idle machine (`hw.activecpu` does not — it
read 10 of 10 on an idle M1 Max while the panicking M4 Pro had 10 of 14 cores offline); and the M1
Max is the worst case for the current rule, so measure on an M4 Pro before generalising.

### Idle the band sampler when nothing is presenting
The `rt+dyn` sampler (one thread per VM, 200 ms) runs whether or not anything reaches scanout.
Idle wakeups are a budget already won (`docs/design/venus-ring-idle-wakeups.md` took the worker from
~75/s to ~0/s). Stop the sampler entirely when nothing is presenting; both gating inputs exist
without an agent — the host power state (`NSProcessInfo.isLowPowerModeEnabled`,
`IOPSGetProvidingPowerSourceType`) and whether anything reaches scanout. The idle far tail is also
not fully clean: max 31–49 ms even when armed (`spikes/macos-timer-wakeup/results-guest-arms.md`).

### A lightly loaded vCPU runs its work several times slower
Measured 2026-10-02 on a stock F44 guest (kernel 7.1.8, 8 vCPUs) on a base M1 (4P+4E, 16 GB), so
the guest has one vCPU per host core. A guest thread pinned to one vCPU, sleeping 5 ms between busy
periods, times a fixed chunk of integer work at 84 ns when the vCPU is busy for ≥ ~60% of the time.
Below ~50% the chunk takes 167-459 ns, typically 291 ns (~3.5x). That is close to the 3.75x
slowdown measured for a `QOS_CLASS_BACKGROUND` vCPU when sizing `--little-vcpus` (`docs/roadmap.md`).
The slowdown lasts for the whole burst: a 10 ms burst after a 50 ms gap stays slow throughout.
It follows utilisation over tens of milliseconds, not burst length. Syscalls show the same split.
`getppid()` costs 0.6 µs busy and 3.5 µs after a 16 ms gap. A `FUTEX_WAKE` costs 3.3 µs busy and
16-18 µs after a ≥ 5 ms gap, the same whether the wakee is on the waker's vCPU or another one, so
the slow party is the waker. The slow values are stepped (167/291/459 ns), which reads as E-cluster
clock steps rather than one core type. That is inference: nothing yet reads the host core or clock.

Why it matters: a browser's light load is all in this regime. Media playback in Firefox is a chain
of thread handoffs at ~70 frames/s, each landing on an idle vCPU. `futex_wake` came to 11% of the
content process's samples, and kernel time to ~45% of its media threads' CPU. It also makes guest
CPU time an unstable measure of work: an idler guest runs a fixed-work probe slower.

What is ours to try, in order:
- **Observe it from the host first.** Read each vCPU thread's core while the duty probe runs (the
  `TPIDR_EL0 & 0xfff` trick from `spikes/rt-ecore-placement/`), plus `powermetrics` per-cluster
  frequency. That confirms or kills the E-core/DVFS reading.
- **Halt polling is not available**: HVF parks WFI inside `hv_vcpu_run`, so the thread never
  gets the chance to stay hot ([band design](design/vcpu-scheduling-band.md)).
- **Thread policy does not move it.** Latency-QoS tier 0 and the band leave it as it is: a 6%-duty
  chunk takes 500 ns in every arm against 84-125 ns at 67% duty, and a futex wake after a 5 ms gap
  costs 19-27 µs in every arm (`spikes/guest-vcpu-qos/RESULTS.md`). On the host, only an
  `os_workgroup` whose thread brackets each period moves a moderately busy thread to P, and not at
  2% duty (`spikes/launch-path-qos/RESULTS.md`); a vCPU thread cannot bracket the guest's periods,
  and whether a joined, un-bracketed thread is placed on P is unmeasured.
- A guest `idle=poll` A/B would confirm the utilisation theory, but not with as many vCPUs as host
  cores.

Knock-ons: the grow rule reads worker CPU time (`HostCpu` in `crates/limina/src/vcpu_policy.rs`).
An E-placed vCPU burns more host time per unit of work, which biases that rule. It may also be part
of *Host-CPU cost of a busy VM is unattributed* below.

### Measure power beyond idle, and anything against Parallels/Vz
Known: an idle guest costs ~13 mW of package power over an empty host, a `vkcube` guest ~0.9 W
(`spikes/macos-timer-wakeup/results-battery.md`); GPU compute throughput over the Metal transport
is measured against the same work native (`perf/2026-10-18-compute.md`, speed only). Disk,
network, builds, video calls, compute and a desktop in use have never been measured in watts,
nothing has been measured against Parallels or Virtualization.framework, and "replace Parallels" is
a perf/W claim with no evidence.
Needed: a work-unit counter per workload (frames, IOPS, packets, wall-clock for a fixed build —
watts alone rank a slow VM first); package power without a human (`powermetrics` needs root, a use
for the deferred privileged helper, `docs/design/privileged-helper.md`); the same workloads under
Parallels and Virtualization.framework on the same host; and the band's host cost under I/O- or
GPU-bound host work (only a CPU-bound job has been measured). Harness:
`spikes/macos-timer-wakeup/battery-cost.sh` + `pm-align.py`. Traps: interleave arms (pack voltage
sags as it drains), verify per block that the differential reached the guest, and never difference
`AppleRawCurrentCapacity` (a self-refitting estimate).

### Host-CPU cost of a busy VM is unattributed (Parallels appears cheaper)
Observed on dogfood (2026-09-03): on similarly busy guests Parallels accrues visibly less Activity Monitor %CPU than our worker. The
standing costs (guest timer exits, the display/present pipeline, the 1 s agent heartbeat, the
virtio-net interrupt-status reads below) are itemised only at idle (`docs/perf/overhead-inventory.md`).
Profile the worker under a representative busy desktop, attribute CPU by exit reason / device /
thread, and compare like-for-like with Parallels before naming a cause. Score with Energy /
`powermetrics`, not the %CPU column.

### vCPU grow thresholds are unvalidated through the middle of the workload range
`crates/limina/src/vcpu_policy.rs` grows on a corroborated runnable spike (1.5 cores busy), a PSI
stall with a utilisation floor (0.8), `load1 >= 2`, or the **host term** (worker process within 0.25
of a core of `online`). The constants were set against an idle desktop, a real desktop day and
synthetic spinners; untested: a compile with a serial link step, a browser playing video, IO-heavy
work where `busy` stays low while tasks wait. Collect traces (`spikes/vcpu-replug-trace/`), check
grow latency and false-grow rate, and move constants only on evidence.
Measured on the dogfood desktop 2026-09-03..06: grows out of 2 online fell 27.4/h → 2.2/h → 0.8/h.
The survivors carry no guest signal and at 0.12–0.18 cores busy exclude every guest path, leaving
the host term — at 2 online its bar is 1.75 cores, which the worker's device threads (the GPU
renderer above all) can clear with the guest idle. Confirming needs the `dynamic vCPUs: … host
{:.2} cores` line, which is `info!` and absent at the shipped `warn` level: run the host with
`RUST_LOG=warn,limina=info` and re-install the guest sampler. A 2 s sampler cannot confirm a
crossing exactly, since the agent reports every 1 s.

---

## Nested virtualization

### About one L2 boot in ten hangs on an interrupt the L2 is never shown
Measured 2026-10-12 on an M4 Pro, macOS 27.0.1 (`spikes/nested-virt-probe/RESULTS.md`, *A real
L2*): a QEMU+KVM guest inside a nested limina guest hangs in 2-3 of 25 boots, spinning on `WFI`
with its virtual timer (PPI 27, hardware-mapped) pending in the L1 KVM's list register. Booting the
L1 kernel with `kvm-arm.vgic_v3_group1_trap=1` (KVM emulates the L2's CPU interface itself) makes
75 of 75 pass. That needs a guest change, and every guest that runs VMs would need it: find the
host-side cause instead. A fault this visible in HVF itself would hit every VZ guest that nests,
so first rule libkrun out:
- **A/B against VZ.** Boot the same image under Virtualization.framework with
  `VZGenericPlatformConfiguration.isNestedVirtualizationEnabled` and run `l2/hang-catch.sh`. If it
  does not hang, diff what VZ presents against libkrun: the ID registers (`ID_AA64PFR0_EL1` GIC
  field, `ICH_VTR_EL2`), the GIC configuration and the devicetree's maintenance interrupt, the
  timer PPIs. QEMU >= 11.1 (HVF nested over Apple's vGIC) is a second comparison.
- **What reaches libkrun while an L2 runs.** Every L2 entry and exit traps `CNTHCTL_EL2` to
  libkrun, which writes it through (`devices/src/legacy/vcpu.rs`, `handle_sysreg_read/write`);
  check the value HVF expects there, whether it should be masked, and whether the hang rate
  follows the round-trip's latency (3 of 6 boots hung with libkrun's debug logging on).
- **The timer's mapped interrupt.** KVM drives PPI 27 through its active state on HVF's GIC
  (`irq_set_irqchip_state`); trace the L1 KVM's `kvm_timer_*` and `vgic_*` events at a hang to see
  whether the L2 ever reads `ICC_IAR1_EL1`, and what the LR holds when it does.

### Both machines name the wrong hypervisor timer interrupt
libkrun lists PPI 12 (INTID 28) as the EL2 timer in the device tree's timer node
(`devices/src/fdt/aarch64.rs`, `create_timer_node`) and in the ACPI GTDT, the default machine
(`devices/src/acpi.rs`, `build_gtdt`, which `gtdt_names_the_device_tree_timer_interrupts` pins to
the device tree's values). HVF raises the EL2 physical timer on INTID 26
(`hv_gic_get_intid(HV_GIC_INT_EL2_PHYSICAL_TIMER)`, measured in
`spikes/nested-virt-probe/RESULTS.md`, *Timer interrupts at EL2*). Nothing reads that entry today:
Linux runs nVHE, and Cardea uses the EL1 virtual timer at EL1 and polls at EL2
(`third_party/cardea/firmware/src/arch.rs`, `idle`). A VHE guest kernel would get no timer. Fix
both tables from `hv_gic_get_intid` when EL2 is on, and the test with them.

---

## GPU robustness — guest-reachable aborts & containment

### The "no guest-reachable aborts" audit, restated for virglrs
Policy by layer: asserts guard internal invariants only; host drivers (KK, zink) tolerate bad input
by clamping or skipping, because `vkCmd*` cannot return errors; the renderer is the trust boundary —
validate, then poison the context; libkrun's Rust decoders return errors instead of `unwrap`.

Where virglrs stands: its non-test `src/venus` and `src/vrend` code holds about 580
`unwrap`/`expect`/`panic!`/`assert!` sites (about 690 with `assert_eq!`, `assert_ne!` and
`unreachable!`; a grep that skips comment lines and stops each file at its `#[cfg(test)]` module).
Most are `expect`s on invariants — formatting into a `String`, a value the decoder already bounded,
a lock — not on guest input. The guest-facing decoders carry their own hardening: many guest-input
fixes, Kani proofs over the venus command stream, ring and guest-memory access
(`src/venus/cs.rs`, `src/venus/ring.rs`, `src/guest_mem.rs`, `src/vrend/transfer.rs`), and fuzz
targets for `venus_command`, `tgsi_translate`, `h264_annexb`, `h265_picture`, `av1_frame` and
`sync_blob` (`third_party/virglrs/fuzz`). No fuzz target covers the classic vrend command decoder
(`src/vrend/decode.rs`), which is how a guest's GL stream reaches zink. The guest-reachable aborts
that survive are in libkrun's virtio-gpu device (next entry).

Next: a vrend decode fuzz target. Recount KK's asserts (`vulkan/` and the `vk_meta*` it compiles in)
only when a KK hardening pass is scheduled.

### Guest-controlled input aborts libkrun's virtio-gpu device
Two guest-reachable panics, both taking the device with them:
- **Descriptor chains.** The GPU worker builds each control-queue chain's reader and writer with
  `Reader::new(..).unwrap()` / `Writer::new(..).unwrap()`, and each cursor-queue chain's reader the
  same way (libkrun `devices/src/virtio/gpu/worker.rs`). Popping a chain checks only the descriptor
  table, so a descriptor whose buffer lies outside guest RAM fails `Reader::new`
  (`FindMemoryRegion`, `descriptor_utils.rs`), and the panic ends the GPU worker thread: every
  context, scanout and fence of the VM stops.
- **Blob flags.** `resource_create_blob` (`virtio_gpu.rs`) panics with `GUEST_HANDLE unimplemented`
  when the guest's `blob_flags` carries `VIRTIO_GPU_BLOB_FLAG_CREATE_GUEST_HANDLE`; the flags arrive
  from the guest unmasked.

Reaching either needs a crafted guest kernel, not an application. Fix, RED-first: log the bad chain
and complete it with an error (or skip it) instead of unwrapping; answer the flag with
`ErrInvalidParameter`.

### A virglrs panic ends 3D for the rest of the VM's life
A panic inside virglrs on the render thread is caught at the rutabaga boundary: every call into
the renderer goes through `Guarded::contain` (libkrun `rutabaga_gfx/src/virgl_renderer.rs`), a
`catch_unwind` that marks the renderer dead, refuses every later call with `EIO`, and leaks the
renderer instead of dropping it. That works because the worker unwinds: no profile in the root
`Cargo.toml` sets `panic`, and virglrs's own `panic = "abort"` (its `Cargo.toml`) applies only
when virglrs is built as its own workspace, since cargo ignores a dependency's profiles. So the
comments that say virglrs's panics abort are wrong for the shipped worker: in `third_party/virglrs`,
`CLAUDE.md` ("Panics abort; they never unwind into C"), `src/venus/cs.rs` (two, under
`panic = "abort"`), `src/venus/ring_thread.rs` (the ring join's `expect`) and
`src/vrend/video/mod.rs` (the decode thread's join); in limina, `scripts/run-suite.sh` and
`scripts/tests/run-suite-verdict.sh` (the worker "builds panic = abort").

The containment is the whole renderer, not the context that panicked: the tables, the GL context
and the venus decoder are shared, so nothing establishes that one context's damage stays in it.
After a panic the guest keeps its disks, network and console, but loses 3D, and the fences pending
in the renderer at that moment never retire (new ones are retired as lost by `virtio_gpu.rs`).
Not contained: a panic on a virglrs-owned thread (venus rings, decode, fence waiters) only ends
that thread and is logged by `limina-paniclog`, which is the worker's only panic hook; the
renderer is not marked dead and 3D hangs. A panic in a Rust callback entered from C (VideoToolbox,
GL debug output) aborts the worker, since an unwind cannot leave an `extern "C"` function.

Some host-state `expect`s and `panic!`s in vrend turn a degraded GL stack into renderer death:
making a sub-context's or ctx0's GL context current (`vrend/context.rs` `make_current`;
`vrend/vrend.rs` `switch_ctx0` and three loops over every context's GL contexts), the blitter's
context (`vrend/context/blit.rs`), and a refused adoption of minted storage, whose message carries
the guest's dimensions (`vrend/resource.rs`). Prefer a fault that poisons the context.

Next: one panic policy for the worker, written down and matched by the comments; mark the renderer
dead when a virglrs-owned thread panics, as `Guarded` does for the render thread; retire a dead
renderer's pending fences so the guest's compositor fails cleanly instead of waiting; and decide
whether per-context containment is worth a virglrs audit of what a context's handlers can leave
half-updated. `spikes/piglit-virgl/` is the regression check for guest-reachable aborts on the GL
path.

### Nothing stops a guest from submitting seconds ahead of the host's decode
A classic (vrend) client that submits faster than the virtio-gpu worker decodes keeps the control
queue permanently non-empty. Measured 2026-09-12 on a stock F44 guest with the webglsamples aquarium at
15k fish: the worker was CPU-bound in a single `process_queue` drain for up to 25 s — ~75% decoding,
~25% blocked in `Vrend::fence_global` → `glFenceSync` → mesa `tc_flush`/`_tc_sync` (a full
threaded-context sync on every guest global fence) — while the page's fps counter read ~57.
Presenting retired frames from inside the drain keeps the window live, but every input and frame
shown is seconds stale under overload. Owed: a back-pressure mechanism that bounds a guest's queued
work against the host's decode rate. Unevaluated candidates: hold guest fences until their work is
*decoded*, not just queued; bound a context's in-flight work; stop draining after a budget and let the
guest's queue fill. It must not bring back the head-of-line blocking the present pump removed, nor
let one heavy context stall another. The cost of a global fence (`fence_global` in virglrs
`vrend/vrend.rs`; `waiter.rs` measures `glFenceSync` at 21% of that path) is a separate throughput
item.

### WebGL antialiasing loses the host Vulkan device (mitigated, cause unknown)
A guest GL context that takes 4x MSAA (WebGL `{antialias:true}`, the spec default) loses the host
device (`vkQueueSubmit → VK_ERROR_DEVICE_LOST`), usually within a couple of minutes; stochastic,
about one arm in four survives, so one arm per side proves nothing.
**Mitigation in force:** the worker sets `VREND_MAX_SAMPLES=1` unless the environment sets it
(`crates/limina-vmm/src/krun/mod.rs`); virglrs caps the advertised `max_samples` (`vrend/caps.rs`,
`capped_samples`). The guest gets `antialias:false` and takes the single-sample path, which has never
died. Cost: no antialiasing for any guest GL app; `VREND_MAX_SAMPLES=4` restores MSAA for testing.
The KK allocator pool refuses to mint once the device is lost, so a loss no longer runs into
unbounded allocation and SIGABRT.
**Established** (`spikes/webgl-msaa/RESULTS.md`): the failing work is the guest's own 4-sample colour
passes whose fragment shader executes a texture `sample()` (a page sampling no texture survives; the
depth attachment makes no difference). The descriptor bytes on the root→set→resource-id→sampler path
are correct when encoded and still correct at the loss. The faulting address falls in no KK
allocation; shader loads from unmapped addresses and bindless reads through invalid `MTLResourceID`s
return zero rather than fault (`spikes/gpu-fault-calibrate/`). So the faulting agent is a
fixed-function unit (sampler, or render-backend load/store) working from a descriptor whose extent
or stride is wrong, not a stale handle. The Metal error is `PageFault` on some runs and `Hang` on
others; serialising every submit (`KK_LIMINA_SERIALIZE=1`) does not prevent it.
**Next:** a no-VM repro (the host loop in `spikes/webgl-msaa/` does not reproduce yet); the `.ips`
faulting VA and the `LIMINA_KK_ADDR_LOG=1` allocation log in the same frame; forward zink's
`MESA_TRACE=markers` labels to the Metal encoder so the report names the operation. Stop varying
display size, canvas size or fullscreen. A real fix must show the page running ≥5 minutes with the
cubes visibly rendering and MSAA granted (`getContextAttributes().antialias` read back), then the
suite and an eyeball pass. Vehicle: `spikes/webgl-msaa/run-arm.sh` (VOIDs an arm with no live
browser or no multisampled blit on the wire; needs `LIMINA_ARM_EXPECT_MSAA=0` under the mitigation).

### Run a ThreadSanitizer round on the virglrs + zink-on-KK host stack
virglrs's vrend keeps the seam that produced the C-era races: a fence waiter thread makes its own
context current in ctx0's share group and waits on syncs (`vrend/waiter.rs`), entering zink's
screen/batch state from a thread zink does not know about; virgl-over-zink is undertested upstream.
The zink race fix is on `limina-kk` ("zink: fix the data races on the cross-thread batch/fence
state", `6aa1f4a2105`); its companion "kk: make the limina debug counters atomic" is only on side
branches (`parked/fence-sync`, `wip/keep-zs`), not on `limina-kk`. One race is left alone
deliberately because locking it is a hot-path perf decision: `zink_batch_reference_resource_move()`
and its `_unsync()` twin mutate the same batch lists from different threads by design. No TSan pass
has covered the Rust renderer. virglrs's loom models cover the venus ring's park and wake
(`src/venus/ring_thread.rs`) and fence retirement (`src/fence.rs`), not vrend's waiter. Recipe:
build host mesa with `-Db_sanitize=thread` into its own prefix and point the worker at it with
`MESA_PREFIX`; link the TSan runtime into the worker with
`RUSTFLAGS="-C link-arg=<tsan dylib> -C link-arg=-Wl,-rpath,<dir>"` (a `DYLD_INSERT_LIBRARIES`
preload is stripped across the worker spawn, and TSan then aborts with "interceptors are not
working"); boot with `TSAN_OPTIONS="halt_on_error=0 log_path=…"`; run a rendering workload (boot alone
misses races that appear once something renders); only one TSan runtime can be loaded. TSan does not
perturb the graphics behaviour away, so it is a rare non-destructive oracle. The C-era reports are in
`spikes/notification-text-corruption/evidence/`.

### GTK4 aborts when `vkGetPipelineCacheData` fails
GTK4 does not check the size query result of `vkGetPipelineCacheData` and aborts in `g_malloc` on
the uninitialised size when the driver fails the call, which a driver may do. No guest hits this
today (the venus trigger is fixed), but it is an upstream GTK bug worth reporting. Probe and write-up:
`spikes/venus-ring-fatal-timeout/`.

---

## GPU correctness

### Every missing venus reply reaches the guest app as `VK_ERROR_OUT_OF_HOST_MEMORY`
The `limina-guest` commit "venus: surface ring loss as VK_ERROR_DEVICE_LOST instead of abort()"
makes a submit on a FATAL ring return `VK_ERROR_DEVICE_LOST` from the ring layer, but
the generated `vn_call_*` (`vn_protocol_driver_*.h`: `dec ? decode : VK_ERROR_OUT_OF_HOST_MEMORY`)
still turns any absent reply into OOM. Reply-pool allocation failure, ring FATAL at submit, ring FATAL
while waiting and a real host refusal all reach the app as one code — the mechanical root of "OOM out
of venus is never memory". Fix, guest-side and upstreamable (`limina-guest`): carry the submit's
result into `vn_ring_submit_command` and have `vn_call_*` return it when the reply is absent. Cheap,
and it makes the next dogfood OOM report classify itself.

### piglit's limina-only failures on classic virgl: rerun, pick a flavour, then the KK remainder
About 100 tests of piglit's buffer, PBO and texture-transfer selection fail on limina and pass on
the upstream rig; most are in virglrs vrend's GLES 3.1 flavour, the rest in KK or zink. The split,
test by test, is in `spikes/piglit-virgl/README.md` (*Against the upstream rig*). Owed, in order:
- **Rerun the 27 timeouts on the current pins.** All are indexed draws under transform feedback,
  which GLES refuses; virglrs de-indexes them since `6c6a60b` ("vrend: draw indexed under
  transform feedback as arrays on GLES 3.1", pinned), and the selection has not run on it.
- **Decide which flavour ships.** The desktop flavour passes 69 more of the limina-only failures
  and regresses 26 uploads.
- **The residual KK/zink list**, which fails under zink-on-venus too:
  `arb_draw_indirect-draw-elements-prim-restart-ugly` (passes on the desktop flavour),
  `arb_shader_image_load_store@host-mem-barrier`, `ext_transform_feedback2@counting with pause`
  and the five geometry-shader xfb tests (KosmicKrisp entries below).

### vrend guests on the GLES flavour compute every double as zero (low priority)
With geometry shaders on in KK, the GLES host flavour lifts vrend guests to GLSL 430, which exposes
`ARB_gpu_shader_fp64`. On a GLES host the guest's virgl driver drops every double instruction
(`virgl_tgsi.c`, `fake_fp64 = HOST_IS_GLES`), an upstream hack so a GLES host can reach GL 4.0, so
each double result reads 0: 357 generated `glsl-4.00` built-in-function tests fail on the vrend tier
in every stage. They skipped before GS. Not a vrend translation bug: host GLES has no fp64 to offer,
and vrend sets `has_fp64` only on the desktop flavour. Fix shape, two halves:
- **Host, limina-only:** let ES contexts in host Mesa enable `ARB_gpu_shader_fp64` (no ES spec has
  doubles, so it is carried, never upstreamed); vrend then sets `has_fp64` on the GLES flavour.
- **Guest, one line on `limina-guest`:** fake only when the host lacks fp64,
  `HOST_IS_GLES && !has_fp64`. It reuses an existing caps bit, so no protocol change, and is an
  upstream candidate. Stock guests keep the zeros they get today.

First, one vrend-tier run on the desktop flavour: KK has no hardware fp64, so zink emulates doubles,
and whether that emulation computes correctly on KK is unmeasured. If it does not, the fix belongs
there and the ES/desktop split is beside the point.

### Re-verify CPU-write → GPU-read coherency on a shared dmabuf under virglrs
A guest that does `gbm_bo_map` → write → unmap on a LINEAR `Argb8888` dmabuf and then samples it from
venus read the buffer's previous contents: the write reaches the host as a control-queue transfer on
libkrun's gpu worker thread, venus reads on its own ring thread, and nothing orders the two. The fix
takes two halves: guest mesa virgl flushes and waits for the bo to idle on unmap of a write map of a
`PIPE_BIND_SHARED` resource ("virgl: settle a CPU write into a shared resource before unmap
returns" on `limina-guest`), and the host finishes the upload before the virtio-gpu
fence signals. Under the C renderer the guest half alone still failed the first write after boot in 5
of 6 boots; both together failed 0 of 7 (measured 2026-08-14). Owed:
- **Re-run the reproducer under virglrs.** The host half was a C-vrend change; virglrs's fence path
  syncs ctx0, where contextless transfers land (`vrend/waiter.rs` `Answer::Syncs`), which should cover
  it but is unmeasured. Reproducer: `spikes/dmabuf-cpu-coherency/probe.c` in a clone of
  `Fedora-Workstation-44.enhanced.synoik.raw` (each failing pass returns exactly the previous pass's
  colour; method in that spike's `RESULTS.md`).
- **The seam is not transfer-specific.** Any control-queue work races the venus ring, so a vrend GL
  render into a shared bo that venus consumes has the same hazard for a consumer that skips implicit
  sync. `VIRTGPU_WAIT`/sync-file consumers are safe; a bare Vulkan importer has not been probed.
- Formats and modifiers other than `Argb8888` + LINEAR are unmeasured.
- The stock tier has no guest half and keeps the bug (documented degradation). The long-term fix is
  host-visible-blob backing for shared bos, which removes the transfer entirely.

### Khronos VK-GL-CTS as an opt-in validation layer on the enhanced guest
Not started. Run dEQP-VK (via venus) and KHR-GL/dEQP-GLES (via vrend) inside the enhanced guest, so
the whole owned stack runs end to end. The current oracles (pixel probes, venus_replay, glmark) catch
crashes and gross misrendering, not format, precision or sync edge cases. Keep it out of the default
suite (a full run takes hours). Sketch: build CTS for aarch64-linux (the build container or the F44
build guest), stage it into the test image or a virtiofs share, drive curated caselists over ssh from a
`scripts/`/xtask runner; start with `*-main` mustpass subsets and a minutes-long smoke list, diffed
against a known-failures baseline.

## GPU present & scanout

### vrend scanout flushes carry no fence off the Metal transport, so a GL desktop pays a Metal copy per frame
`virtio_gpu_plane_prepare_fb` (`drivers/gpu/drm/virtio/virtgpu_plane.c`) returns before allocating a
plane fence for any primary plane whose bo is not a guest blob, then fences only dumb or imported
objects. A GNOME desktop scans out through vrend (non-blob), so its flushes carry no fence, no
`GuestFlushHold` forms, and the compositor may render into the buffer while it is on glass. The
supervisor covers that with a Metal-blit copy of every unheld frame (`docs/graphics.md` §4,
`window/copy.rs`), and that copy is what keeps the stock tier correct: the window server composites
the copy, never the buffer the guest is drawing into. It adds ~1.2 ms of latency per frame,
4.6–6.1 ms worst case under a 24–33 fps WebGL load (measured 2026-09-12). limina's DKMS module
fences every primary-plane flush when the host offers the limina extension
(`guest/virtio-gpu-limina/0001-hook-the-limina-extension.patch`), which the host does only with
`--gpu-metal`, so this is closed on the transport and open everywhere else. Remaining: fence
when the host holds flushes without the extension (a capability of its own in `LIMINA_INFO`, or the
module keyed on something the host offers in venus mode too), and confirm on a vrend desktop that
the flush parks rather than taking the blocking `sync_iosurface` path. Check with
`LIMINA_PRESENT_MUTATION_TRACE=1` (zero surfaces changed while up) and the worker's
`scanout N flushes are fenced` line.

### A venus scanout occasionally shows an older complete frame after a newer one
With the ordered present copies in place, a host-side screen recording of a stressed two-output
synoik desktop with frame stamps on showed 9 of 1075 recorded frames older than one already on
screen (measured 2026-09-30; 66 before the copies). Each is a complete frame, one or two stamps
back, never a run. Two shapes: *late* (an older frame never recorded before its successor, 6 of 9)
and *repeated* (`A B A`, 3 of 9). synoik's write-up is `LIMINA-scanout-shows-older-frame.md` in
its repo. A one-frame jitter, not corruption. The unfenced-flush race `docs/graphics.md` §4 leaves
open cannot produce this: it shows a newer or partly drawn frame, never an older finished one.

Established from the worker log over the same session: the worker's order check
(`scanout N stepped back`) logged nothing, and no frame was dropped for a busy ring until after the
recording. The copy rings grew to 5 and 6 surfaces per output under the load. So the worker handed
frames on in flush order, unless that check does not cover parked copies. Confirm that first.
Then the candidates are downstream of the worker: the supervisor presenting or re-presenting
surfaces out of order, the window server showing a reused copy surface's earlier content, or the
recorder itself. Next: reproduce on a dev-Mac clone with synoik, its runtime frame stamp
(`synoik msg action debug-toggle-frame-stamp`) and draw ledger, and log, per scanout, the order the
supervisor hands surfaces to the layer, to compare with the recording. Also extend `kmschurn.py
stamp2-vk` so that the stamp each output shows can never decrease, under a forced host delay and
CPU load. That is a check without a recording.

### `vkWaitRingSeqnoMESA` blocks the virtio-gpu control thread behind one client's ring
libkrun's `submit_all` (`rutabaga_gfx/src/virgl_renderer.rs`) releases the renderer lock before a
ring wait but still calls `waiter.wait()` on the control-queue thread, so a ring in the middle of
`vkCreateGraphicsPipelines` (hundreds of ms on a cold cache) stalls every other context's
`SET_SCANOUT`, flushes, cursor and fence processing for that long. Venus's design, not a bug against
upstream, but a latency fault of ours to measure: log wait durations on a seated desktop under a
shader-heavy client and see how often the compositor's flush sits behind one. If it matters, make the
wait asynchronous (park the execbuf's continuation — its fence and the following `CREATE_BLOB` — on
the ring seqno). That touches the virtqueue FIFO ordering contract `CREATE_BLOB` relies on, so it
needs a design, not a patch.

### A WebGL window repaints as a slideshow in the GNOME overview unless another window is hovered
User-seen 2026-09-12 on stock Debian (GNOME 50.3, vrend) with the WebGL aquarium in one Firefox window and a
second page in another: with the overview open, the aquarium's thumbnail repaints as a fast slideshow
while nothing or its own window is hovered, and at its reported frame rate while the other window is
hovered. Outside the overview both are fine. Not measured. Start from the worker log's
`control queue drain ran` lines in each hover state and check whether presents stall or the guest
stops drawing. The hover dependence points at the compositor choosing what to repaint until a log says
otherwise.

---

## GPU perf

### vrend's cold shader and pipeline compiles run on the virtio-gpu worker thread
A vrend context drawing something new freezes the whole virtio-gpu device for as long as the
compiles take: other contexts' flushes, presents and the cursor queue all wait. Measured 2026-10-20
on the synoik image with Flathub Loupe 51 (its GL extension has no venus, so GTK's GL renderer runs
on virgl), with `LIMINA_WAKE_PROBE=1`: startup drains of 589, 999 and 1211 ms, of which 579, 995 and
1180 ms were fence handling, and single `SUBMIT_3D`s of 189 and 388 ms. Opening a dialog with new
shaders did the same. A worker `sample` places the fence time in `Vrend::fence_global` →
`take_fence` → `glFenceSync`. Mesa flushes that without `PIPE_FLUSH_ASYNC` (`syncobj.c`: deferred
only with a single context), so `tc_flush` syncs the threaded context and runs the queued zink draws
inline, including `zink_create_gfx_pipeline` compiling on KosmicKrisp, which has no
graphics-pipeline-library path to defer them. The `SUBMIT_3D` outliers are the front of the same
chain: TGSI→GLSL translation and program linking in `select_linked_program`. The cost varies from
boot to boot with what is already compiled.

Single guest ioctls stalling for 100 ms to seconds are probably the same class. Firefox's canvas
thread on a stock-session guest (M1 host, Basemark Canvas Test, guest Mesa `26.2.3-3.limina`)
logged 27 ioctls over 100 ms in 8 traced 15 s runs — about 3 a run, typically 100–475 ms — spread
over `RESOURCE_CREATE`, `GEM_CLOSE`, `EXECBUFFER`, `MAP` and `WAIT`, on the PBO and the CPU-pointer
arm alike (measured 2026-10-05). Two runs opened with multi-second ones in their first 100 ms
(`VIRTGPU_MAP` 7.3 s, `RESOURCE_CREATE` 3.4 s, both under `BufferData`). Nothing guest-side
distinguishes the stalled calls from their thousands of fast siblings. Confirm by catching one with
`LIMINA_GPU_TRACE` and the worker log at the matching timestamp; the other candidate for the queue
head is the `vkWaitRingSeqnoMESA` entry above.

Fix shape (decided): move vrend off the virtio-gpu worker thread, so a compile holds only its own
context's fences and never the queue. Two smaller levers were set aside: an async fence flush in
the host Mesa fork (vrend's sync thread waits on those fences from another, shared context, which
is the case Mesa's single-context guard exists for), and persistent host pipeline caches (they
shorten the first stall per host but leave the structure). Shortening the compiles is not enough
either: SPIR-V programs (`docs/design/vrend-spirv.md`), skipping the host's second varying
optimisation, handing a batch to zink's driver thread at the first draw after a new shader, and
KosmicKrisp compiling a pipeline's two stages on two threads together took Loupe's About dialog
only from about 185 ms to 156 ms, Mesa-cold and Metal-warm, and the SPIR-V path empties the per-VM
shader cache. They are kept in tags: `limina-kk-pre-stall-revert-2026-10-20` (Mesa),
`main-pre-stall-revert-2026-10-20` (virglrs) and `limina-pre-stall-revert-2026-10-20` (libkrun,
the wake probe's per-phase drain split).

### Metal's own shader cache is unmanaged and unexplained
Below Mesa's disk cache, which limina now places and caps per VM (`--gpu-shader-cache`), macOS keeps
a per-user Metal cache under `$(getconf DARWIN_USER_CACHE_DIR)com.apple.metal/` (here two
directories, `16777235_530` and `32024`, each holding `functions*.{data,list}`). limina neither
places, sizes nor clears it, and nothing here says what it keys on. It matters: with Mesa's cache
empty, Loupe's About dialog on a host whose Metal cache already held the GLSL path's pipelines
stalled the worker 263–267 ms. The first three boots on the parked SPIR-V path
(`docs/design/vrend-spirv.md`), which produces different MSL, stalled 3.6 s, 0.56 s and 0.43 s,
nearly all in fences, before settling at 197–200 ms (measured 2026-10-20). So a host that has never run a
workload pays a Metal compile that no test here measures, and every "cold" number taken so far is
cold for Mesa only.

Open questions:
- What the cache keys on: MSL source, pipeline descriptor, GPU, OS build, or the process's
  identity (the worker binary, its signature, its bundle id).
- Its size cap and eviction, and whether a macOS or Xcode update empties it.
- Whether KosmicKrisp's `VkPipelineCache` maps onto `MTLBinaryArchive`, or could. An archive limina
  owns could be placed per VM, like the Mesa cache.
- What an App Store sandbox changes about the cache's location.

Next: read Apple's documentation on `MTLBinaryArchive` and the shader cache, and inspect the cache
directories while a worker compiles. Measure a truly cold boot by pointing the cache somewhere empty,
if anything allows that (an environment variable, a different user, or a sandboxed container). Then
decide whether limina manages it as it does Mesa's, pre-warms it, or only documents it.

### Texture uploads on virgl create a staging resource thousands of times per run
The same runs show 7–8k `RESOURCE_CREATE` per 15 s on the canvas thread, about 100 of them over
1 ms, mostly `virgl_staging_alloc` ← `virgl_resource_transfer_map` ← `st_texture_image_map` on the
`TexSubImage` path (the destination texture is busy, so the write goes to staging). The staging
uploader is meant to suballocate from one ring buffer; this many creates means it is refilling (or
missing the resource cache) per upload. Each create is a host round trip, and the slow ones join
the class above. Next: count staging refills against upload sizes in `virgl_staging.c`, then decide
between a larger staging buffer and keeping the old one cached.

### Decide whether KK should advertise `VK_EXT_vertex_input_dynamic_state`
Throughput, not correctness. Host GL is zink-on-KK, and without this extension zink compiles vertex
input into the pipeline, so every shader × vertex-layout combination is a separate PSO; with it, zink
sets the layout through `CmdSetVertexInputEXT` and collapses the permutations. Check first: Metal
compiles the vertex descriptor into the pipeline state object (`MTLRenderPipelineDescriptor.vertexDescriptor`).
If that still holds under **MTL4**, which this tree encodes with, KK could implement the extension
only by caching PSO variants keyed on vertex input — zink's current job moved one layer down, for no
gain. The answer decides whether the item is worth anything.

---

## KosmicKrisp

### AGX faults on a zeroed ComputeContext during guest texture uploads (cause unknown)
Dogfood `limina-vmm` SIGSEGV on thread `gpu worker`, six times 2026-08-31..09-11 at uptimes from 1.4 h to 2 d 17 h:
guest GL texture upload → zink `zink_copy_image_buffer` → KK `kk_CmdCopyBufferToImage2` (pre_gfx
compute slot) → AGX `prepareForEnqueue+672` (×5) / `blitCDMTextureToTexture+840` (×1), storing
through a NULL `ComputeContext+0x918` pass-state pointer that only `beginComputePass` writes.
Established: the context memory was zeroed wholesale; the KK encoder was its own live incarnation
(encoder guard: 250M checks, 0 bad), so a stale KK encoder pointer is ruled out, as are a pool
segment cap or allocation failure, address space, host RAM, the shared-event log flood and venus
context count. Armed: the context hook on `limina-kk` ("kk: watch the AGX context behind each
compute encoder, not just the encoder", `e6a5ba3cef9`; ivars + pass-state canary at
birth and every op; `[LIMINA-CTX]` report; the op is skipped instead of faulting;
`LIMINA_KK_CTX_CANARY`) plus the encoder guard (`LIMINA_KK_ENC_GUARD`). Because the guard prevents
the crash, silence is not evidence — the oracle is
`grep -E 'LIMINA-ENC|LIMINA-CTX|is stale|refusing to close'` in the worker log, and the dispatch ring
at `<LIMINA_KK_POOL_SNAPSHOT>.dispatch.<pid>`. AGX reusing one ComputeContext across successive
encoders is normal; two live on one is the signal. If the hook names AGX, a Radar is owed. Full
record: `spikes/kk-alloc-pool/RESULTS.md`.

### The kernel logs a shared-event fault continuously while any VM runs
While a `limina-vmm` lives the kernel emits `IOGPUFamily … IOGPUCommandQueue::schedule_shared_event:
Failed to find shared event reference` continuously (thousands per minute under load), stopping the
instant the process exits. Chronic from launch across process instances and not the cause of the AGX
fault above, but scheduling a wait on a shared event the kernel cannot resolve is a defect on the KK
semaphore path (`bridge/mtl_sync.m`, `kk_sync.c` timelines), and each is an unnecessary round trip
per submission. Not investigated.

### Nothing catches a sampler destroyed while submitted work still references it
A `COMBINED_IMAGE_SAMPLER` descriptor carries a 16-bit index into a device-wide sampler table, not a
resource ID. When the last `VkSampler` reference goes, `kk_sampler_heap_remove_locked` releases the
`MTLSamplerState` and `kk_query_table_remove` zeroes the GPU-visible slot and recycles the index. Legal
Vulkan (the app must not destroy in-use samplers), but an app that gets it wrong produces a GPU
address fault arbitrarily later with nothing naming the sampler. Owed: a debug-build check recording
the last submission to reference each slot and asserting when a retirement runs ahead of it (the
dead-resource-ID scan cannot serve: a recycled index looks live). Not the WebGL-MSAA loss (the
`LIMINA_KK_SAMPLER_LEAK` arm still dies; zero retirements during a run).

### Nothing checks that a sampled image's texture type matches the shader's declaration
`kk_descriptor_set` writes an `MTLResourceID` and sampler index; the generated MSL reads it as the
SPIR-V's declared type (`texture2d<float>` for a 2D view). A `VK_IMAGE_VIEW_TYPE_2D` view of a
four-sample image has been seen in a `COMBINED_IMAGE_SAMPLER`; Metal's 2D and 2DMultisample layouts
differ, so such a read misaddresses off a valid base and no descriptor-byte check can see it. Vulkan
forbids the read, so this is a missing debug assertion. It belongs at the draw (set layouts do not
know shader dimensionality): the bound pipeline's declared image dimension against the bound view's
`sample_count_sa`. A debug check for the view half (`LIMINA_KK_ADDR_CHECK`, reporting
`[LIMINA-MSBIND]` from `kk_cmd_draw.c`) exists only on side branches (`parked/fence-sync`,
`wip/keep-zs`), not on `limina-kk`; on those it was not the WebGL-MSAA cause (0 of 24 draws at the
loss).

### The source of a NULL allocation added to the residency set is unknown
`kk_device_add_{heap,buffer,texture}_to_residency_set` used to pass NULL to
`mtl_residency_set_add_allocation`; Metal stores it and the next submit faults in
`-[AGXG13XFamilyResidencySet _commitAddedAllocations:…]` (`KERN_INVALID_ADDRESS at 0x18`) on whichever
ring thread submits (seen once, as a worker SIGSEGV in `synoik_desktop_survives_snapshot_restore`).
All entry points now refuse NULL and the texture path logs the caller. What produced the NULL is not
established (suspect: the three image-view residency call sites in `kk_image_view.c`); the guard has
never fired since. If `[LIMINA-RESIDENCY] refused a NULL texture, called from %p` appears, symbolise
that address — it names the site.

### Transform feedback captures nothing from an indirect draw
KK emulates xfb in the vertex shader (`kk_nir_lower_xfb.c`): while capture is on, `kk_xfb_draw`
reissues each *direct* draw as a non-indexed list of primitive vertices, so the shader can map
`vertex_id` to a primitive and a buffer slot. That needs the vertex count on the CPU. An indirect
draw (`vkCmdDraw*Indirect*`) during capture writes nothing and counts nothing in
`PRIMITIVES_GENERATED`/`_WRITTEN`. GL reaches this through `glDrawArraysIndirect` and friends with
transform feedback active, which piglit's selection does not exercise. Fix: a small compute pass
that reads the indirect arguments and writes the remapped draw's arguments and slot limit before
the draw.

### PRIMITIVES_GENERATED misses a primitive across a pause
`ext_transform_feedback2@counting with pause` reads 2 where 3 primitives were generated. It fails
under zink-on-venus too, so the query is in KK. KK counts generated primitives per draw at
`kk_draw_impl`; which draw the pause/resume pair drops is not established.

### The list-restart unroll can hang the GPU
With KK's list-restart skip turned off (`LIMINA_KK_NOLISTRESTART=0`) on limina-kk `88b1341efe8`, piglit's
`glsl-fs-flat-color` on the vrend tier hung the GPU (`kIOGPUCommandBufferCallbackErrorHang`, device
lost, worker SIGSEGV); the encoders before it alternate a 160x160 render pass and a compute dispatch,
one split per unrolled draw. It passes with the default skip and on the restart-scan stack, where
zink no longer hands KK list restart. The venus tier still reaches the skip or the unroll. Untried on
the pre-graphics/batched branch `limina-kk-pregfx` (`perf/listrestart-2026-10-07/`). A GPU hang is
host-wide: reproduce only on an otherwise idle host.

### A long guest shader trips macOS's GPU watchdog (vrend fixed; venus and the costs below open)
piglit's `ext_timer_query@time-elapsed` doubles the loop count of a slow fragment shader until one
draw takes 0.1 s. macOS's GPU watchdog stops fragment work like that after tens of milliseconds
(`kIOGPUCommandBufferCallbackErrorHang`, one host GPU restart per hang). The same shader hangs bare
Metal with no VM, and the timer query is innocent (`spikes/timer-query-hang/RESULTS.md`). Any guest
shader in that class, such as a heavy WebGL page, reaches it the same way, on either tier. Before
the fix, KK marked the device lost, every GL context in the VM failed, the control queue stalled
for ~20 s and the worker died on signal 11.

The fix keeps the device and stops the offender:
- **KK** moves to a new Metal queue after a hang (macOS bans the queue, not the process). It
  delivers from the CPU the signals a banned queue never fires, and it charges each hang to the
  command pool whose work ran into it.
- **zink** resets the context that caused `ZINK_HANG_LIMIT` hangs; past `ZINK_HANG_BUDGET` for the
  screen, it resets any context that hangs, until `ZINK_HANG_FORGIVE_S` passes without one. The
  worker defaults these to 2, 4 and zink's own 3600.
- **vrend** creates robust contexts and refuses a reset one.
- **zink** no longer crashes on a lost device's descriptor pools.

Measured on the piglit test: 4 GPU restarts with the desktop alive and a clean poweroff, against a
dead VM before.

Where it lives: `limina-kk` (KK and zink, ledger section H), virglrs `49ddd13`, and the worker's
defaults in `crates/limina-vmm/src/krun/mod.rs`.

Still owed:
- **Work already queued still hangs.** A limit of N costs about N + 2 restarts, because the
  offender's queued work runs after its reset. Each hang also stalls the virtio-gpu control queue
  for the ~120 ms the watchdog takes.
- **The refused app is not told.** The guest virgl driver has no reset-status path, so it sees
  failed submits and a dead canvas, not `GL_GUILTY_CONTEXT_RESET`.
- **Venus is not covered.** A venus context's hang reaches KK the same way, but nothing charges
  it to a guest context or refuses it yet.

### Geometry shaders: the failures left on the piglit GS list
KK runs geometry shaders on poly's compute emulation, on by default (`LIMINA_KK_GEOMETRY_SHADER=0`
withdraws them). Under zink-on-venus the GS list (no fp64) passes 2501 of 2622; what still fails,
each deferred by choice when GS landed:
- **Vertex streams > 0** (`gs-stream-location-aliasing`, `stream-different-zero-gs-fs`): KK reports
  one vertex stream. The same limit makes virglrs withdraw `transform_feedback3`.
- **`clip-distance-{bulk,itemized}-copy`**: precision only. The interpolated error reaches 1.2e-6
  against the test's absolute 1e-6 on values near 11, about 1 ulp.
- **`point-size-out`, `redeclare-pervertex-out-subset-gs`**: a guest zink bug, not KK.
  `delete_psiz_store` drops 1.0 point-size stores in a GS that emits more than once; it should drop
  them only when every store is constant 1.0 (a `limina-guest` change, and an upstream one).
- **`fbo-cubemap-array`** fails on the venus tier only, with GS off as well: every layer reads layer
  0. The host stack and the vrend tier pass (`spikes/kk-gs/cube-array-layers.c`).
- **`tes-primitiveid`** counts 24 invocations where 16 are expected: quads drawn as non-indexed
  triangles.
- **`tes-gs-max-output -small -scan 1 50`** times out on both tiers.
- `arb_gl_spirv` failures are the guest lacking `spirv-as`, not a driver result.
Latent, no test reaches it yet: `nir_to_msl.c`'s `load_output` builds its mask with a 32-bit
`1 << location`. A fix ("kk: test a load_output's location against a 64-bit mask") and a poly change
that computes each GS input vertex index once ("poly: compute each geometry shader input vertex
index once", GS MSL about 3.7× smaller, no behaviour change) are not on `limina-kk`: each sits on a
local side branch of its own (`limina-kk-output-mask`, `limina-kk-gs-vertex-hoist`) that no remote
carries. The hoist needs the GS piglit lists first: host probes do not cover adjacency or line
inputs.

### Three commits on `kk-hardening` are unreachable from the pin
`kk-hardening` in `/Volumes/mesa-cs/mesa` is a local branch no remote carries, on an older base than
`limina-kk` (the pin, `059715d2aa5`): `git log limina-kk..kk-hardening` lists 105 commits. All but
three have a counterpart on `limina-kk`, by patch id or, for ten rebased ones, by subject. The three
that do not:
- "zink: fix lost-wakeup deadlock in the multi-context unflushed-batch wait" (`a9a8381b7ea`);
- "zink: re-look-up the pipeline when vertex elements change and vertex input is static"
  (`abe1407d4b3`);
- "kk/query: resolve 64-bit timestamp copies straight to the destination" (`ba52f87b8ab`). It
  drops a compute dispatch but fixes nothing: the timer-query hang it was written for is the GPU
  watchdog (above).

Next: decide each on its merits, carry what is wanted onto `limina-kk` (and its ledger row), and
push or delete the branch so nothing lives only on this disk.

---

## Metal transport

### A guest process can forge its own fence completion
The worker's fence watch (`crates/limina-vmm/src/krun/metal_ext.rs`, `Fences::watch`) reads
`completed` and `error` from the ring's control block, which sits in an arena the guest maps
writable. A process that writes `completed = u64::MAX` signals every pending fence of its context at
once, including the implicit write fences the FENCE ioctl adds to shared dma-bufs, while the GPU is
still writing: a compositor then reads a half-drawn buffer. Guest-internal integrity only; the host
is not at risk. Fix shape: the replayer knows completion host-side (the commit-feedback block in
`crates/limina-gpu-host/src/exec.rs`); report it through a channel the guest cannot map (a
host-only page or the IPC) and never read it back from shared pages.

### One process can still fill the control queue across several files
`virtio-gpu-limina` refuses a file's FENCE with EBUSY past 64 waiting (`metal_fence_flood` covers
one file), but a process that opens several render-node files gets 64 each, and the queue holds 256
descriptors. Fix shape: a device-wide bound beside the per-file one, which needs a counter on
`virtio_gpu_device` (the `0001` hook patch).

### Reserved fields go unchecked, and the ioctl interface has no version
The ring protocol is versioned: `limina_gpu_wire::VERSION` (5) is written by the producer and
checked when the host attaches (`crates/limina-gpu-wire/src/ring.rs`, `Consumer::attach`), and
`DRM_VIRTGPU_LIMINA_INFO` reports it to the guest driver. The guest module's ioctl structs
(`guest/virtio-gpu-limina/virtgpu_limina_drm.h`) carry no version. Within the ring, nothing reads
`CmdHeader.flags`, no `_pad` field is checked, and a payload longer than its command and its
declared tails is accepted, its extra bytes dropped, although `Error::BadPayload`'s doc covers
"trailing bytes that do not add up". The guest module checks `pad` only in the FENCE ioctl.
Anything an old host ignores can never be given a meaning later. Fix: reject non-zero reserved
values and stray trailing bytes now, while no guest sends any, and give the ioctl interface a
version.

### A guest hands Apple's library loader and MSL compiler arbitrary bytes
`CREATE_LIBRARY_DATA` (`crates/limina-gpu-host/src/exec.rs`, `create_library_data`) passes up to
`MAX_LIBRARY_DATA` (256 MiB) of guest-written bytes to `newLibraryWithData:`, a parser of Apple's
whose robustness against malformed input nobody here has measured. `CREATE_LIBRARY`
(`create_library`) is the same trust problem one layer up: guest MSL source handed to the Metal 4
compiler. The bytes are copied out of the arena before parsing, so the guest cannot change them
mid-parse, and a failure fails the reply only. A crash in either takes the GPU host process, and
every context of the VM with it; `limina-gpu-host` runs unsandboxed (unlike `limina-netd`, which
calls `sandbox_init`), and no fuzz target reaches either path. Next: fuzz both with mutated
metallibs and MSL in a throwaway process before anything untrusted runs them, and sandbox the GPU
host.

### `LIMINA_GPU_METAL=0` turns the transport on
`metal_ext::start` (`crates/limina-vmm/src/krun/metal_ext.rs`) tests only whether the variable is
set (`var_os(..).is_none()`), so any value, `0` included, starts the GPU host. Fix: read the
value, not its presence.

### The GPU host has no resource budget
Each arena is capped at 4 GiB (`MAX_ARENA`, `crates/limina-gpu-host/src/metal.rs`), but neither
the number of arenas a context creates nor the number of objects is bounded. Arenas shared through
a blob resource are not placed in the 16 GiB arena window (libkrun `devices/src/virtio/gpu/ext.rs`,
the `ARENA_FLAG_BLOB` arm of arena creation), so the window does not bound them either. Every object
also keeps a copy of its creation payload for restore (`Record.payload` in `crates/limina-gpu-host/src/journal.rs`), and a
`CREATE_LIBRARY_DATA` is filed with its bytes inline, up to 256 MiB per library. One guest process
can therefore grow the GPU host without limit. Fix: a per-context budget of arenas, objects and
recorded bytes, refused with `OutOfMemory` like an allocation failure.

### Nothing fuzzes the Metal transport
The fuzz workspace (`fuzz/`) covers the control frame, CTAP2 and the vdagent stream; no target
reaches the command ring's consumer, the executor's decoders or the library loader, though the ring
and every command come from a guest. Next: a ring-consumer target and an executor-decode target
(no Metal needed for the decode half), then the loader (previous entries).

### Lead, unconfirmed: the GPU host may inherit the worker's descriptors
`GpuHost::spawn` starts the GPU host with `posix_spawn` and no file actions or attributes
(`crates/limina-gpu-host/src/client.rs`), so every worker descriptor not marked close-on-exec
crosses into it. Whether any does is not checked. Next: list the GPU host's open descriptors on a
running VM (`lsof -p`); if any are the worker's, spawn with `POSIX_SPAWN_CLOEXEC_DEFAULT` and pass
only what it needs.

### Fence order within a file is only a userspace promise
All of a file's fences share one dma-fence context (`lf->fence_ctx` in
`guest/virtio-gpu-limina/virtgpu_limina.c`), and the fence event path signals every lower id on a
context, so FENCE(10) then FENCE(5) means answering 5 signals 10. Guest-internal. Fix: refuse a
seqno below the file's last in the ioctl. Related: a LIMINA_FENCE without `INFO_RING_IDX` would
collide with a rutabaga context on the same file fencing ring 0; refusing it makes that explicit.

### Surface and arena imports drop the object before the host call
The import ioctls in `virtgpu_limina.c` read `hw_res_handle`, then `drm_gem_object_put` it before
`limina_call`. A concurrent GEM_CLOSE plus another process's blob create can reuse the id, since the
ida hands out the lowest free one. Exploitability low. Fix: hold the object across the host call.

### Exported resource ids have three owners
`ExtState::export_arena` (libkrun `devices/src/virtio/gpu/ext.rs`) checks only its own exports, not
rutabaga's resources. An extension blob under a live rutabaga id flips `is_ext_blob`, so that
rutabaga resource is never freed. Only a guest kernel can collide ids, so this is self-harm. Fix:
one owner, with exported arenas filed as rutabaga resources.

### The GPU host gives up on recoverable Mach errors
`crates/limina-gpu-host/src/mach.rs` `reply` forgets the send-once right before `mach_msg`, so a
failed send leaks it, and `server.rs` exits on any receive error, `MACH_RCV_TOO_LARGE` included,
which ends every context of the VM. Fix: treat both as recoverable, destroying the message.

### Restore's dependence on Metal's allocator is measured on one host
Restore reproduces Metal resource IDs and GPU addresses by relying on allocator behaviour measured on
an M1 under macOS 26.6 (first fit, the 32 KiB granule and 16 KiB guard, slot reuse within 100
tries). Each value is verified, so a macOS change fails safe, but nothing says so before users hit
it. The two tests that measure it, `are_host_values_reproducible_across_gpu_hosts`
(`crates/limina-gpu-host/tests/first_light.rs`) and `footprints_of_small_objects`
(`crates/limina-gpu-host/src/metal.rs`), are `#[ignore]` and so outside the suite. Next: run them
on every host OS we ship to, as a gate.

### Every context of a VM shares one GPU host process
Cross-context isolation rests on Metal 4 residency sets, which Apple documents as paging control,
not protection, and guest GPU addresses are forwarded unchecked. Either verify that a non-resident
heap in the same process faults, or document every KK client of a VM as one trust domain.

### Typed ids and a Metal wrapper module
Every id is a bare integer (ctx, arena, export, surface, doorbell, seqno, Mach port), with
`(u32, u32)` keys and ctx 0 meaning "orphan"; newtypes and an `ArenaOwner` enum would turn missed
checks into compile errors. Surface send rights are flattened to `u32` (`into_raw`) two layers early
and leak when a bind is refused; moving a `SurfacePort` would close that. `Result<_, String>` crosses
the `GpuExtension` trait. `exec.rs` mixes 43 `unsafe` blocks into its handlers because objc2
marks the Metal setters unsafe; a thin typed wrapper, like virglrs's `vulkan.rs`, would separate the
validator from the FFI.

### The command ring has its own copy of `limina-ring`'s handshake

`limina-ring` (`crates/limina-ring`) is the protocol-free core of `limina-gpu-wire`'s command ring:
the control block's single-writer lines, the park/publish/recheck handshake, the position checks.
The Metal transport keeps its own copy, because its framing, `completed`/`error` fields and
`VERSION` are frozen by the generated C header and the guest driver. The two have diverged:
`limina-ring`'s consumer may attach after the producer has published (`ef686860`, for netd, whose
guest can send before netd attaches), while the wire ring's `Consumer::attach` still refuses a ring
whose `tail` is not 0 (`crates/limina-gpu-wire/src/ring.rs`). Moving it onto the core is a
layout-preserving refactor (the `c_header` test fails on any offset drift), for a quiet moment in
the transport, and it has to settle which attach rule the transport wants; until then a fix to
either handshake belongs in both.

## Video

### Stock-tier Firefox never gets hardware decode (virgl offers I420/YV12 as decode targets)
`virgl_is_video_format_supported` ignores profile/entrypoint and answers with the generic sampling
check, so vanilla mesa advertises NV12, YV12 and IYUV for decode. ffmpeg's
`vaapi_decode_find_best_format` scores exact `sw_pix_fmt` matches at `INT_MAX`; for 8-bit 4:2:0 the
three-plane formats tie above NV12 and the last advertised (IYUV) wins. Firefox's DMABUF path accepts
only NV12/P010, logs `Unsupported VA-API surface format 808596553` (I420), destroys its frame pool and
decodes in software on every stream, codec-independent. Chrome and GStreamer choose NV12 themselves;
I420 decode itself is correct (framemd5 bit-identical to software), which is why
`ffmpeg -hwaccel vaapi` hides it. The enhanced tier carries the fix ("virgl: do not offer
three-plane 4:2:0 as a decode target" on `limina-guest` withholds YV12/IYUV for
`PIPE_VIDEO_ENTRYPOINT_BITSTREAM`). Owed: send it upstream so stock Firefox gets the hardware path —
stock images load vanilla mesa, and libva probes `/usr/lib64/dri-freeworld/` ahead of
`/usr/lib64/dri/`, so RPM Fusion's unpatched `mesa-va-drivers-freeworld` wins even where ours is
installed. Its upstream form is ready on `upstream/guest-2026-10`; the ledger
(`docs/upstreaming/ledger/mesa.md`) has it `upstream-now` with no issue or MR yet, and no upstream
suite test, since no stock host enables virgl video decode, so the case rests on the code and
ffmpeg's exact-match selection. The durable fix is a virgl wire query for per-codec decode-target
formats (the protocol has none). Unchecked: other VA-DMABUF consumers (GStreamer `vaapisink`,
Chromium's other paths). Stock-tier validation vehicles: Chrome and GStreamer
(`docs/design/h264-hevc-decode.md`).

### Decode targets on the stock tier are one-page stubs; the remaining zero-copy work
On vanilla mesa an exported VA decode surface's dmabuf is a 4096-byte stub at every resolution
(`spikes/va-dmabuf-size`). The enhanced tier gives decode targets real guest memory ("virgl: give
video decode targets real guest memory" and "virgl: allocate a decode target as one composite
planar resource" on `limina-guest`) and carries no refusal to export a too-small dmabuf, so Firefox
has its hardware decoder there.
Still owed, owned by `docs/design/blob-decode-targets.md` §Phases: glupload's direct importers refuse
everything (`DirectDmabufExternal … cannot produce texture-target 2D`) and fall back to the copy
uploader; the guest/host layout contract (the guest computes tight strides, the IOSurface is
Metal-aligned, and the host writeback copy reconciles them) must land before the copy can go; and the
stock tier, reachable only by upstreaming.

### Guest virgl gates the composite decode-target create at the caller, not at the emitting site
"virgl: allocate a decode target as one composite planar resource" on `limina-guest` gates the
composite shape on the sampler bitmask (`is_format_supported(buffer_format,
PIPE_BIND_SAMPLER_VIEW)`) in `virgl_video_create_buffer`. The site that emits the planar create,
`virgl_resource_create_front` (`virgl_resource.c` on `limina-guest`), checks only
`VIRGL_RESOURCE_FLAG_VIDEO_TARGET` && `VIDEO_PLANAR_TARGET` && more than one plane. Unreachable today
only because the video path is the one caller that sets `VIRGL_RESOURCE_FLAG_VIDEO_TARGET`. Any new
caller, or a relaxed video gate, brings back what the gate prevents in its worst form: the kernel
has already handed out the handle, the host's refusal is invisible, and the context goes to `Illegal resource`
for the rest of its life, silently dropping every later submission. Fix: move (or duplicate) the
sampler lookup into `virgl_resource_create_front` on `limina-guest`, re-export, bump the mesa RPM
release, redeliver.

### Hardware decode: what is still synchronous
VA-API decodes run on a thread per codec and every read of a target waits for its picture
(`docs/design/async-video-decode.md`). What remains:
- **A target lent to a venus context decodes synchronously.** A Vulkan read passes no host barrier,
  so a composite target whose surface was ever imported into venus keeps the old stall. Lifting it
  needs the guest to fence END_FRAME (mesa-guest, design phase 3) and the host to trust that per
  codec (phase 4). The same guest fence closes the existing race for a decode already queued when
  the lend happens.
- **A per-plane target with only some planes backed by surfaces still uploads synchronously.** A
  per-plane target whose planes are all surfaces has each plane's rows written on the decode thread
  (virglrs `bcfe1ad`, "vrend: write a surface-backed video buffer's planes on the decode thread"), so the stock tier's decodes stay asynchronous; one with only some of its
  planes as surfaces keeps the `glTexSubImage2D` on the render thread when a reader settles it.
- **A playback's first frames still wait a few ms each for room in the queue.** gst-va submits
  frames ahead at start-up, and 4-8 decodes wait for a four-deep queue, ~4 ms each and 8-10 ms in
  all. A deeper queue would remove it at the cost of more undelivered pictures holding the
  decoder's pool.
- **Under the macOS Game Mode clamp the decode thread falls behind.** Measured 2026-09-23: Firefox's
  VP9 playback went from 0.03 ms to 2-15 ms a frame in END_FRAME (worst 327 ms) with the whole
  worker starved. `VIRGLRS_SUBMIT_STATS` now counts the two waits that can cost END_FRAME -- a full
  decode queue, and a decode into a target whose previous picture has not landed -- on their own
  `vrend video:` lines, so the next clamped run can split them from plain CPU denial.

### One context's decode holds back every other context's fences
Classic fences retire through a single waiter thread in FIFO order (`third_party/virglrs/src/vrend/waiter.rs`),
and a fence taken while its context decodes waits there for the picture to land, so every fence
queued behind it waits too. **The order is the guest's, not the waiter's:** stock guest Mesa's virgl
winsys never sets `VIRTGPU_EXECBUF_RING_IDX`, so every classic fence from every process sits on the
device-wide dma-fence context, with one id sequence interleaved across contexts, and the guest kernel
signals every older fence on that context when a newer one is delivered
(`virtio_gpu_fence_event_process`). Retiring gnome-shell's fence ahead of a decoding context's older
one would signal the decoder's early. No host-side reordering is correct.

What it costs, bounded by one decode (VideoToolbox time plus the plane write), measured at real speed
(`spikes/flush-latency/RESULTS.md`): about 2.2 ms a decode at 720p, 3.0 at 1080p and 6.3 at 4K VP9.
gnome-shell's fenced submits keep their ~2.4 ms median at every size; their p95 goes from ~4 ms to
7-9 ms at 4K (synchronous decode: 10.6). The plane write is ~11% of a 4K decode and time queued ~8%,
so neither faster copies nor a higher-priority decode thread would move this measurably.

The only way out is a timeline per context: the virgl winsys asks for one ring at context init and
submits with `RING_IDX`, after which the host can order classic context fences per context rather
than globally. That is a mesa-guest change for the enhanced tier (stock guests keep the shared
timeline, correct and slower), and it would ride the same delivery as the END_FRAME fence of the
async-decode design's phase 3.

### mpv's VA-API path cannot render on venus
`mpv --hwdec=vaapi` loads the driver but libplacebo's dmabuf interop fails probing surface formats —
`vk->MapMemory(...): VK_ERROR_MEMORY_MAP_FAILED (../src/vulkan/malloc.c:973)` — the `vo/gpu` load is
abandoned and mpv falls back to software (measured 2026-09-03, F44 enhanced guest, mpv 0.41.0). Firefox's VA-API path is
unaffected, so it is libplacebo's map of venus memory, not decode. It matters because mpv is the
easiest source of objective A/V-sync and dropped-frame numbers, which on this tier currently describe
only software decode.

---

## Audio

### PipeWire ignores the device latency virtio-snd reports, so players mis-sync
The device reports the host DAC's remaining latency in `virtio_snd_pcm_status.latency_bytes` and the
guest kernel turns it into `runtime->delay` correctly, but spa's `alsa-pcm.c` never calls
`snd_pcm_delay`: `get_status()` derives delay as `buffer_frames - avail` (= `appl_ptr - hw_ptr`), so
the sink's `Latency` is its own period (512 frames) and nothing else. Measured on F44 / PipeWire
1.6.2, `paplay --latency-msec=50`, A/B'd in one boot with `LIMINA_SND_ZERO_LATENCY=1`: device
reporting 1346 frames → `pa_stream_get_latency` 78,190 µs; reporting 0 → 78,267 µs. The result is a
steady lipsync error equal to the device latency (28 ms on built-in speakers, hundreds of ms on
Bluetooth). The decoder is ruled out (same offset with VA-API disabled). Options: (a) patch PipeWire so
`get_status()` adds `snd_pcm_delay`'s excess over `buffer_frames - avail`, ship it as an enhanced-tier
component and upstream it (correct for every virtio-snd guest); (b) for stock guests, complete tx
descriptors only when frames are audible so the latency shows in `appl_ptr - hw_ptr` — bounded by the
8192-frame (170 ms) guest buffer, so only a partial correction that cannot cover ~200 ms Bluetooth
without underruns.

---

## Clipboard & agents

### Automated coverage gaps in the session helper
`limina-agent-session`'s ext-data-control backend (`guest/limina-agent-session/src/wayland_clip.rs`)
is verified live only: `l1_session_helper.rs` exercises the RemoteDesktop path and
`l2_clipboard_vdagent.rs` deliberately stops the helper. Nothing automated covers helper reconnect
after a supervisor restart or after the D-Bus session dies. (Per-peer serials and stale-offer
rejection are covered by `l1_clipboard_multi_session.rs`.) The seat gate in `layout_gate.rs` is
unit-tested through `poll` with the seat state passed in, and `seat_owner_is` on sample seat
files; `SeatWatch` (the inotify watch) and `seat_active` (the live read of
`/run/systemd/seats/seat0`) have no direct test, and no L1 runs a seatless session beside a fronted
one.

### A stock guest's idle inhibitors never reach the host
Measured 2026-10-03 on a stock F44 guest, SELinux Enforcing: Firefox playing a video registers two
gnome-session inhibitors ("Playing video", "Playing audio", flags 8 = idle), and they exist only in
`org.gnome.SessionManager`'s `InhibitedActions` on the user's session bus. gnome-session forwards
only logout → `shutdown` and suspend → `sleep` to logind (`gsm_systemd_set_inhibitors` in
`gnome-session/gsm-systemd.c`), so `systemd-inhibit --list` does not change. `qemu-ga` runs as
`virt_qemu_ga_t`, which can read logind over the system bus but is denied every route to a session
bus (the socket write, `runuser`'s setgid, and the transient unit `--machine=user@.host` needs). So
the stock tier keeps the display awake on the audio + hardware-decode heuristic alone, which misses
software-decoded video and silent video. The enhanced tier relays the real inhibitor. Ways to
close the gap, none started:
- Upstream: have gnome-session forward idle inhibitors to logind. What stands in the way is that
  logind's `idle` lock also blocks automatic suspend; systemd #29129 (split `idle` into a power
  part and a screen-lock part) and #41982 (a session-lock inhibitor) are open with no PR.
- A QGA poll of `InhibitedActions` where the agent is unconfined (AppArmor guests such as Ubuntu).
  It costs one `guest-exec` process per poll and shares the port with the clock tick.
- logind's session `IdleHint`, which QGA can read, does honour idle inhibitors, but it only turns
  true after GNOME's `idle-delay`, and never when screen blanking is off (`idle-delay=0`).

---

## Networking

### A killed worker leaves its net socket file behind
The unixgram net backend's local `krun-net-<pid>-N.sock` in `$TMPDIR` is removed when the backend
drops, when an open fails, and on the VMM's exit (`Vmm::stop` runs an exit observer before `_exit`).
A worker that crashes or is killed still leaves its file. Do not sweep by the embedded pid — pids
are recycled; a safe sweep needs proof the owner is gone (a lock the worker holds, say).

### vmnet interfaces leak until the host refuses new ones
InternetSharing's own leak, triggered when two vmnet clients claim one subnet. Measured 2026-10-22
on macOS 27.0.1: a limina VM held a shared interface on 192.168.64.0/24, and Apple's `container`
started a VM on its `default` network, also 192.168.64.0/24. InternetSharing logged `network …
doesn't belong to client` and `mis_bridge_create: … already exists, deleting`, destroyed the bridge
the limina VM was on, and reset that network to idle. From then on every vmnet interface failed to
join the bridge (`BRDGADD: failed Device not configured`), and none was cleaned up when its process
exited (`mis_network_cleanup_bridge: interface … does not belong to network`). They accumulated
(~250) until `vmnet_start_interface` returned `VMNET_FAILURE` (1001) for every caller.
`sudo killall InternetSharing` (SIP refuses a `launchctl kickstart`) clears them; a later
`VMNET_FAILURE` with no `vmenet` interfaces left is the same wedged state.

limina's own exits do not leak: macOS reclaims an interface when its process dies, however it dies
(a 50-minute suite run, ~200 networked VMs, never held more `vmenet` interfaces than live workers).
limina now pins vmnet shared to 192.168.223.0/24 (`SHARED_SUBNET` in `crates/limina-vmm/src/krun/mod.rs`),
so it no longer shares a subnet with `container`. The wedge needs InternetSharing to hand out a
subnet another client's network still claims; it did not reproduce on demand, so
`net_vmnet_container.rs` (opt-in, `LIMINA_CONTAINER_COLLISION_TEST=1`) is a guard, not a
reproducer. Owed: a Feedback to Apple with the log sequence above. Not limina's leak, but still
owed in libkrun's `vmnet.rs`: a start that answers after its 10 s timeout, or that fails while
handing back an interface, is never stopped.

### An idle guest reads virtio-net `InterruptStatus` about 2,400 times a second
Measured 2026-08-27 on a stock F44 guest at a settled idle desktop with `--net`: 72,374 MMIO reads of
`0xa01f060` in 30 s — offset `0x060` (`InterruptStatus`) on `a01f000.virtio_mmio` → `virtio_net`.
virtio-blk (`0xa01d060`) was a distant second at 2,896; every other device was in the tens. MMIO
writes are not logged, so the true exit count is higher. Unknown whether this is gvproxy's normal
traffic, an ack pattern costing an extra read per event, or a genuine interrupt storm. Start by
rerunning the count with `--no-net`, and against a guest with no NAT traffic.

## TPM

### Debian's arm64 kernel has no driver for our TPM
Our TPM is a memory-mapped TIS on both machines: `tcg,tpm-tis-mmio` in the device tree, and on the
ACPI machine (the default) `MSFT0101` with a TPM2 table naming start method 6, TIS (libkrun
`devices/src/acpi.rs`, `build_tpm2`). Debian testing's `linux-image-*-arm64` (seen on
`7.1.13+deb14`) builds `CONFIG_TCG_TIS_CORE=m` but leaves `CONFIG_TCG_TIS` unset, so `tpm_tis`,
which binds both, is absent: the device appears, `/dev/tpm0` never does. The firmware still
measures the boot and hands the event log to Linux, so systemd waits for `/dev/tpm0` and
`/dev/tpmrm0` until its 90 s timeout, then boots without a TPM. That is the degradation every stock
guest without `tpm_tis` gets from `tpm = true`. Fedora builds every driver below; Debian's options,
from its packaging configs (`debian/config/config` and `arm64/config`, to be re-read on a booted
kernel before building on any of them):

- **ACPI with a CRB TPM: the route on the default machine.** Both kernels build `tpm_crb` in, and
  ACPI already describes the whole platform, so what is missing is the interface: a CRB register
  file beside the TIS one in front of janus, a TPM2 table with the CRB start method, and CRB in
  Cardea, which speaks only the MMIO TIS for measured boot (`docs/design/firmware-and-acpi.md`,
  *TPM on Debian*, where CRB is left optional).
- **TPM on SPI behind an emulated SoC SPI controller: the device-tree fallback.** Debian has
  `TCG_TIS_SPI=m`, the generic driver (`tcg,tpm_tis-spi`), and builds no `SPI_VIRTIO` and no `SPI_PL022`, but Debian and
  Fedora both build `SPI_BCM2835`, `SPI_OMAP24XX`, `SPI_IMX`, `SPI_SUN6I`, `SPI_MESON_SPICC` and
  `SPI_TEGRA114`; BCM2835's is the simplest register model. TCG's SPI protocol carries the same
  TIS register accesses, so janus and the TIS register file stay as they are; the new parts are
  the controller (FIFO, chip select, a fixed clock node) and an interface switch: Cardea speaks
  only the MMIO TIS (no SPI) and passes the device tree through unmodified, so the firmware
  would keep the MMIO TIS for measured boot and disabling that node before the OS sees the tree
  would be new Cardea work, or a kernel with both drivers binds two TPM devices to one TPM.
  A few days of work and a second OS-facing interface to test, for the device-tree machine only.
- **I2C: not viable.** Debian has no generic `TCG_TIS_I2C`, only `TCG_TIS_I2C_CR50` (expects
  Google's cr50 vendor ID: an impersonation) and `TCG_TIS_I2C_INFINEON` (believed TPM 1.2 parts
  only), and no `I2C_VIRTIO`, so it also needs an emulated I2C controller.

Until one lands, a TPM on a Debian guest costs 90 s per boot and gives nothing; leave it off.

### The TPM's state file is synced on the vCPU that sent the command
The TIS device runs each command inside the guest's `tpmGo` write, and the janus backend writes
the state file (`sync_all`, rename, directory sync) before it answers, all on that vCPU's thread.
A slow disk stalls the vCPU for as long as the sync takes. One L2 reboot stalled once (the guest
never came back) and never again in 22 runs; this is the candidate, not a finding. Moving the
write off the vCPU would need the device to answer before the state is durable, which the
backend's "no write the guest saw succeed is lost" rule forbids, so measure the sync's latency
under load before changing anything.

---

## Guest images & delivery

### The stock guest's i2c-virtio driver can panic at switch-root (host mitigated, guest unfixed)
The driver's interruptible wait frees in-flight requests. If systemd SIGKILLs the initrd's udev
while it reads the SBS battery's sysfs, the completion interrupt then calls `complete()` on freed
memory and the kernel panics. That happened in 2 of 53 stock boots, and in none of 60 without the
battery device (`spikes/snd-boot-stall/RESULTS.md`). The host mitigation handles the virtio-i2c
kick on the vCPU that made it, from a cached battery snapshot. That shrinks the window to interrupt
delivery but does not close it: with it, 0 stalls in 100 boots (measured 2026-09-23; at the
earlier 2 in 53 a clean 100 happens by chance about 2% of the time). The ACPI machine keeps the
battery on virtio-i2c too (`BAT0` under the adapter, libkrun `devices/src/acpi.rs`), so both
machines carry the window. Owed: carry one of the posted guest fixes on our kernel fork's
`limina` branch for the enhanced tier (the kref or virtqueue-reset patch, not the uninterruptible
wait). The stock tier waits for Fedora to ship a fixed kernel.

### Guest tools cannot be installed from the app alone
The enhanced payload (kernel + mesa RPMs + agents + installer) is not bundled, and a second Mac has no
RPMs and no toolchain. The qga bootstrap kit delivers limina-agent through the stock guest agent, but
only on an unconfined agent — F44 Enforcing keeps SSH as its delivery path. Designed, not built
(`docs/design/distribution.md` §5): a versioned `limina-guest-tools-<ver>-<distro>.tar.zst` with a
manifest, and `limina install-guest-tools [<vm>]`, which downloads or takes `--payload`, verifies,
stages into an ephemeral read-only `--share`, and runs the installer through the agent or prints the
one-line `sudo` command. The installed `tools_version` goes into the VM definition and is re-checked on
connect: a mismatch prompts, never refuses to boot.

### `deliver-payload.sh` verifies only the agent binaries
After the installer runs, `scripts/provision/deliver-payload.sh` checks that the installed
`limina-agent` and `limina-agent-session` hash to the payload's, and fails the delivery if not. It
does not check the kernel or the mesa it installed: the installer's kernel-default lines are
printed for a human to read, and nothing compares `rpm -q` or `uname -r` against the payload. A
delivery whose mesa or kernel did not take reports success. Fix: verify every component the payload
carries, from the payload manifest the next entry calls for.

### The guest Mesa pin is not what any delivered image runs
`third_party/manifest.toml` pins `[mesa-guest]` at `0642e0178` on base `mesa-26.2.5`, and
`patches/mesa-guest/` is exported from it. Every enhanced image and the current payload (r38) run mesa `26.2.3-6.limina`, built at `14952457e03` on `mesa-26.2.3`
(`docs/images.md` §Component versions), which the checkout at `/Volumes/mesa-cs/mesa-guest` is still on. Neither rev
is an ancestor of the other. So a rebuild from the pin ships a series no image has run, and
`patches/mesa-guest/` describes no delivered build. Next: build and deliver the pin, or move the pin
back to the delivered rev until that happens.

### The installer does not check the payload against the guest release
`scripts/provision/install-enhanced.sh` checks that the mesa RPMs share one NEVRA but never that the
payload was built for the booted guest's Fedora release; a payload built for another release installs
and can break the desktop silently. Add a payload manifest (target `/etc/os-release`
`ID`/`VERSION_ID`, component versions, per-file sha256) and refuse on mismatch — the same manifest
`docs/design/distribution.md` §5 calls for.

### No Parallels import helper
`vmlib/import.rs` clones or references an existing raw disk; nothing converts a Parallels VM (merge
snapshots, `qemu-img convert -f parallels` to raw, regenerate the initramfs with `virtio_mmio` rather
than `virtio_pci`, rewrite `/dev/sdX` fstab entries, add `console=` GRUB args, remove Parallels Tools).
The runbook `docs/dogfooding-parallels-migration.md` documents it; a guided `import` helper would
de-risk the `virtio_mmio` trap.

### v1-space-cache btrfs conversion never validated end to end
A 16 KiB-page kernel cannot mount a btrfs still on the v1 free-space cache (`open_ctree failed: -22`);
only migrated or old installs have v1, and stock 4k kernels mount it. `ensure_btrfs_free_space_tree`
in `install-enhanced.sh` sets `space_cache=v2` on every btrfs fstab line, builds the tree live
(`remount,clear_cache,space_cache=v2`), verifies it, and arms the 16k one-shot only once the tree
exists (otherwise `limina-arm-16k.service` arms it after a stock boot builds the tree). Only the awk
and `bash -n` have been checked. Validate on a real v1-btrfs guest: `btrfs inspect-internal
dump-super -f <dev>` should show `compat_ro` `0x3` afterwards, and the 16k kernel should mount root.

---

## Build & tooling

### `LIMINA_HOST_GALLIUM` no longer swaps the worker's host Mesa
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh` with `MESA_PREFIX=<prefix> LIMINA_HOST_GALLIUM=1`
puts `<prefix>/lib` on `DYLD_LIBRARY_PATH`, and the worker does map that prefix's `libEGL` and
`libgallium`, but virglrs's `eglInitialize` then fails (`egl: failed to create dri2 screen`) and
the GPU degrades to software-2D. Measured 2026-10-07 with the shared `zink-kk-prefix` itself as
`MESA_PREFIX`, so it is the mechanism, not a build; the same libraries and `DYLD_LIBRARY_PATH`
initialise fine in a host process (`spikes/gles32-gate/es32gate`). Until it is fixed, A/B a host
Mesa change with a runtime switch in one build (as `LIMINA_KK_NOLISTRESTART` and
`LIMINA_ZINK_MVK_WORKAROUNDS` do). Next: an `EGL_LOG_LEVEL=debug` worker run (it printed nothing
extra once, so check the variable reaches the worker first).

### Fold `xtask bundle` into `xtask app`
`bundle` (writes `target/Limina-smoke.app`, debug, ad-hoc) no longer earns a second command: `app`'s
assemble + sign + dmg phase measured ~25 s (2026-08-31), the rest is the cargo build, which `cargo xtask app --debug`
avoids, and `build-app.sh` already supports `LIMINA_ALLOW_ADHOC=1` / `LIMINA_SIGN_IDENTITY=-` and
`LIMINA_NO_TIMESTAMP=1`. What `bundle` still has: `--open` (a LaunchServices launch booting the L1
`limina.hold` guest — the Dock-launch path where launchd's 256-fd limit bit) and independence from
`/Volumes/mesa-cs` (`app` sources the KK/zink dylibs from it). The two also sign different
identities: `bundle` writes `CFBundleIdentifier` `br.dev.kov.limina` (`xtask/src/main.rs`), `app`
`eti.noronha.limina` (`scripts/build-app.sh`), so a smoke launch does not exercise the shipping
identity. Fix: add `--open` to `app` and delete `bundle`; the references are
`docs/dev-onboarding.md` and `CLAUDE.md`. The module doc in `xtask/src/main.rs` still says `bundle`
assembles `target/Limina.app`.

### Enable and measure the virtiofs DAX window
libkrun supports DAX on macOS: the device builder takes a window size (`set_dax_window_size`,
`src/libkrun/src/api/device_builders.rs`), the VMM creates the shm region from it, and the macOS
passthrough implements `FUSE_SETUPMAPPING` by mapping file ranges into that window through HVF
(`devices/src/virtio/fs/macos/passthrough.rs`, `setupmapping`). limina never sets a window
(`fs_device` in `crates/limina-vmm/src/krun/mod.rs`), so every guest gets plain FUSE read/write.
Owed: enable it, confirm window alignment and `FUSE_SETUPMAPPING` on 16 KiB host pages (the stock-4k
guest separately from the 16k one, `docs/design/16k-page-requirement.md`), settle host↔guest uid
mapping, and measure it against plain FUSE before making it the default.

---

## Tests — flakes & coverage

### `synoik_desktop_survives_snapshot_restore` has an intermittent trigger that was never isolated
Both observed failures gave byte-identical numbers: 36/1000 landmarks moved against a 1% budget, rows
{0:17, 1:19}, colours 233 → 235, confined to rows 0–52 at full width, max channel delta 54, dy = 0 —
the top panel's blurred translucent background restores slightly differently, invisible to the eye
(frames: `spikes/synoik-restore/panelblur-{pre,post,band}.png`). Rates: 1 failure in 3 standalone
runs plus one under a full suite; a 14-run attempt (9 idle, 5 under CPU saturation) produced 0. The
untested axis is VM lifecycle churn and I/O (poke VMs cycling, valgrind in a guest, mesa rebuilds),
which the failing sessions had. Mitigation `ce7ad78`: the body keeps the 1% budget and the panel band
only has to stay a live panel; the `post-restore:` line prints the body/band split, so the next
failure says which side moved. Open: the trigger, and whether the KK plane-index change is involved
(needs N runs per arm).

### Nothing exercises the host half of venus ghost containment
`vkr_ghost_containment.rs` asserts that a refused import leaves the context alive. On an enhanced
image, "venus: allocate dma-buf import memory synchronously" on `limina-guest` makes the refusal
synchronous, so no ghost is created, virglrs's ghost path (`Lookup::Ghost` →
`Dispatched::Ghosted`) never runs, and the test passes on the guest fix alone. The host half
covers stock and older guests. Proving it needs an env-gated, off-by-default fault-injection hook in virglrs that fails the Nth create of a named command
for a context. The one async path left after the guest fix is `vkCreateImage` on a
memory-requirements-cache hit: create the same image key twice (the miss is sync and seeds the cache,
the hit is async), inject on the second, and assert the context survives and the ghost is logged. The
same hook would cover the reply-carrying-ghost poison (`wants_reply` → poison), which has only unit
coverage.

### Fence-accurate present is armed only on windowed boots, so no headless gate reaches a parked classic scanout
`fence_present_policy` (libkrun `virtio_gpu.rs`) defaults on only when `LIMINA_SHOWN_ACK_FD` exists,
and only windowed workers set it. So every headless boot presents synchronously — and headless is what
gets scored automatically (fluster, the replay corpora, `capture.sh`, virglrs's `harness/vm/frame.py`).
`venus_fence_present` and `venus_park_on_busy_reset` force `LIMINA_FENCE_PRESENT=1` and cover the blob
chain; `metal_fence_present` forces it too and checks that a Metal-transport desktop's flushes are
fenced and held (the worker's `flushes are fenced` line), which is the transport's surfaces, not a
classic resource; and the perf battery runs `--window`. What no gate reaches is a parked classic
(vrend) scanout.
Flipping the default for headless is small (with `ack_active` false the cookie leaves `unconfirmed` at
present time and the hold ends after `latch_delay`), but first: the flip also arms venus parking
headless (`try_park_present` checks `fence_present_enabled()` first), changing present timing for the
boots the video corpora and fluster goldens were recorded from — re-verify or re-record those; and
`LIMINA_FENCE_LATCH_MS` defaults to 35, tuned for CoreAnimation, which headless would cap near 28 fps —
pick a delay for the PNG encoder. After the flip: fluster verdicts and replay-corpus scores unchanged,
and the frame oracle reaching the parked path with no knob set.

### `seated_gpu_workload_survives_restore_unchanged` intermittently loses the extension rings on restore
`l2_desktop_restore_landmarks.rs` fails with `gpu restore: extension rings: GPU host did not reply`
(libkrun `virtio/gpu/worker.rs`, the `ext.restore_rings` arm), then `staged snapshot replay failed;
guest session will restart`. Seen in three full-suite runs on different branches (2026-10-18,
2026-10-19 and 2026-10-20, identical lines); the test passed alone straight after each time. The
words are the GPU host client's timeout (`crates/limina-gpu-host/src/mach.rs`, the reply wait), and
every GPU-host call, restore included, shares one flat 10 s deadline (`CALL_TIMEOUT_MS`,
`crates/limina-gpu-host/src/client.rs`). That makes a slow restore under suite load the first
suspect, not an established cause. Open: what the rings wait on when the host does not reply,
whether the reply deadline is sized for a loaded host or the reply is really lost, and a vehicle
that reproduces it (loop the test under suite-like load with the VM held on failure).

### `the_guest_vblank_falls_on_the_host_grid` has single vblanks far off the grid under load
`l2_host_vblank.rs` allows 50 µs (1200 ticks) of phase error on each of 30 vblanks; failing runs have
one outlier while the rest sit at 50–550 ticks: 1732 ticks (2026-10-18) and 3751 ticks (2026-10-19,
next to a 404013/396739 spacing pair, i.e. one late wake that the next one caught up). Both were
under a full suite; the test passed alone straight after. Open: whether the outlier is guest
scheduling (the wake latencies the test prints reach 2.6 ms), the host vsync source, or the push
path — and then whether the oracle should tolerate a bounded number of late wakes or the late wake
is a real defect.

### Watch `l1_real_session_helper_bridges_clipboard_via_mock_mutter` for a second failure
Seen once (2026-08-04): the mock log reached `CLAIMED_NAME / CREATE_SESSION / START / ENABLE_CLIPBOARD`, but
`PASTED sess-host-to-guest-42` never arrived (`l1_session_helper.rs`). Treated as a flake by decision,
not analysis. If it fires again, first check whether the wait for `PASTED` is generous enough under a
parallel nextest lane.

---

## Rules these items taught

**Testing and measurement**
- Wait for the condition you mean, not a signal that precedes it. "Wait for A, then assert on B that
  lands just after A" fails only under load and trains readers to dismiss red. Poll for B, bounded by
  the deadline.
- An oracle must accept only the final state it expects, not the first state that differs from the
  previous one.
- A test that drives the GPU must name the refused command when it fails: fold the renderer's refusal
  log into the panic, and size each leg's deadline to its work. A silent timeout forces a re-run just
  to learn anything.
- A step that fails by timing out must be examined before the deadline's kill, not after: killing the
  local `ssh` hangs up the stalled guest process, and the VM goes with the test. Take the stacks at the
  deadline (`Guest::forensics`, `ssh_exec_timeout_or`) and, to chase one, loop the test with the VM
  held on failure (`LIMINA_TEST_HOLD_ON_FAIL`). A rerun that passes explains nothing.
- A comparison between two builds needs N runs per arm when the failure is intermittent. When a
  failure is stochastic, repeat an arm before believing it.
- Never change two variables to make an arm cheaper, and never run an arm without a capture.
- A gate that cannot reach the shipped path reports on something else while sounding as if it
  reported on this. Confirm its configuration arms the path it claims to score.
- When two independent fixes uphold one invariant (guest-side and host-side), each needs its own
  vehicle. A test that passes because of one proves nothing about the other.
- When guests share credentials a wrong-VM connection is invisible, so a harness proves guest identity
  with a per-handle marker (reading the allocated ssh port is CLAUDE.md's rule).
- A test that passes alone but fails in the suite is an oracle defect or a product defect, never
  noise.
- Measure a tail with many samples and report counts. One sample per cell gives a precise-looking
  number that does not reproduce.
- Before measuring a loaded cell, have it print the guest's own idle percentage: a load that never
  arrives reads as a result (spinners started from an ssh session die on SIGHUP — use `setsid nohup`;
  `pkill -f '<pattern>'` over ssh kills its own session).
- A per-thread transient effect does not license a whole-system conclusion.
- Check a debug-vs-release build before calling something a perf regression (`cargo xtask run` builds
  debug).
- A validator class that fires on healthy frames explains nothing. Diff instances between a failing
  and a passing half, and prove a mask is not just a slowdown before reading survival under it.
- A label can perturb what it names: Metal hashes a pipeline label into the UID it reports, so labels
  must be content-derived to join runs.
- A Metal GPU-capture replayer crash on one of our traces is not evidence about the workload: Xcode's
  replayer mangles KosmicKrisp's MSL deterministically (Radar FB118532264,
  `spikes/webgl-msaa/traces/RADAR-replayer-source-corruption.md`). The recorded command stream is still
  browsable.
- A GPU error counter that counts only what the submit call returns is blind to handlers that swallow
  their own failures. Witness correctness with the backend's own log line, not "zero errors".
- An operation that merely coincides with a wedge is a lead, not a cause; take a `sample <pid>` first —
  it names a deadlock outright.
- "This change did nothing" can mean "not yet": a necessary half of a two-part fix, tried alone, looks
  like a dead end.
- Do not re-diagnose a colour permutation from pixels when the matrix arithmetic reproduces every
  sample. The arithmetic is the proof.
- A workaround outlives its cause unless something re-measures it. Delete a spike whose conclusion is
  falsified rather than annotating it.

**Diagnostics and guards**
- A diagnostic must not change the state it observes.
- A check enforced only in the guest kernel module is not a host bound.
- A guard's doc comment must match its measured trajectory, not an endpoint reading. A guard that never
  binds, documented as coverage, is worse than none.
- A high-water-mark counter must alert on growth, not level, or it fires forever after one episode.
- A deadlock guard is only as true as its premise about *who* is waiting: a wait issued from outside
  the stream must not reuse the stream's deadlock verdicts.
- When two blocking waits each need the other's producer, make each blocker visible to the other and
  poison deterministically. A timeout trades the wedge for killing healthy slow waits.
- When deferring delivery of a result, name the party that blocks on it. If nothing waits, the deferral
  is not latency, it is handing out stale data.
- A signal handler that retries a fault must be able to say the fault was its own, and bound the retry
  when it cannot. xnu reports every arm64 SIGBUS as `BUS_ADRALN`, so `si_code` says nothing.
- A peer's death is not always an event. A connected unix datagram socket on macOS gets no kqueue event
  when its peer closes; the next send's error is the only signal, so loss is handled where that error
  surfaces, and the work item that met it is still completed.

**GPU and renderer**
- A VM crash that arrives through a web page means the guest-reachable crash surface includes GL/vrend,
  not only the Vulkan path.
- Containing the damage after a GPU fault (stop allocating once the device is lost) is worth doing
  whatever the fault turns out to be; it is the half of the harm we own.
- Guest drivers turn host ring loss into `VK_ERROR_DEVICE_LOST`, never `abort()`: ring loss is a
  legitimate runtime event on a VMM that suspends.
- A port keeps the mechanism behind a conclusion, not just the conclusion. "Refuse superres",
  "clear rects are filtered", "scanouts reach the supervisor" each survived the C→Rust port while
  the submit, the renderer-side filter and the Mach handoff that made them safe did not.
- Never smuggle a host-injected operation into a guest-owned id space; give it a first-class entry point.
- A completion wait must cover the queue that did the work. Finishing "the current context" is not a
  fence.
- A guest's rate limit on its own notifications is a promise the host must keep. Mesa rings a venus
  ring's doorbell at most once per idle timeout, so a doorbell that arrives while the ring is awake
  still restarts the ring's idle clock; discarding it lets the ring park inside the guest's window
  with the guest forbidden to ring.
- A bound and the access it guards are derived from one description of the format, never two.
- Host-initiated transfers and readbacks are never charged to a guest context; the failure's type
  decides whether it poisons, not the ctx id or call site.
- A per-context ledger keys on the context's occupancy, not its id: guests reuse ids, and resources
  outlive the context that created them.
- Every Metal object KK mints, texture views included, is registered in the residency set itself, not
  only via its heap. An unregistered allocation faults stochastically, as a read, at an address KK does
  not track.
- Distinct bitfields in one storage unit are one memory location to the memory model: per-flag atomics
  cannot fix a race on one of them; separate the fields.
- A re-created decoder must resynchronise, not resume: replaying the create restores the object, not its
  reference pictures, and VideoToolbox renders wrong pixels rather than failing. Drop inter frames until
  a keyframe and say so in the log.
- A GStreamer plugin blacklisted in `~/.cache/gstreamer-1.0/registry.*.bin` survives an upgrade of its
  dependencies; delivery that fixes a plugin's crash must drop the registry cache.

**Snapshots and worker swaps**
- A fresh worker's virtio-gpu comes up with virtio's default EDID, so any worker swap must re-apply each
  scanout's display configuration and identity *before* the guest resumes.
- After a worker swap, never assume the host's display-table beliefs survived: re-assert the arrangement
  once the fresh device can hear it, and gate "resumed" on an epoch, not a frame counter carried across
  the swap.
- A quiesce oracle counts only devices a driver actually took (`DRIVER_OK`), never "status != 0". Suspend
  has a guest-kernel floor (≥ 6.17 for `virtinput_freeze`); below it the VM keeps running.
- A snapshot records the virtio-mmio device layout (type, base, irq) and what each device offers
  (features, queue count, topology fields such as a console's ports), and restore refuses a machine
  whose devices moved or changed while keeping the suspended session. Any change to the spawn-time
  device list, or to what a device offers, is a one-way door for parked VMs.
- A snapshot journal keeps every create whose handle a retained create references, even after the
  referenced object is destroyed.
- `hv_vm_protect` on a page a vCPU is mid-store into lost the preceding store in 5 of 5 probe landings
  (`spikes/hv-stage2-write-loss/RESULTS.md`); never protect live guest pages in shipped code without
  measuring with a real guest first.
- Confirm a VM has exited before starting another on the same disk.

**Windows and input**
- Nothing in the pointer path guesses a guest layout on the host. Positions come from the guest's
  report, the echo-fitted lines or the identity fallback, and the diagnostic says which.
- A tick-driven path needs the same "is there a live guest" gate as the event paths — gate on "no live
  worker", not on one lifecycle phase.
- A window's lifetime follows the slot table, not scanout on/off; the guest toggles scanouts on every
  ordinary modeset.
- Classify and route a host key on purpose, or drop it. Never forward it blind: under a grab the user
  cannot cancel a destructive host action.

**Guest delivery and the stock floor**
- An installer never makes an unproven kernel the permanent boot default: trial-boot once and promote
  only after it reaches multi-user.
- A feature that exists only for the stock floor must not depend on stock-initramfs contents it cannot
  control; reach for a device class every stock initramfs ships (USB HID).
- A disable switch turns off the whole capability surface, not one transport.
