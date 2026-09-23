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

### The guest-cursor echo check judges against the identity mapping, so with two displays it is unreadable
`echo::verdict` is fed `echo::expected_pixel(unit, scanout)` = `unit × scanout` (single call site
in `input.rs`). That is right only when the absolute range covers one display. With several
displays the range spans the desktop bounding box and the true expectation is absfit's fitted line
(`pixel = a·u + b`), so the check either warns about a disagreement it computed wrongly or falls
into `Ok(None)` and goes silent, which reads as agreement. Fix: expect the fitted pixel where a fit
exists, keep identity only for the unfitted case, and say which one was used in the message.
Diagnostic only: nothing in the pointer path reads the verdict.

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
slot N"). Next: when it fires, read it against `[CURSOR] slot=N hide`, then decide whether the
captured path gets `shape_slot`'s tolerance or the fix belongs upstream in the position.

### A cursor plane left enabled just outside a display's edge
A dogfood log from a single-session guest (2026-08-24) showed, 31 times, `other slots also showing a cursor:
[(1, (-10, 582))]` while the pointer was legitimately at the far edge of slot 0. The neighbouring
slot keeps a visible plane a few pixels outside its own scanout. Placement is unaffected (the echo
names the real slot), but `shape_slot` treats it as a second cursor and it feeds the undrawn-cursor
fault above. Decide whether the guest should hide it or the echo should ignore a plane whose origin
is outside its scanout.

### A held seam leaves no trace
`window/seams.rs` is pure policy with no logging, so a seam that was held (the adjacent panel not
fullscreen, not on its active Space, or off-screen) cannot be told from one that was never reached.
Fix, on the `cursor::undrawn_fault` pattern: log one line when a hold engages and one when it
releases, naming the side, the slot the range leads to, and which coverage answer refused it
(called from `input.rs`, `seams::Hold::of(...).apply(range)`).

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
Reported once (2026-08-22), cause unknown. Every press the tap sees logs `pointer capture: click at (x,y) —
grabbed=…; fullscreen=… key=… space=… on-screen=… grab-enabled=… latched=…`, and a system-disabled
tap logs `pointer capture: the system disabled our event tap … re-enabled` — events in that gap
reach the app untapped, which matches the reported signature. Next sighting: read those lines
before forming a theory.

### Needs repro: the released pointer can come back invisible
Seen once on the two-panel rig (2026-08-23), fullscreen on both panels after a click had promoted the grab:
releasing a hard grab with Ctrl-Opt left no cursor drawn; the pointer was live, and pushing it up to
reveal the chrome brought the image back. The per-tick blank-wear check and the unhide fix
(`64aee92`) are both in, so this is a path that skips both or a macOS unhide that did not take.
Catch it with the poke-VM trace env on.

### Needs repro: the menu-bar reveal drops while a macOS menu is still open
Observed on the dogfood Mac (2026-08-22): a small downward move with a menu open releases the ask and the chrome retracts under the open
menu. The ask is slaved to `NSMenu::menuBarVisible` (`InputState::menubar_observed`) and released by
`reveal_step`; which of the two lets go first has not been measured.

### `notch = extend` does not hide the band when the reveal triggers ungrabbed
While macOS has the menu bar out, the strip overlay keeps covering its band, so the revealed bar
sits behind it. The grant path works (`InputState::menubar_observed`); the band standing down in
the uncaptured case is missing. Small.

### Needs repro: pointer not shown right after logout/login under synoik
Intermittent on dogfood (sighted 2026-08-22) under synoik, not under mutter — a mutter "cannot reproduce" is a false
negative here. Chase it on a clone of `Fedora-Workstation-44.enhanced.synoik.raw`, in a loop: plane
visibility is readable from the `[CURSOR] … visible=` trace, so many cycles can be checked
automatically, and the guest synoik session can be asked what it saw. One clean cycle did not
reproduce it.

### Assemble the grab tier once per tap event
`capture_tap.rs` builds `GrabMode` at several sites from the `captured` atomic, the per-event `soft`
predicate and `GrabState` (`grab_policy::grab_mode`/`capture_tier` calls). `soft` comes from
`window_facts` (a window-server round trip) and must not be cached: a stale `space_visible` once
left the keyboard pointed at a guest that had left the screen. Shape: the tap samples facts once per
event, a `TapCtx` method assembles the tier, and only the answer travels; `grab_policy` stays pure
and parameterised so its assertions run without a VM.

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

### A guest VT switch leaves the host holding the wrong session's arrangement report
The virtio-gpu wire does not change across a seat switch (same scanouts, modes and resources, no
event). Two faults follow when the incoming session runs no `limina-agent-session`:
1. The report goes stale instead of absent. `layout_gate` holds while a session is inactive, which
   is correct only if the incoming session claims the report. A compositor started with
   `systemd-run --uid=… synoik --session` never reaches `graphical-session.target`, which is what
   `limina-agent-session.service` is `WantedBy`.
2. A present report outranks everything. `absfit::abs_position` consults the fitted lines only
   `if !arrangement::has_report()`, so the contradiction/refit machinery is locked out;
   `desktop_in_range` and `range_shares` also build the captured confinement and seam shares from
   the stale report.

Measured 2026-08-24 on the dogfood guest: sends landed 2452 px off, the echo warned `we sent the pointer to
slot 0 … the guest shows its cursor on [(1, …)] and none on slot 0`, and the captured pointer was
clamped inside the other session's desktop — for exactly the windows where the other session owned
the screen, clearing on every return (`layout_gate.poll` re-sends on inactive→active).
Fixes, cheapest first: demote a report the echo keeps refuting (absfit's `CONTRADICTIONS` pattern;
host-side, so it also covers the stock tier); name the tier behind each send (report, fit or
identity) in the echo-mismatch warning; on the enhanced tier, have root `limina-agent` watch logind
and tell the host when the seat's active session changes; reconsider the helper's `WantedBy`, or
treat "no helper in the active session" as a reason to distrust the held report.

### A `SUBMIT3D` error storm across a guest DRM-master handoff
Measured 2026-08-24 on the dogfood Mac with the C renderer: about 20 s after a guest `chvt` between two seat
sessions, `ctx 25 submit_command -> Err("ErrRutabaga(ComponentError(22))")` repeated at ~4 KiB per
command for the length of the handoff — consistent with a compositor that keeps submitting after
losing DRM master. Owed on virglrs: reproduce with the multi-session steps above; trace the EINVAL to
its emission site before reasoning from the message; check what the worker does with a context whose
submits fail in a sustained run (log volume, poison, recovery).

---

## Lifecycle & supervisor

### The control center snapshots VMs on the AppKit main thread
A 1 s `NSTimer` (`crates/limina/src/center/mod.rs`) calls `controller.refresh`, which runs
`model::snapshot()` on the main thread — including every per-VM `stat()`: `disks_line`'s existence
check and the cheap-depth pre-flight behind `VmRow::blocked` (`docs/design/vm-start-preflight.md`
§3.6). A dead network mount can block a `stat()` for seconds and freeze the UI. Fix: snapshot on a
background thread and hand the finished rows to the main thread.

### Move the supervisor⇄worker control sockets to Mach ports
Five UNIX sockets carry the control plane (`limina-ctrl`, `limina-resize`, `limina-balloon`,
`limina-fido-usb`, `limina-moc-usb`) at the predictable path `$TMPDIR/limina-<kind>-<pid>.sock`,
mode `srwxr-xr-x`, so any same-user process can connect. That matters most for FIDO (CTAPHID for the
SEP-backed passkey store) and MOC (Touch-ID-gated fingerprint protocol). A bootstrap-registered
receive right would make the gate a capability and die with the process. Prior art:
`crates/limina-surfaceport` registers a per-process bootstrap name, survives worker relaunches and
falls back when registration fails. Check first: is a same-user bootstrap lookup actually harder to
reach than a socket path; how the test harness would drive a port; whether death/relaunch semantics
survive the worker's `libc::_exit` on every guest power-off. Meanwhile `tmpsock.rs` removes what a
run allocated at every `process::exit` site (`exit_cleanup()`); a SIGKILLed supervisor leaves a
harmless stray socket (binders unlink before `bind()`). **Do not add a startup sweep that reaps
sockets whose embedded pid is dead** — pids are recycled.

### Movable VM library and per-VM placement
Design: `docs/design/vm-definitions.md` §8. The library location is only `$LIMINA_VM_LIBRARY` or the
default (`vmlib/bundle.rs`); nothing persists it, and an unplugged external-volume library makes
`create_dir_all` silently grow a shadow library on the boot volume. Order: (1) persist a library
path in `config.toml` (env > config > default, re-read per call) plus a guard refusing creation on
an unmounted volume; (2) a "Change VM Library Location…" picker that repoints without migrating;
(3) per-VM placement via symlink-as-registration, showing dangling links greyed out as "volume not
mounted". Interim: symlink `~/Library/Application Support/Limina/VMs` to the external disk.

### Inventory the firmware features the guest probes and gets `NOT_SUPPORTED`
Each declined feature is guest behaviour inherited by default instead of chosen. PSCI 1.0 +
`SYSTEM_SUSPEND` are offered (libkrun `hvf/src/lib.rs`, gated by `LIMINA_PSCI_SYSTEM_SUSPEND`).
Owed: the rest of PSCI 1.x (`SYSTEM_RESET2`, `CPU_FREEZE`, `CPU_SUSPEND` idle states, the full
`PSCI_FEATURES` answer set), SMCCC/`ARCH_FEATURES`, PPTT/topology, and anything else a guest
probes — decide offer or decline for each and record it.

---

## Suspend/restore

### `limina suspend` reports failure on a suspend that succeeded
For a managed VM, `cmd_suspend` (`crates/limina/src/main.rs`) waits up to 75 s for
`vmlib::runtime::wait_stopped`, which polls the bundle's run **flock**. Observed twice: the snapshot
was written 4.5 s after the request and the worker had exited, yet the CLI blocked the full 75 s and
printed `did not suspend within 75s — the guest could not quiesce … it is still running`; `limina ls`
then showed the VM suspended and it resumed normally. So the run lock outlives a successful suspend,
and the message blames the guest and asserts the VM is running. Fix: treat `state.toml`'s
`suspended` record (or the snapshot file, as `cmd_suspend_flat` does) as the success signal, and
find what keeps the flock held (a lingering holder of the lock fd first). This is the path automation
uses.

### `limina suspend <disk>` leaves the supervisor alive and blocks the next suspend
After a flat-disk `limina suspend <disk>` the worker snapshots and exits 126, but the supervisor
stays up holding its gvproxy (and SSH port) and a stale window, and `SIGTERM` does not end it. The
next `limina suspend` on the same disk refuses with "multiple limina supervisors match"
(`cmd_suspend_flat` `pgrep -f`s the disk path and keeps every process named `limina`). Reproduced
2026-08-29 on all three cycles of a synoik poke session. Decide whether a flat-run supervisor exits after a
CLI-requested suspend (a windowed run parks behind the play button by design), make `SIGTERM` end a
parked supervisor, and have `cmd_suspend_flat` skip a supervisor whose worker already suspended.

### The snapshot bracket gives up on a slow guest, wakes it, and then misses its late sleep
The SIGTSTP bracket in `crates/limina-vmm/src/krun/mod.rs` has a fixed `QUIESCE_TIMEOUT` of 20 s;
on expiry it logs `bracket: ABORTED`, pulses `wake::guest` and re-arms. Measured 2026-09-16 on the F44
enhanced golden: a fresh seated GNOME session reaches PSCI SYSTEM_SUSPEND 11 s after
`systemctl suspend`, a *restored* one took 23.5 s — the wake landed on a guest still awake, the guest
slept 3.5 s later, and the bracket had already given up. The suite now waits for the guest to be
asleep before signalling, so it no longer exercises this; the dogfood path still runs bracket-first
under the same 20 s. The honest outcome of a missed budget is "not suspended, still running".
Choose: a longer budget, no wake on abort, or a wake only if the guest is later seen asleep. The
host-sleep bracket in `power.rs` (`DEVICE_WAIT` = 15 s) has the same shape and needs the same
decision.

### A snapshot does not record the spawn-time device topology
Any change to the worker's spawn-time device list (e.g. adding a virtio-serial port) makes every
snapshot taken before it unrestorable into a worker built after it; the suite never sees this
because it takes and restores on the same build. `validate_transport_states` (libkrun
`vmm/src/device_manager/hvf/mmio.rs`) fails closed only when a *captured* device (in practice
virtio-gpu, the only one still at DRIVER_OK) is missing from its mmio base/irq; it does not see
ports, config or feature changes on devices the guest froze to INIT, and a `TODO(feature-drift)`
asks for an `acked_features` compare. Fix: a topology/feature fingerprint in the snapshot header,
refusing a mismatch loudly and naming both sides. Until then, resume and shut down parked VMs
before installing a build that changes the device list.

### Device workers can write guest RAM while `dump_ram` runs
Pausing the vCPUs stops new kicks, not writers already running: a device thread writing guest RAM
during the dump can tear it (used.idx advanced while the payload is half copied). On the raw path
the worst offender is net RX from gvproxy; on the s2idle production path the guest has frozen net
and blk, which leaves the GPU renderer thread. Fix: park the separate-thread writers (GPU renderer,
blk) for the length of the dump. Check the thread inventory first — if `save_snapshot` runs on the
event-loop thread, the EventManager-dispatched devices are already quiesced.

### Configure can coarsen the IPA granule of a suspended VM
A suspended VM has no supervisor, so the control center shows it Stopped and offers Configure.
Changing *Memory pages* 4 KB → 16 KB and then resuming replays a guest layout the coarser granule
cannot express, and blob maps refuse mid-replay (a finer granule is safe). Record the granule in the
`Suspended` record (`vmlib/state.rs`, which carries only `snapshot`) and refuse or warn on a
coarsening resume. The only guard today is the help text beside the popup (`center/controller.rs`).

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

### Restore tests cannot see guest kernel errors
The enhanced image's `kernel.printk` is `1 4 1 7`, so `console.log` carries only emergency
messages and a virtio-gpu `response 0x…` error (logged at `err`) never reaches it; a restore test's
console after resume is empty. Have the restore tests copy `journalctl -k` over ssh into scratch
after each restore and fail on `virtio_gpu`/`[drm]` errors.

---

## Memory

### The settle-sweep fault handler fields every fault at a guest address
`sweep_fault_handler` (libkrun `hvf/src/released_ram.rs`) catches any SIGBUS/SIGSEGV whose address
falls in a guest region, forever after the first sweep. By design it ignores `SWEEP_ACTIVE` (a
last-window fault can arrive after the flag clears, and chaining it would restore SIG_DFL for good).
The side effect: a non-protection fault at a guest address (e.g. `BUS_ADRALN` from a misaligned
atomic in a device bug) refault-loops silently instead of crashing. Fix: field only
`SEGV_ACCERR`/`BUS_ACCERR` and chain the rest. Such a loop shows as `sweep_faults` climbing into the
millions (stats verb / decision trace).

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
clean and abnormal exits is ours. Until settled, anyone running repeated crash arms should watch
`vm_stat` compressor pages and reboot before the segment limit.

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
cache. Measured 2026-08-13: a 1.12 G balloon with the host billing 36.95 G, 24.91 G compressed; it recovered in
~5 min only because the footprint pushed the host to `warn` and triggered the trickle dig — host alarm
is a poor trigger. Today the discriminator is `actual_bytes` on the trace row. Give the stranded case
its own verdict or label (as `AllowanceBand` and `shortfall` were split out). Inelastic runs are not
downstream of give-backs (0 of 27 long runs had one in the preceding 120 s), and the io give-back's
MemFree/MemAvailable gate narrows the main path into the stranded state.

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
settle now and then.

### Post-episode warm-read tax (~1.6x) after a deep balloon dig
After a deep dig and give-back, a fully recovered guest (cache re-warmed, kswapd idle, io-some <1%,
no swap) reads its own page cache at ~16 GB/s against ~26 GB/s pristine (192 vs 118 ms/pass on the
S3 bench vehicle, measured 2026-08-12). Cause unidentified; deflate pacing is ruled out as a lever. Leading hypothesis:
page-cache folio-order collapse, where scattered 4 KiB inflates fragment the buddy lists and the
re-warm under duress rebuilds the cache as order-0 folios. Probes: `/proc/buddyinfo` and folio-order
stats across an episode; in the recovered state `drop_caches` then a calm re-read (back to ~118 means
build conditions, still ~192 means persistent fragmentation). A real fix probably needs a
host-page-aware batched-inflate balloon. Low priority: it bites only after a real host-pressure
episode.

---

## vCPU & power

Background for this section — why idle guests need a real-time band, what it costs, and what
ships — is in `docs/design/vcpu-scheduling-band.md`.

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

### Revisit the band arm cap, which costs about 20% of venus throughput
`arm_cap()` is half the efficiency cluster, floored at 1 (1 on an M1 Max, 2 on an M4 Pro). On
`gl-replay-venus` the shipped default scores 44.8–49.4 against 56.67–56.80 with the static
all-banded arm, so the whole cost is band reach (`perf/2026-09-21-remeasure.md`). The cap stays for
safety, by decision. A revisit must account for: the two band-sensitive instruments disagree in sign
(banding all four vCPUs restores `gl-replay-venus` but costs ~12% on `vk-replay-venus-headless`);
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

### Measure efficiency beyond idle, and against Parallels/Vz
Known: an idle guest costs ~13 mW of package power over an empty host, a `vkcube` guest ~0.9 W
(`spikes/macos-timer-wakeup/results-battery.md`). Disk, network, builds, video calls and a desktop in
use have never been measured in watts, and "replace Parallels" is a perf/W claim with no evidence.
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

## GPU robustness — guest-reachable aborts & containment

### The "no guest-reachable aborts" audit, restated for virglrs
Policy by layer: asserts guard internal invariants only; host drivers (KK, zink) tolerate bad input
by clamping or skipping, because `vkCmd*` cannot return errors; the renderer is the trust boundary —
validate, then poison the context; libkrun's Rust decoders return errors instead of `unwrap`. The
surface was counted against the C renderer and needs recounting now that virglrs is the boundary:
virglrs `src/venus` and `src/vrend` (`unwrap`/`expect`/`panic!`/`assert!` on guest-reachable paths,
tests excluded — a raw grep over all of virglrs `src` gives ~1250 hits, so a real per-path count is
owed); KK `vulkan/` (178 asserts at the last count); `vk_meta*` compiled into KK (71); libkrun
virtio-gpu (89). The GL path needs its own answer: a guest's GL stream reaches zink through virglrs's
vrend, so hardening the Vulkan decoder does nothing for it. Decide what vrend validates before zink
sees a command.

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

### KosmicKrisp's command-allocator pool has no ceiling
The pool in `kk_device.c` (`limina-kk`) mints on a miss and retires surplus allocators only in a
call already served from the pool. Two ways a guest drives it without bound; only the lost-device
case is contained (the pool refuses to mint after `vk_device_is_lost_no_report`).
- **A client that never lets a pass complete.** Every allocator stays `in_use` and the pool mints
  one per pass. Measured 2026-08-26 on the host (zink-on-KK) with `spikes/notification-text-corruption/glyphmimic`:
  100 passes → 101 live class-0 allocators, 300 → 301, 600 → 435, 930 → 510 — an unbounded
  in-flight pool, not a leak, depending only on render-pass count. Any per-frame completion or flush
  keeps it under the watermark, which is why gnome-shell (flushing every frame) never shows it. Repro:
  `spikes/notification-text-corruption/mimic-host.sh 93`, watching stderr for
  `[LIMINA-ALLOC-POOL] class 0 grew to N`.
- **A slow GPU.** `kk_alloc_pool_get` never blocks on GPU progress, reasoning that the client's own
  fencing bounds in-flight depth — which does not hold for a guest. Under full Metal shader validation
  (~10x slower GPU) class 1 grew to 7,283 allocators against a 4 MiB budget until
  `IOGPUMetalCommandBufferStorageAllocResourceAtIndex` refused and the worker took SIGABRT.

Fix shape: enforce the budget — at a ceiling, retire, block on GPU progress, or fail the submit —
without stalling `cs_start_render` in the common case. The "budget" in the warning is reported, not
enforced. Rate-limit the "in-flight depth is outrunning completion" warning too: it fires on every new
peak, runs to thousands of lines and adds load of its own. The growth warning only fires upward
(`watermark_warned`), so read it as a high-water mark; the clock-paced pool report gives live/peak.

### zink can begin rendering with a stale stencil attachment
Measured 2026-09-04 on the dogfood Mac: SIGABRT after 29 h of session at `assert(!ctx->dynamic_fb.info.pStencilAttachment
|| ctx->gfx_pipeline_state.rendering_info.stencilAttachmentFormat)` (`zink_context.c`), via
`zink_draw` → `zink_batch_rp` → `begin_rendering` on the threaded-context worker. Crash report:
`spikes/zink-stencil-attachment-assert/`. The guest workload is unknown (the app-launched worker's
stderr is not captured). Mechanism, derived from source and not observed: `begin_rendering` refreshes
the attachment pointers only when `rp_changed || rp_layout_changed || (!in_rp && rp_loadop_changed)`,
while `zink_update_rendering_info` recomputes the formats on every call; a begin during a blit sets
`pStencilAttachment` unconditionally, `zink_batch_no_rp` skips its `tc_info` reset while blitting, and
the blit's rebind of the same framebuffer does not raise `rp_changed` — so the next draw pairs a stale
non-NULL pointer with a freshly UNDEFINED format. (A stencil format mapping to UNDEFINED is ruled out:
`zink_get_format` returns UNDEFINED only for two 4444 colour formats.) Our stack hits this far more
often than upstream: KK lacks `EXT_multisampled_render_to_single_sampled`, so every MSAA
render-to-texture goes through `zink_render_attachment_shadow`, which toggles `blitting`. Low
priority — asserts are compiled out of the shipped stack, so the likely outcome is one wrong frame; a
stale image view reaching KK is not ruled out. Fix on `limina-kk`: refresh the pointers whenever the
formats are recomputed; worth upstreaming.

### Run a ThreadSanitizer round on the virglrs + zink-on-KK host stack
virglrs's vrend keeps the seam that produced the C-era races: a fence waiter thread makes its own
context current in ctx0's share group and waits on syncs (`vrend/waiter.rs`), entering zink's
screen/batch state from a thread zink does not know about; virgl-over-zink is undertested upstream.
The zink race fixes are on `limina-kk` (`a0d96c18f02`, `c398247db94`). One race is left alone
deliberately because locking it is a hot-path perf decision: `zink_batch_reference_resource_move()`
and its `_unsync()` twin mutate the same batch lists from different threads by design. No TSan pass
has covered the Rust renderer. Recipe: build host mesa with `-Db_sanitize=thread` into its own prefix
and point the worker at it with `MESA_PREFIX`; link the TSan runtime into the worker with
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
mesa-guest 0005 makes a submit on a FATAL ring return `VK_ERROR_DEVICE_LOST` from the ring layer, but
the generated `vn_call_*` (`vn_protocol_driver_*.h`: `dec ? decode : VK_ERROR_OUT_OF_HOST_MEMORY`)
still turns any absent reply into OOM. Reply-pool allocation failure, ring FATAL at submit, ring FATAL
while waiting and a real host refusal all reach the app as one code — the mechanical root of "OOM out
of venus is never memory". Fix, guest-side and upstreamable (`limina-guest`): carry the submit's
result into `vn_ring_submit_command` and have `vn_call_*` return it when the reply is absent. Cheap,
and it makes the next dogfood OOM report classify itself.

### virglrs's transfer bounds error does not say which bound failed
`layout` in `vrend/transfer.rs` collapses its three exits (stride smaller than a row, layer stride
smaller than a layer, offset plus span past the pages) into one bare `Error::IovOutOfRange`.
(`BoxOutOfRange` is already separate.) Diagnosability only: a distinct variant or detail per exit,
carrying the two quantities compared.

### Re-verify CPU-write → GPU-read coherency on a shared dmabuf under virglrs
A guest that does `gbm_bo_map` → write → unmap on a LINEAR `Argb8888` dmabuf and then samples it from
venus read the buffer's previous contents: the write reaches the host as a control-queue transfer on
libkrun's gpu worker thread, venus reads on its own ring thread, and nothing orders the two. The fix
takes two halves: guest mesa virgl flushes and waits for the bo to idle on unmap of a write map of a
`PIPE_BIND_SHARED` resource (mesa-guest 0008), and the host finishes the upload before the virtio-gpu
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

### vrend scanout flushes carry no fence, so every GL desktop pays a Metal copy per frame
`virtio_gpu_plane_prepare_fb` (`drivers/gpu/drm/virtio/virtgpu_plane.c` on the linux fork's `limina`
branch) returns before allocating a plane fence for any primary plane whose bo is not a guest blob,
then fences only dumb or imported objects. A GNOME desktop scans out through vrend (non-blob) on both
tiers, so its flushes carry no fence, no `GuestFlushHold` forms, and the compositor may render into
the buffer while it is on glass. The supervisor covers that with a Metal-blit copy of every unheld
frame (`docs/graphics.md` §4): correct, but ~1.2 ms of added latency per frame, 4.6–6.1 ms worst case
under a 24–33 fps WebGL load (measured 2026-09-12). Fix, enhanced tier only (a commit on the fork's `limina` branch): fence
every primary-plane flush the host can hold, so the enhanced tier goes back to zero-copy and the stock
tier keeps the copy. libkrun already reports the change (`scanout_held`). Check with
`LIMINA_PRESENT_MUTATION_TRACE=1` (zero surfaces changed while up) and the worker's
`scanout N flushes are fenced` line.

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

### Direct-KMS strictly double-buffered clients run at ~30 fps
kmscube `-A` ran at 31 fps on a 60 Hz host whatever `LIMINA_FENCE_LATCH_MS` was (8 and 35 ms both
gave 31), measured 2026-06-23, before the virglrs present path — re-measure before acting. A client that blocks
on flip-complete misses every other vsync because the fence-accurate present waits twice in sequence
(GPU render complete, then the CoreAnimation latch) and the round trip exceeds one vsync. Wayland
desktops and fullscreen apps reach 60 because mutter triple-buffers; only bare direct-KMS
double-buffered clients (kmscube, SDL-KMS demos) are affected. If they ever matter: fire the
atomic-KMS fake vblank at render-complete or on a vsync-cadence timer without reintroducing tearing,
or shave the present round trip below one vsync. Run kmscube over ssh as `sleep N | kmscube …` (it
polls stdin and bails on EOF).

---

## GPU perf

### Decide whether KK should advertise `VK_EXT_vertex_input_dynamic_state`
Throughput, not correctness. Host GL is zink-on-KK, and without this extension zink compiles vertex
input into the pipeline, so every shader × vertex-layout combination is a separate PSO; with it, zink
sets the layout through `CmdSetVertexInputEXT` and collapses the permutations. Check first: Metal
compiles the vertex descriptor into the pipeline state object (`MTLRenderPipelineDescriptor.vertexDescriptor`).
If that still holds under **MTL4**, which this tree encodes with, KK could implement the extension
only by caching PSO variants keyed on vertex input — zink's current job moved one layer down, for no
gain. The answer decides whether the item is worth anything.

### Only if GPU-bound workloads reappear: KK's per-draw root re-fetch
A candidate GPU-side cost: KK re-fetching the root on every draw. Unverified on the current tree and
worth nothing unless a current perf instrument is GPU-bound — check both before spending time on it.

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
context count. Armed: the context hook in `limina-kk` `bb3994fc6db` (ivars + pass-state canary at
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
`sample_count_sa`. `LIMINA_KK_ADDR_CHECK` already reports the view half as `[LIMINA-MSBIND]`
(`kk_cmd_draw.c`). Not the WebGL-MSAA cause (`[LIMINA-MSBIND]` fired on 0 of 24 draws at the loss).

### A meta dispatch "restores" the compute root instead of the caller's root
The compute path binds its root through `kk_cmd_bind_root_to_argument_table` (`kk_cmd_dispatch.c`),
which records it in `cmd->state.root_addr`; the meta dispatch's closing "Rebind the exiting root"
(`kk_cmd_buffer.c`) then rebinds `cmd->state.root_addr` — the compute root just bound, not the
graphics root live before it. Nothing breaks today because the draw flush rebinds unconditionally.
Fix: save the caller's root before the dispatch and restore that.

### The source of a NULL allocation added to the residency set is unknown
`kk_device_add_{heap,buffer,texture}_to_residency_set` used to pass NULL to
`mtl_residency_set_add_allocation`; Metal stores it and the next submit faults in
`-[AGXG13XFamilyResidencySet _commitAddedAllocations:…]` (`KERN_INVALID_ADDRESS at 0x18`) on whichever
ring thread submits (seen once, as a worker SIGSEGV in `synoik_desktop_survives_snapshot_restore`).
All entry points now refuse NULL and the texture path logs the caller. What produced the NULL is not
established (suspect: the three image-view residency call sites in `kk_image_view.c`); the guard has
never fired since. If `[LIMINA-RESIDENCY] refused a NULL texture, called from %p` appears, symbolise
that address — it names the site.

---

## Video

### A host failure decoding a held AV1 frame drops the next frame too
`Av1::advance` (virglrs `vrend/video/mod.rs`) submits the frame held at the eight-slot wall with
`?`, so a VideoToolbox error on the held frame returns before the incoming frame's shape is
recorded, and that frame is dropped at END_FRAME as well. Super-resolution frames no longer take
this path (they decode normally), so it needs a genuine host decode failure. Fix: record the
incoming frame's shape before the held one is submitted, or submit the held frame without `?` and
log its failure.

### Stock-tier Firefox never gets hardware decode (virgl offers I420/YV12 as decode targets)
`virgl_is_video_format_supported` ignores profile/entrypoint and answers with the generic sampling
check, so vanilla mesa advertises NV12, YV12 and IYUV for decode. ffmpeg's
`vaapi_decode_find_best_format` scores exact `sw_pix_fmt` matches at `INT_MAX`; for 8-bit 4:2:0 the
three-plane formats tie above NV12 and the last advertised (IYUV) wins. Firefox's DMABUF path accepts
only NV12/P010, logs `Unsupported VA-API surface format 808596553` (I420), destroys its frame pool and
decodes in software on every stream, codec-independent. Chrome and GStreamer choose NV12 themselves;
I420 decode itself is correct (framemd5 bit-identical to software), which is why
`ffmpeg -hwaccel vaapi` hides it. The enhanced tier carries the fix (mesa-guest 0011 withholds
YV12/IYUV for `PIPE_VIDEO_ENTRYPOINT_BITSTREAM`). Owed: send it upstream so stock Firefox gets the
hardware path — stock images load vanilla mesa, and libva probes `/usr/lib64/dri-freeworld/` ahead
of `/usr/lib64/dri/`, so RPM Fusion's unpatched `mesa-va-drivers-freeworld` wins even where ours is
installed. The durable fix is a virgl wire query for per-codec decode-target formats (the protocol has
none). Unchecked: other VA-DMABUF consumers (GStreamer `vaapisink`, Chromium's other paths). Stock-tier
validation vehicles: Chrome and GStreamer (`docs/design/h264-hevc-decode.md`).

### Decode targets on the stock tier are one-page stubs; the remaining zero-copy work
On vanilla mesa an exported VA decode surface's dmabuf is a 4096-byte stub at every resolution
(`spikes/va-dmabuf-size`). The enhanced tier gives decode targets real guest memory (mesa-guest 0013,
0014) and dropped the too-small-export refusal (0018), so Firefox has its hardware decoder there.
Still owed, owned by `docs/design/blob-decode-targets.md` §Phases: glupload's direct importers refuse
everything (`DirectDmabufExternal … cannot produce texture-target 2D`) and fall back to the copy
uploader; the guest/host layout contract (the guest computes tight strides, the IOSurface is
Metal-aligned, and the host writeback copy reconciles them) must land before the copy can go; and the
stock tier, reachable only by upstreaming.

### Guest virgl gates the composite decode-target create at the caller, not at the emitting site
mesa-guest 0017 added the sampler-bitmask check (`is_format_supported(buffer_format,
PIPE_BIND_SAMPLER_VIEW)`) to `virgl_video_create_buffer`. The site that emits the planar create,
`virgl_resource_create_front` (`virgl_resource.c` on `limina-guest`), checks only
`VIRGL_RESOURCE_FLAG_VIDEO_TARGET` && `VIDEO_PLANAR_TARGET` && more than one plane. Unreachable today
only because the video path is the one caller that sets `VIRGL_RESOURCE_FLAG_VIDEO_TARGET`. Any new
caller, or a relaxed video gate, brings back what 0017 fixed in its worst form: the kernel has already
handed out the handle, the host's refusal is invisible, and the context goes to `Illegal resource`
for the rest of its life, silently dropping every later submission. Fix: move (or duplicate) the
sampler lookup into `virgl_resource_create_front` on `limina-guest`, re-export, bump the mesa RPM
release, redeliver.

### Hardware decode blocks the virtio-gpu control thread for the length of every frame
`Session::decode` (virglrs `src/videotoolbox.rs`) calls `VTDecompressionSessionDecodeFrame`
without `kVTDecodeFrame_EnableAsynchronousDecompression` and returns only once the output callback
has run. `Video::end_frame` (`src/vrend/video/mod.rs`) calls it inline, so the wait runs on the
libkrun gpu worker thread that serves the whole control queue. Every other context's submits,
flushes, `SET_SCANOUT`, cursor and fence processing queue behind one frame's hardware decode. A stack
sample of a dogfood worker playing 640x368 video in Firefox while a Moonlight stream also used
the media engine (measured 2026-09-23, M4 Pro) showed that thread spending 1094 of 4485 samples in `Session::decode`
(896 in `FigSemaphoreWaitRelative`, 179 in a synchronous XPC reply from `VTDecoderXPCService`). The
same run logged `control queue drain ran 100–626 ms` warnings. That run had the worker clamped to
priority 4 by macOS Game Mode, which inflated the waits (with the clamp gone they fell to 229/3834),
but the coupling is independent of the clamp: any slow decode (a
contended media engine, a large AV1 frame) stalls the desktop's present with it. Fix shape: move
decode onto a per-session decode thread (or VT's asynchronous mode), and retire the END_FRAME's
fence when its picture lands, not when the command is processed. Delivery into the target must stay
on the thread that holds the GL context (the VT callback thread has none, `docs/graphics.md` §4.5).
It also has to keep the per-codec frame ordering and the restore journal's view of which frames were
decoded. Check: the same stack sample shows no `Session::decode` under `Worker::process_gpu_command`, and
`LIMINA_GPU_TRACE` flush-to-present latency on a seated desktop does not move when a video starts.

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

### The control plane drops first-message violators at whatever rate the guest offers
`control.rs` accepts a peer, logs `control: peer's first message was not HELLO …; dropping`, and
returns — no per-peer backoff and no cap on concurrent unauthenticated peers, so a
reconnect-without-backoff guest can still spin the accept loop (measured before the muxer and helper
fixes: 396,747 connects in 150 s, 2026-08-21). The muxer now resets connections whose proxy socket cannot be
created, so a storm no longer kills the worker. Consider a per-peer accept backoff or a cap on
concurrent unauthenticated peers.

### Log which guest session each clipboard copy came from
All `limina-agent-session` peers can push to the host pasteboard and the last write wins — deliberate,
for cross-session paste including the gdm greeter (arbitration in `docs/roadmap.md` §M12). The cost is
that a copy from a background session reaches the host with nothing that explains it. Log the
originating peer for each accepted guest offer (no protocol change needed); session id and active
state would need the agent to report them, since the host cannot see `loginctl`.

### Automated coverage gaps in the session helper
`limina-agent-session`'s ext-data-control backend (`guest/limina-agent-session/src/wayland_clip.rs`)
is verified live only: `l1_session_helper.rs` exercises the RemoteDesktop path and
`l2_clipboard_vdagent.rs` deliberately stops the helper. Nothing automated covers helper reconnect
after a supervisor restart or after the D-Bus session dies. (Per-peer serials and stale-offer
rejection are covered by `l1_clipboard_multi_session.rs`.)

---

## Networking

### The net device dies permanently when gvproxy hangs up
On HANG_UP/READ_HANG_UP from the backend socket, libkrun's net worker (`devices/src/virtio/net/worker.rs`,
the `backend_socket` arm) logs "VIRTIO-NET FATAL … Networking is now disabled!" and stops servicing
it. The supervisor can respawn gvproxy on the same socket path (`gateway.rs`), but the guest NIC stays
dead until the VM restarts. Fix: a small libkrun change that reconnects to the socket path on hang-up.

### An idle guest reads virtio-net `InterruptStatus` about 2,400 times a second
Measured 2026-08-27 on a stock F44 guest at a settled idle desktop with `--net`: 72,374 MMIO reads of
`0xa01f060` in 30 s — offset `0x060` (`InterruptStatus`) on `a01f000.virtio_mmio` → `virtio_net`.
virtio-blk (`0xa01d060`) was a distant second at 2,896; every other device was in the tens. MMIO
writes are not logged, so the true exit count is higher. Unknown whether this is gvproxy's normal
traffic, an ack pattern costing an extra read per event, or a genuine interrupt storm. Start by
rerunning the count with `--no-net`, and against a guest with no NAT traffic.

---

## Guest images & delivery

### Guest tools cannot be installed from the app alone
The enhanced payload (kernel + mesa RPMs + agents + installer) is not bundled, and a second Mac has no
RPMs and no toolchain. The qga bootstrap kit delivers limina-agent through the stock guest agent, but
only on an unconfined agent — F44 Enforcing keeps SSH as its delivery path. Designed, not built
(`docs/design/distribution.md` §5): a versioned `limina-guest-tools-<ver>-<distro>.tar.zst` with a
manifest, and `limina install-guest-tools [<vm>]`, which downloads or takes `--payload`, verifies,
stages into an ephemeral read-only `--share`, and runs the installer through the agent or prints the
one-line `sudo` command. The installed `tools_version` goes into the VM definition and is re-checked on
connect: a mismatch prompts, never refuses to boot.

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

### Fold `xtask bundle` into `xtask app`
`bundle` (writes `target/Limina-smoke.app`, debug, ad-hoc) no longer earns a second command: `app`'s
assemble + sign + dmg phase measured ~25 s (2026-08-31), the rest is the cargo build, which `cargo xtask app --debug`
avoids, and `build-app.sh` already supports `LIMINA_ALLOW_ADHOC=1` / `LIMINA_SIGN_IDENTITY=-` and
`LIMINA_NO_TIMESTAMP=1`. What `bundle` still has: `--open` (a LaunchServices launch booting the L1
`limina.hold` guest — the Dock-launch path where launchd's 256-fd limit bit) and independence from
`/Volumes/mesa-cs` (`app` sources the KK/zink dylibs from it). Fix: add `--open` to `app` and delete
`bundle`; the references are `docs/dev-onboarding.md` and `CLAUDE.md`. The module doc in
`xtask/src/main.rs` still says `bundle` assembles `target/Limina.app`.

### virtiofs DAX window
Not implemented (`docs/roadmap.md` §M5; `docs/design/16k-page-requirement.md`). Wire libkrun's
virtio-fs shm region (`VirtioShmRegion`, `fs/device.rs`), confirm window alignment and
FUSE_SETUPMAPPING on 16 KiB host pages (testing the stock-4k guest separately from the 16k one), and
settle host↔guest uid mapping. Without DAX every guest gets plain FUSE read/write.

---

## Tests — flakes & coverage

### `l1_silent_agent_is_reported_and_recovers` has no timing margin
Fails about 9% of the time run solo (1 in 11 isolated runs, measured 2026-08-14), so load is not the cause. It sets
`LIMINA_AGENT_SILENT_SECS=1` (`l1_liveness.rs`) and the liveness sweep also runs every 1 s, so
"silent for 1.0 s" is a phase tie and the healthy seed agent `limina-init` is sometimes reported
silent, then "heartbeating again" a second later. Fix: give the threshold margin over the sweep
interval (threshold ≥ 2× sweep, or a shorter sweep under test). The balloon is not involved
(`GuestConfig::l1_from_env` sets `memory: None`).

### libkrun `sweep_fault_handler_fields_concurrent_touches` depends on a timing collision
In `hvf/src/released_ram.rs`, run by `cargo test -p krun-hvf --lib` (not by `cargo xtask test`). It
passed 2 of 5 runs on a clean tree (2026-08-27), failing with `no toucher write collided with a sweep window in 50
sweeps`: it needs a racing write to land inside a sweep window it does not control. Make the
collision deterministic (hold the window open until the toucher has written, or count observed
windows and skip when there are none); do not just raise the 50.

### Seated venus replay stalls under suite load
Only under suite parallelism: the guest-side `eglretrace --headless` replay never prints `Rendered`
and the ssh bound gives up at ~956–959 s, in either `venus_replay_matches_llvmpipe_reference` or
`venus_shell_replay_matches_llvmpipe_reference`. Signature: the venus context is created, KK shader
work runs ~90 s, then the replay wedges at the first frame boundary and the worker log shows only a
1 Hz `capture: configure scanout` for ~15 minutes. Three sightings (2026-08-12, 08-13, 08-27), each in a suite run that took
3094–3343 s against ~2200 s for a green run the same day; solo reruns pass in 63–164 s. Ruled out: a
granule effect, and the once-a-minute `vsock muxer: unexpected dgram pkt: 3` (libkrun's timesync
datagram reset by a guest with no listener). **Next occurrence, debug it live instead of rerunning:**
is `eglretrace` starved of GPU progress (read the worker log at the stall timestamps) or of vCPU time
(the harness runs several VMs at once)?

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
image, mesa-guest 0006 (the synchronous dma-buf import allocate) makes the refusal synchronous, so no
ghost is created, virglrs's ghost path (`Lookup::Ghost` → `Dispatched::Ghosted`) never runs, and the
test passes on the guest fix alone. The host half covers stock and older guests. Proving it needs an
env-gated, off-by-default fault-injection hook in virglrs that fails the Nth create of a named command
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
chain, and the perf battery runs `--window`; what no gate reaches is a parked classic (vrend) scanout.
Flipping the default for headless is small (with `ack_active` false the cookie leaves `unconfirmed` at
present time and the hold ends after `latch_delay`), but first: the flip also arms venus parking
headless (`try_park_present` checks `fence_present_enabled()` first), changing present timing for the
boots the video corpora and fluster goldens were recorded from — re-verify or re-record those; and
`LIMINA_FENCE_LATCH_MS` defaults to 35, tuned for CoreAnimation, which headless would cap near 28 fps —
pick a delay for the PNG encoder. After the flip: fluster verdicts and replay-corpus scores unchanged,
and the frame oracle reaching the parked path with no knob set.

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
- A comparison between two builds needs N runs per arm when the failure is intermittent. When a
  failure is stochastic, repeat an arm before believing it.
- Never change two variables to make an arm cheaper, and never run an arm without a capture.
- A gate that cannot reach the shipped path reports on something else while sounding as if it
  reported on this. Confirm its configuration arms the path it claims to score.
- When two independent fixes uphold one invariant (guest-side and host-side), each needs its own
  vehicle. A test that passes because of one proves nothing about the other.
- The limina-test harness runs pre-built binaries from `target/debug/`; `cargo test -p limina-test`
  does not rebuild them. An A/B of shipped behaviour must rebuild and re-codesign between arms.
- A test harness must read the ssh port the supervisor allocated, never assume 2222. When guests share
  credentials a wrong-VM connection is invisible, so prove guest identity with a per-handle marker.
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
- A guard's doc comment must match its measured trajectory, not an endpoint reading. A guard that never
  binds, documented as coverage, is worse than none.
- A high-water-mark counter must alert on growth, not level, or it fires forever after one episode.
- A deadlock guard is only as true as its premise about *who* is waiting: a wait issued from outside
  the stream must not reuse the stream's deadlock verdicts.
- When two blocking waits each need the other's producer, make each blocker visible to the other and
  poison deterministically. A timeout trades the wedge for killing healthy slow waits.
- When deferring delivery of a result, name the party that blocks on it. If nothing waits, the deferral
  is not latency, it is handing out stale data.

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
- A snapshot records the virtio-mmio device layout (type, base, irq), and restore refuses a machine
  whose devices moved while keeping the suspended session. Any change to the spawn-time device list is
  a one-way door for parked VMs.
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
