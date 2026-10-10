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
(`64aee92`) are both in, so this is a path that skips both or a macOS unhide that did not take.
Catch it with the poke-VM trace env on.

### Needs repro: the menu-bar reveal drops while a macOS menu is still open
Observed on the dogfood Mac (2026-08-22): a small downward move with a menu open releases the ask and the chrome retracts under the open
menu. The ask is slaved to `NSMenu::menuBarVisible` (`InputState::menubar_observed`) and released by
`reveal_step`; which of the two lets go first has not been measured.

### Needs repro: `notch = extend` may not hide the band when the reveal triggers ungrabbed
While macOS has the menu bar out, the strip overlay must not cover its band. Uncaptured, with the
pointer at a panel's top, `InputState::menubar_observed` (run every tick) grants the reveal and
`reconcile` stands the overlay down. A reveal with the pointer elsewhere (a Space switch, an
app-initiated reveal) skips the grant by design. Which ungrabbed trigger leaves the band covering
the bar is not established; catch one before changing anything.

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

### UEFI variables survive a torn write only by being reformatted
The variable store is a file mapped into the guest (`docs/design/efi-vars.md`, mechanism B): every
write lands in the file through the page cache, but there is no fault-tolerant write, so a host
power loss mid-update can leave a store whose header the firmware rejects, and it then formats a
new one. Today that costs one more shim fallback boot. It loses enrolled keys once Secure Boot
keeps them there. Fix shape: ArmVirtQemu's CFI flash (`VirtNorFlashDxe` + a CFI device in
libkrun) with the FTW working and spare blocks `VarStore.fdf.inc` already lays out; do it before
Secure Boot ships.

### Inventory the firmware features the guest probes and gets `NOT_SUPPORTED`
Each declined feature is guest behaviour inherited by default instead of chosen. PSCI 1.0 +
`SYSTEM_SUSPEND` are offered (libkrun `hvf/src/lib.rs`, gated by `LIMINA_PSCI_SYSTEM_SUSPEND`).
Owed: the rest of PSCI 1.x (`SYSTEM_RESET2`, `CPU_FREEZE`, `CPU_SUSPEND` idle states, the full
`PSCI_FEATURES` answer set), SMCCC/`ARCH_FEATURES`, PPTT/topology, and anything else a guest
probes — decide offer or decline for each and record it.

---

## Suspend/restore

### The worker's quiesce budget plus a slow save can outrun the supervisor's bracket timeout
The worker waits up to 45 s for the guest to quiesce (`QUIESCE_TIMEOUT`,
`crates/limina-vmm/src/krun/mod.rs`) and then writes the snapshot; the supervisor abandons the
bracket at 60 s (`SUSPEND_BRACKET_TIMEOUT`, `supervisor.rs`). A guest that quiesces late and a
large RAM dump can together pass 60 s, so the supervisor reports the suspend abandoned while the
worker completes it. Fix: have the supervisor's bound start at the worker's quiesce verdict (or
extend while the worker reports a save in progress) rather than at the request.

### `limina ls` shows a parked VM as running
A supervisor parked behind the play button after a window-menu suspend holds the run lock, so
`limina ls` reads the VM as running, never suspended, although its `[suspended]` record is
written and the runtime socket reports `parked`. Fix: let the listing consult the runtime socket's
parked state (or the record) before the lock.

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

### What is left of the Game Mode clamp now that the worker is out of the app's tree
While a games-category app is fullscreen and frontmost (Moonlight is one: `LSSupportsGameMode=true`),
`gamepolicyd` turns Game Mode on and clamps every process in an app's process tree to priority 4:
timers ~100 ms late, CPU denied, CoreAudio callbacks stalled for up to a second
(`spikes/game-mode-throttle/RESULTS.md`). The worker is no longer in that tree: a transient launchd
job (`ProcessType=Interactive`) starts it and the supervisor hands it its fds over Mach
(`crates/limina-launch`; `l1_worker_lineage` asserts the lineage). What remains:
- **Confirm it on the dogfood Mac.** The suite cannot turn Game Mode on. The oracle is `ps -M -p
  <worker>` reading 31 (and `97R` for the banded vCPU) while `gamepolicyd` logs `Game mode status is
  now on`, with smooth guest video and audio beside a fullscreen Moonlight.
- **The supervisor stays clamped** (it owns the window), so its present path into the window still
  runs late under Game Mode. Measure what that alone costs.
- **Identity.** TCC attributes mic and camera use to the process responsible for it, now the
  launcher job rather than Limina.app. The worker is signed as `eti.noronha.limina` with the app's
  Info.plist, so grants should land on the same identity; recheck audio capture from a
  Dock-launched app.
- **Native Mach channels are not owed for reach.** The control line protocol and the input
  datagrams are unnamed socketpairs carried across as fileports, so nothing else can reach them.
  Their consumers sit inside libkrun (the virtio-input backends poll an fd, the shown-ack reader in
  `virtio_gpu.rs` reads `LIMINA_SHOWN_ACK_FD`), so moving them to Mach messages means teaching
  libkrun's event loop to wait on ports (`EVFILT_MACHPORT`); it would buy whole-message writes and
  fewer fds, nothing more.
- **Strict timers on the present and audio paths**: the `gpu latch` thread's `thread::sleep` to its
  35 ms deadline (`virtio_gpu.rs`, `LIMINA_FENCE_LATCH_MS`), and anything else there. Still worth it
  for the supervisor's side, which stays clamped.
- **Detect and explain**: our own threads at priority 4 are a direct oracle. Tell the user, and
  name the game-side levers (the menu-bar toggle, which held across re-fullscreens;
  `LSSupportsGameMode=false` in apps they build).

When launchd cannot be used (no gui domain, `launchctl` refusing), the supervisor posix_spawns
the worker as before and logs that Game Mode will clamp it. `LIMINA_WORKER_LAUNCH=spawn` forces
that path.

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
- **Thread-level levers not yet measured for this effect:** the vCPU threads' QoS class, an
  `os_workgroup` joined by the vCPU threads (CLPC's input for interval workloads), and the RT band
  itself. A mostly idle RT thread runs on E, so the band may worsen this on vCPU 0 while it fixes
  timer lateness.
- A guest `idle=poll` A/B would confirm the utilisation theory, but not with as many vCPUs as host
  cores.

Knock-ons: the grow rule reads worker CPU time (`HostCpu` in `crates/limina/src/vcpu_policy.rs`).
An E-placed vCPU burns more host time per unit of work, which biases that rule. It may also be part
of *Host-CPU cost of a busy VM is unattributed* below.

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

### A virglrs panic ends 3D for the rest of the VM's life
A panic inside virglrs is caught at the rutabaga boundary: every call into the renderer goes
through `Guarded` (libkrun `rutabaga_gfx/src/virgl_renderer.rs`), which catches the unwind, marks
the renderer dead, refuses every later call with `EIO`, and leaks the renderer instead of dropping
it. The containment is the whole renderer, not the context that panicked: the tables, the GL
context and the venus decoder are shared, so nothing establishes that one context's damage stays
in it. After a panic the guest keeps its disks, network and console, but loses 3D, and the fences
pending in the renderer at that moment never retire (new ones are retired as lost by
`virtio_gpu.rs`). Not caught: panics on virglrs's own threads (venus rings, decode, fence
waiters), and panics in Rust callbacks entered from C (VideoToolbox, GL debug output), which abort.
Next: decide whether a dead renderer should retire its pending fences so the guest's compositor
fails cleanly instead of waiting, and whether per-context containment is worth a virglrs audit
of what a context's handlers can leave half-updated. `spikes/piglit-virgl/` is the regression
check for guest-reachable aborts on the GL path.

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

### piglit's buffer and texture-transfer groups on classic virgl: what upstream vrend shares
The same 1871-test selection (piglit `quick`, buffer/PBO/texture-transfer groups) ran on two
stacks, measured 2026-10-05:
- **Upstream rig:** stock QEMU 10.2 + virglrenderer 1.3.0 on an Intel host, guest Mesa `main`
  b39d173ca93, surfaceless (125 tests never reached the driver there). Run in
  `spikes/upstream-repro/virgl-pbo-upload-wait/`.
- **limina:** stock and enhanced F44 guests on virglrs vrend over zink-on-KK, gbm. Run in
  `spikes/piglit-virgl/`.

**Fail on both stacks** (guest Mesa or vrend behaviour, so upstream candidates):
- `copyteximage 3d`.
- `arb_get_texture_sub_image-get`/`-getcompressed` on compressed 2D. With Fedora's Mesa,
  `-getcompressed` segfaults in the guest.
- `getteximage-targets 2d_array s3tc`: zeros from layer 6 on.
- `fbo-readpixels-depth-formats`: float depth reads 0.999985 for 1.0, a 24→32-bit expansion off by
  0x80.
- `arb_shader_image_load_store@early-z`: half the occlusion count.
- `arb_shader_image_load_store@invalid`: out-of-bounds and invalid-format image atomics return
  garbage instead of zero.
- `ext_transform_feedback2@draw-auto offset`.
- `arb_texture_buffer_object@max-size` (128 MiB, flaky on the rig).

**Fail only on the rig** (pass on limina):
- `arb_transform_feedback_overflow_query-basic` and `arb_query_buffer_object@qbo`.
- `getteximage-targets cube_array s3tc`.
- `teximage-colors` RGB 3_3_2, off by one LSB.
- `tessellation triangle_fan flat_first`.

### piglit's limina-only failures are now mostly vrend's GLES flavour
Same selection, rerun 2026-10-06 with KK `43152beb621` (`spikes/piglit-virgl/`). The stock and
enhanced guests still fail the same tests, about 100 on limina's host that pass on the upstream rig.
Running each one in the enhanced guest under zink-on-venus as well
(`MESA_LOADER_DRIVER_OVERRIDE=zink`, which reaches KK without vrend) splits them:
- **Pass under zink-on-venus, so they are in virglrs vrend.** Its GLES 3.1 flavour has no geometry
  shaders or tf3, and lacks image caps. That accounts for the image load/store, SSBO and most
  transform-feedback failures, plus BPTC float uploads, `getteximage-formats`, `pos-array` and DSA
  `transformfeedback-buffer*`.
  - Texture buffers reach it since KK emulates R32G32B32 texel buffers (KK `c76a16a26f8`), which
    gives host zink `OES_texture_buffer`; 22 texture-buffer tests pass under vrend. Still failing
    there: the legacy ALPHA, LUMINANCE, LUMINANCE_ALPHA and INTENSITY formats in
    `arb_texture_buffer_object@formats (fs|vs, arb)`.
  - All 27 timeouts (300 s) are indexed draws under transform feedback, which GLES refuses. virglrs
    de-indexes them since `6c6a60b` (pinned); the selection has not been rerun on it.
  - virglrs's desktop flavour passes 69 more of these and regresses 26 uploads; which flavour ships
    is undecided.
- **Fail under zink-on-venus too, so they are in KK or zink:**
  - `arb_draw_indirect-draw-elements-prim-restart-ugly` (passes on virglrs's desktop flavour).
  - `arb_shader_image_load_store@host-mem-barrier`.
  - `ext_transform_feedback2@counting with pause` and the five geometry-shader xfb tests: see the
    KosmicKrisp entries below.
- **Fence waits that give up.** "waiting got error - 16, slow gpu or hang?" after 15–40 s turns a
  pass into a fail on either guest. `vbo-subdata-*` fail this way in some full runs and pass 3/3 run
  alone (`spikes/piglit-virgl/rep.sh`), so treat a lone difference with that line as a host stall.

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

### Single virtio-gpu ioctls stall for 100 ms to seconds under a canvas workload
Firefox's canvas thread on a stock-session limina guest (M1 host, virgl over zink-on-KK, Basemark
Canvas Test, guest Mesa `26.2.3-3.limina`) logged 27 ioctls over 100 ms in 8 traced 15 s runs —
about 3 a run, typically 100–475 ms — spread over `RESOURCE_CREATE`, `GEM_CLOSE`, `EXECBUFFER`,
`MAP` and `WAIT`, on the PBO and the CPU-pointer arm alike (measured 2026-10-05). Two runs opened
with multi-second ones in their first 100 ms (`VIRTGPU_MAP` 7.3 s, `RESOURCE_CREATE` 3.4 s, both
under `BufferData`). Nothing guest-side distinguishes the stalled calls from their thousands of fast
siblings, so the cause is likely host-side: the virtio-gpu control queue behind another context, a
virglrs resource create or destroy that blocks, or the worker's own scheduling. Next: catch one with
`LIMINA_GPU_TRACE` and the worker log at the matching timestamp; the
`vkWaitRingSeqnoMESA` entry below is one candidate for the queue head.

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

### piglit's `ext_timer_query-time-elapsed` hangs the GPU (root cause OPEN; copy-dispatch mitigation insufficient)
On the vrend tier (vrend → zink → KK), the piglit binary `ext_timer_query@time-elapsed` hangs a Metal
command buffer — `kIOGPUCommandBufferCallbackErrorHang` (MTL4CommandQueueErrorDomain Code=1), zink then
reports the device lost. After two GPU restarts the kernel logs `Deny submissions/ignore app[limina-vmm]`
and drops that worker's later GPU work, so everything after it in the VM stalls; a new worker gets a
working GPU. `arb_timer_query@query gl_timestamp`, run first, passes before the ban. Any guest GL app
timing frames with `GL_TIME_ELAPSED` can trip it. A GPU hang is host-wide: reproduce only on an
otherwise idle host. Vehicle: `spikes/piglit-virgl/ab.sh <disk> <kk icd> ext_timer_query@time-elapsed <out>`;
the host-GPU oracle is `log show --predicate 'eventMessage CONTAINS "GPURestart"'` plus the worker log's
`kIOGPUCommandBufferCallbackErrorHang` + `[LIMINA-DEVICE-LOST]` ring.

Reproduces readily in-guest on a quiet host — NOT a special-environment interaction. On a spare, idle
M1 Mac mini (8-core, 16 GB) it hangs with a single test, headless (`--display-capture`), within ~1 s of
the `ext_timer_query` GPU context creating. The device-loss ring is dominated by the test's own
`render 160x160 ... draws=1` interleaved with `compute` encoders, and the failing command buffer shows
`gpu=0.000000..` — it never started; the queue was already wedged at its head (a victim, not the cause).
(A faithful bare-metal raw-Vulkan reproduction has not been built; `ts-probe`'s `ts-copy` passes, which
only means that probe does not reproduce the shape, not that the bug needs more than one in-guest test.)

The copy DISPATCH is not the root cause, and the "resolve-direct-to-dst" mitigation does not fix it.
A/B on that host (verified 2026-10-10), control vs. mitigated KK, identical env (spawn worker, headless,
same clone recipe):
- Control (unmitigated `build-kk`): the mapped dylib was confirmed by `lsof` (15097240 B); hangs on the
  first `ext_timer_query` `render 160x160` while desktop renders are still live in the ring.
- Mitigated (`build-kk-kkq`): `kk_CmdCopyQueryPoolResultsToMemoryKHR`, for a timestamp-pool copy with
  `VK_QUERY_RESULT_64_BIT` and no availability/partial, resolves the counter-heap entries straight into
  the destination (`mtl_command_resolve_counter_heap` → `pDstRange->address`) and skips the compute copy
  dispatch. It FIRES — within the failing command buffer the mitigated ring is `compute, render160,
  compute` where the control's is `compute, compute, render160, compute, compute`, and the hang lands
  deeper into the test's own `render 160x160` loop rather than on its first render — but still hits the
  same `kIOGPUCommandBufferCallbackErrorHang`, same `gpu=0.000000..`. (The mitigated dylib's load is
  inferred from that differential ring, not separately `lsof`-confirmed.)
So removing the copy dispatch is correct but insufficient; the wedge is intrinsic to the timer-query
render + counter-heap-timestamp (`vkCmdWriteTimestamp2`) loop itself. Leading suspects to probe next,
not yet confirmed: the per-iteration compute (staging) + render + counter-heap write cadence on one KK
queue, and residency of the direct-resolve device-address target (the ring's residency set lists
`0 buffers`; the old path resolved to the pool BO, resident by construction). (A prior in-guest A/B had
read "skipping only the dispatch clears the hang"; this control/mitigated A/B — same host, same env,
control dylib `lsof`-confirmed, distinguished by the differential ring rather than one SKIP run —
overrides it. The prior read was on a different host and GPU, windowed, under another VM's load; treat
a single-arm SKIP run as inconclusive.)

The mitigation commit is byte-correct (`resolveCounterHeap` writes the same uint64 the 64-bit copy
kernel would; the pending pool-BO resolve still folds, so `vkGetQueryPoolResults` stays correct) and
removes an unnecessary dispatch, so it stands on `kk-hardening` as a correct-but-insufficient cleanup —
not "the fix". Local/unpushed; the kosmickrisp manifest pin is NOT bumped for it. The copy kernel
(`libkk_copy_queries`, `kk_query.cl`) is bounded — under `WAIT_BIT` it reads the slot and returns, no
wait loop — so it never hung in its own execution either way.

Repro venue note: a quiet spare host driven over ssh must run the worker with
`LIMINA_WORKER_LAUNCH=spawn` (the default launchd path leaves the guest dark over ssh — no serial, net,
qga, or present, vCPUs at 0%). If the host's Homebrew LLVM major differs from the one the zink/gallium
build linked, that is a confound only for llvmpipe/draw, which zink does not use for GPU work — low.
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
`1 << location`. A fix (53ed2368595, branch `limina-kk-output-mask`) and a poly change that computes
each GS input vertex index once (6e8b33cc5a4, `limina-kk-gs-vertex-hoist`, GS MSL about 3.7×
smaller, no behaviour change) wait for the next limina-kk change. The hoist needs the GS piglit
lists first: host probes do not cover adjacency or line inputs.

---

## Video

### AV1 has no host decoder on M1/M2, so AV1 playback there never counts as video
virglrs offers AV1 only where `VTIsHardwareDecodeSupported` says so (M3+), so on an M1 or M2 the
guest sees no AV1 profile and decodes with its own dav1d. The host decoder never runs, and the
display-wake heuristic (guest audio plus host decode, `window/wake_policy.rs`) never fires for an
AV1 video there. Measured 2026-10-03 on the dev Mac (M1 Max) with a stock Debian guest: Firefox
playing VP9 held the assertion, AV1 did not, and the guest's `vainfo` listed no AV1 profile. The
enhanced tier is covered by the relayed inhibitor, which Firefox registers whatever the codec.
Fix shape: port the C backend's dav1d fallback, which reverses the "there is one decoder, and it is
VideoToolbox" decision in `third_party/virglrs/docs/design.md`, so that decision's text changes
first. The constraints are recorded in `docs/design/av1-decode.md` ("Not ported: the dav1d
fallback"): replay from the last shown key frame, feed the serializer's own units, ask for
invisible frames, refuse cleanly. The same decoder would bring back the super-resolution fallback.
Costs to weigh: a new `unsafe` binding module, dav1d in the bundle, host CPU instead of guest CPU
plus a copy into the guest's surfaces, and the harness's two-leg equivalence across a decoder
switch. To be agreed with the virglrs session before anyone builds it.

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

### Hardware decode: what is still synchronous
VA-API decodes run on a thread per codec and every read of a target waits for its picture
(`docs/design/async-video-decode.md`). What remains:
- **A target lent to a venus context decodes synchronously.** A Vulkan read passes no host barrier,
  so a composite target whose surface was ever imported into venus keeps the old stall. Lifting it
  needs the guest to fence END_FRAME (mesa-guest, design phase 3) and the host to trust that per
  codec (phase 4). The same guest fence closes the existing race for a decode already queued when
  the lend happens.
- **Per-plane uploads still run on the control thread.** Only the VideoToolbox wait left it; the
  `glTexSubImage2D` of each plane happens when a reader settles the target. Measured on the stock
  tier, whose targets are all per-plane (`spikes/flush-latency/RESULTS.md`, "The stock tier"):
  0.15-0.46 ms a plane, ~0.9 ms a frame and 2.7% of the render thread at 4K VP9. Moving them off
  (unpack buffers, or a decode-thread GL context) can save no more than that.
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
rejection are covered by `l1_clipboard_multi_session.rs`.)

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

### An idle guest reads virtio-net `InterruptStatus` about 2,400 times a second
Measured 2026-08-27 on a stock F44 guest at a settled idle desktop with `--net`: 72,374 MMIO reads of
`0xa01f060` in 30 s — offset `0x060` (`InterruptStatus`) on `a01f000.virtio_mmio` → `virtio_net`.
virtio-blk (`0xa01d060`) was a distant second at 2,896; every other device was in the tens. MMIO
writes are not logged, so the true exit count is higher. Unknown whether this is gvproxy's normal
traffic, an ack pattern costing an extra read per event, or a genuine interrupt storm. Start by
rerunning the count with `--no-net`, and against a guest with no NAT traffic.

## TPM

### Debian's arm64 kernel has no driver for our TPM
Debian testing's `linux-image-*-arm64` (seen on `7.1.13+deb14`) builds `CONFIG_TCG_TIS_CORE=m` but
leaves `CONFIG_TCG_TIS` unset, so nothing binds `tcg,tpm-tis-mmio`: the device tree node and the
platform device appear, `/dev/tpm0` never does. The firmware still measures the boot and hands
the event log to Linux, so systemd waits for `/dev/tpm0` and `/dev/tpmrm0` until its 90 s
timeout, then boots without a TPM. That is the degradation every stock guest without `tpm_tis`
gets from `tpm = true`. Fedora builds every driver below; Debian's options, from its packaging
configs (`debian/config/config` and `arm64/config`, to be re-read on a booted kernel before
building on any of them):

- **TPM on SPI behind an emulated SoC SPI controller.** Debian has `TCG_TIS_SPI=m`, the generic
  driver (`tcg,tpm_tis-spi`), and builds no `SPI_VIRTIO` and no `SPI_PL022`, but Debian and
  Fedora both build `SPI_BCM2835`, `SPI_OMAP24XX`, `SPI_IMX`, `SPI_SUN6I`, `SPI_MESON_SPICC` and
  `SPI_TEGRA114`; BCM2835's is the simplest register model. TCG's SPI protocol carries the same
  TIS register accesses, so janus and the TIS register file stay as they are; the new parts are
  the controller (FIFO, chip select, a fixed clock node) and an interface switch: edk2 cannot
  speak SPI, so the firmware keeps the MMIO TIS for measured boot and must disable that node
  before the OS sees the tree, or a kernel with both drivers binds two TPM devices to one TPM.
  The one stock-kernel route found; a few days of work and a second OS-facing interface to test.
- **I2C: not viable.** Debian has no generic `TCG_TIS_I2C`, only `TCG_TIS_I2C_CR50` (expects
  Google's cr50 vendor ID: an impersonation) and `TCG_TIS_I2C_INFINEON` (believed TPM 1.2 parts
  only), and no `I2C_VIRTIO`, so it also needs an emulated I2C controller.
- **ACPI with a CRB TPM.** Both kernels build `tpm_crb` in, and edk2's Tpm2DeviceLibDTpm speaks
  CRB, but arm64 Linux takes ACPI or a device tree, never both: the whole platform would have to
  be described in ACPI. Out of proportion for this.

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
delivery but does not close it. Owed: carry one of the posted guest fixes on our kernel fork's
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
