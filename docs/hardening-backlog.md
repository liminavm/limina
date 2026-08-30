# Hardening / finish-what's-shipped backlog

Consolidated "finish what's shipped" punch-list (cross-cutting, drawn from the milestone sections in
`docs/roadmap.md`). These are loose ends on **already-shipped** milestones (M1–M5) plus the cheap M8
polish wins — closing them rather than opening a new milestone. Each entry points at the roadmap
section / file where it's detailed. Prioritized roughly by user-visible value; pick top-down or by
appetite.

Done first (2026-06-23, with user): **runtime window resize** — ✅ SHIPPED, see below.

## Display / window
- **The captured cursor can go undrawn on the display the user is looking at.** Dogfood
  2026-08-24, cold-booted two-display session, **rare**: the pointer moved and GNOME overview
  fired, but nothing was drawn while captured; `Ctrl-Alt-F1`/`F2` fixed it permanently, and the
  *uncaptured* pointer wore the guest's shape throughout.
  What the evidence already excludes. The IOSurface path: `building guest cursor from IOSurface
  … failed` appears **0** times in that log, and the worn shape uses the same map and lookup.
  A stale per-slot `cursor.id` across a plane migration: traced on a working two-display capture
  (2026-08-24, stock guest), the guest sends `hide` for the old slot AND a fresh `shape id=` for
  the new one on every crossing. And any momentary lookup miss: `update_capture_cursor` re-reads
  the state every tick, so only *stored* state can persist a hide until a modeset.
  What is left is the slot disagreement. The captured layer draws each window's OWN slot and
  hides when that slot's `cursor.visible` is false — while the worn shape takes **whichever**
  slot has a plane (`shape_slot`). So a guest whose cursor plane sits on the other CRTC leaves
  the user's own window hidden and the worn pointer looking perfectly healthy, which is exactly
  the report. That is not a hypothetical state: `shape_slot` exists because the guest's cursor
  is on a different display from the host pointer while the absolute device's per-display shares
  are still being learned — a fresh two-display boot, which is when this was seen. Confirm by
  reading `[CURSOR] slot=N hide` against the capture slot at the moment of the report, then
  decide whether the captured path owes the same tolerance `shape_slot` gives the worn one, or
  whether the honest fix is upstream in the position.
- **The guest-cursor echo check judges against the IDENTITY mapping, so with two displays it is
  unreadable.** `echo::verdict` compares the guest's cursor plane to
  `echo::expected_pixel(unit, scanout)` = `unit × scanout`, which is only the right expectation
  when the absolute device's whole range covers one display. It does not: the range spans the
  guest's entire desktop bounding box, so every slot's share is a *fraction* of it and the true
  expectation is the fitted line `absfit` already holds (`pixel = a·u + b`). The failure is not
  loud — the check either warns about an agreement it mis-computed, or (2026-08-24, chasing the
  seam hold) falls into `Ok(None)` and goes silent in a way that reads exactly like "the guest
  agrees". A diagnostic that lies in both directions is worse than none. Fix: expect the fitted
  pixel where a fit exists, keep the identity fallback only for the unfitted case, and say which
  one was used in the message. Diagnostic-only — nothing in the pointer path reads the verdict.
- **The suspend overlay reached only the primary window; a secondary just froze — FIXED
  2026-08-22.** Noticed on the rig while testing park/resume on two panels: the snapshot save
  takes ~13 s, and for all of it a second panel simply stopped updating with nothing to say why.
  Each secondary wears its own instance now (`GuestWindows::veil`), driven from the same tick
  block as the primary's so they rise and fall together — including when the bracket is abandoned
  and the VM keeps running. **Only the suspend flavor can reach a secondary**: parked and
  resuming both happen after the park has closed every secondary window, and the restore splash
  predates their existence. Rig-confirmed on both windows.
- **A parked (or resuming) window grabbed and blanked the pointer on every visit to its Space —
  FIXED 2026-08-22.**
  Measured 2026-08-22: three grab/release cycles in two minutes on each of two suspended VMs,
  one per Space visit — `pointer capture: taken — the guest gained the screen (fullscreen=true,
  panels 0 -> 1)` while no guest process exists, each followed by the blank being re-worn on a
  window with nothing behind it. The user sees the cursor vanish over a suspended VM; Cmd-Ctrl-G
  is the only way back. Both *event* paths already stand down while parked — the CGEventTap
  returns early (`capture_tap.rs:261`, "the tap must claim NOTHING") and the NSEvent monitor
  forwards nothing but the resume click (`mod.rs:3414`) — but the **tick** has no such gate, and
  `grab_on_screen_gain` (`input.rs:1055`) fires because a parked fullscreen window does gain the
  screen every time its Space comes forward. Its refusal chain asks whether the grab is off,
  whether the pointer is already ours, and whether the pointer is over guest content; it never
  asks whether there is a guest at all. Fix: give the refusal chain the park fact (a pure verdict
  in `grab_policy`, unit-testable), and gate the tick's remaining guest-pointer work — echo
  follow, repark, wear verify, mapping probe — on the same `parked()` the tap uses, since none of
  it has a guest to serve — and the gate is "no live worker", not "Parked" alone, because the
  grabs continued through the Resuming phase where `parked()` is already false
  (`window::speaks_for_a_guest`). Rig-confirmed: **0** grabs across two parked windows, one of
  them held ~90 s while panels were switched, against three grab/release cycles in two minutes
  before.
- **The pointer cannot be drawn for the first ~350 ms of a Space-switch animation, and every
  public signal that would let it says so too late.** Measured 2026-08-22 on six flicks: a
  three-finger Space switch animates for ~530 ms, and `isOnActiveSpace`, key status and
  app-active all move at the switch's **commit** — so a captured pointer stays hidden,
  disassociated and parked for the whole animation, and appears only once it settles. macOS
  itself draws a moving cursor throughout an ordinary Space switch, so the behaviour is
  achievable; we simply cannot see the transition start. The one public signal that leads is
  `NSWindow.occlusionState`, which loses `.Visible` **175–200 ms before** the flip (174, 175,
  184, 188, 170, 201) — because it means "any pixel of our window still shows", so sliding OUT
  we stay visible until about two-thirds through, while sliding IN it turns visible ~535 ms
  ahead of the flip. Hanging the release on it would return the pointer for the last third of
  the animation; judged not worth the extra release trigger on its own, and parked deliberately
  rather than half-fixed. Trackpad gesture events ARE visible to the tap (`NSEventType` 29, the
  burst starting ~530 ms before occlusion drops, which would cover the whole animation) but fire
  for every gesture including two-finger scrolls, so using them means classifying by touch count
  — a heuristic the grab should not hang on, and one that would still miss Ctrl-arrow and
  Mission Control. Remaining lever if this is ever picked up: a private CGS space-change
  callback, unpriced, and likely to fire at the commit like everything else.
- **A secondary window left fullscreen when the session handed over to gdm — FIXED 2026-08-22.**
  `scanoutgone` was closing the window, and the guest sends one for every ordinary modeset: the
  logout trace showed the second slot re-modeset twice over (3024x1960 → 1920x1440 → back) with
  the window torn down and rebuilt each time, which also fired a `pointer capture: taken — the
  guest gained the screen` the user never asked for. A dark slot now keeps its window and its
  Space; only the slot table takes a window down (`windows::slot_fate`). The re-entry that was
  supposed to give fullscreen back — and sometimes did not — is gone from this path entirely,
  along with the duplicate `toggleFullScreen` that `SecondaryWindow::open` used to make before
  `apply`'s own restyle. Rule recorded in docs/graphics.md §"A panel owns a slot". Confirmed on
  the two-panel rig over two logout/login cycles: 5 modesets on slot 1, 1 window opened (the
  deliberate one, when the panel was switched on), 0 closes, no unasked-for grab.
  - Still open, and a different mechanism: a **reboot** does drop a fullscreen secondary, because
    the firmware phase collapses the pool to slot 0 (`DisplayTable::wanted`) and the table really
    does dismiss the other slot. Nothing mirrors the console onto the other panels, so there is
    nothing to keep the window up *for*. If a fullscreen VM should stay fullscreen on every panel
    across a reboot, that is a mirroring feature, not a lifetime fix.
- **NEEDS-REPRO: the pointer does not appear immediately after a logout/login cycle — seen under
  SYNOIK, not mutter.** Observed on dogfood 2026-08-22, intermittently. Which side owes the fix is
  open: a Vulkan compositor drives the cursor plane differently from mutter (see §Stock virtio-gpu
  formats in `docs/images.md`), and a session restart is exactly where a plane left armed or a
  shape never re-uploaded would show. Note the standing rule — **a mutter "cannot reproduce" is a
  FALSE NEGATIVE here**, so this must be chased on the synoik image
  (`Fedora-Workstation-44.enhanced.synoik.raw`, cloned), not the mutter one. The guest synoik
  session is a peer and can be asked what its side saw. **One clean cycle on a synoik poke VM
  2026-08-22 did NOT reproduce it** — the trace across the logout/login showed cursor-plane
  updates flowing, no stuck hide, and no grab-state anomaly. Weak evidence at best against
  something seen twice in a day of dogfooding; if it is chased again, do it in a LOOP (the plane's
  visibility is readable from the `[CURSOR] … visible=` trace, so the check can be automated over
  many cycles rather than eyeballed once).
- **NEEDS-REPRO: the reveal drops while the menu is still open, on a small downward move.**
  Observed on dogfood 2026-08-22. Moving the pointer down slightly with a macOS menu open releases
  the ask, so the chrome retracts out from under an open menu. The ask is slaved to the observed
  `NSMenu::menuBarVisible` (`InputState::menubar_observed`) and released by `reveal_step`; which of
  the two lets go first is unmeasured. Same vehicle and traces as the entry above.
- **A guest desktop that is not a rectangle had no edge class for the dead space, in two
  places — FIXED 2026-08-23.** A desktop is a union of rectangles, so any vertical offset or
  height mismatch leaves corners of the bounding box belonging to no monitor, and both the
  captured clamp and the pressure filter assumed the box IS the desktop. (1) `fit::range_step`
  clamped captured motion to `0..=ABS_MAX`, the ends of the whole box, so a display whose top is
  not the box's top could be pushed past its own edge into dead space, where the pointer is over
  no output and — the cursor plane being per-scanout — nothing draws it. It now clamps against
  the guest's reported desktop (`arrangement::Desktop::confine`): a candidate that lands on a
  monitor is taken as it is, so seam crossing survives; one that lands nowhere is clamped
  against the rect the *previous position* occupied, never the capture slot's, which the echo
  leaves a step behind after a crossing. Every clamp then happens at a wall by construction, so
  the pressure charges where the hand is pushing and needs no filter. The gain moved to the same
  geometry — the slot's share of the range over its share of the window — retiring an estimate
  that assumed a top-aligned row of scanout-sized monitors. (2) `arrangement::outer_edges`
  called an edge outer only when it sat at a bbox coordinate, so an offset monitor's leading
  edges read as seams and `Edges::keep` dropped the push: a wall the guest holds the pointer
  against that charges nothing. `outer_edges_at` takes the position and asks what is on the
  other side of the edge *there* — no per-side answer can be right for an edge that is a seam
  over the span its neighbour covers and a wall over the rest.
  Measured on the two-panel rig 2026-08-23, guest at BenQ `(1512,0) 2048x1152` and built-in
  `(0,747) 1512x948`: **0 of 2374** absolute positions landed on no monitor, the deepest reach
  on the BenQ was y=1152.0 and the shallowest on the built-in y=747.0 — both walls to the unit —
  and **53** upward pressure events charged at the built-in's top, where the old box clamp had
  nothing to charge with until range y=0. That run was fullscreen, so it verifies (1) only: the
  captured path takes its pressure from the confinement and never consults `outer_edges_at`,
  whose end-to-end differential is *uncaptured hover* (plain motion, no button — a press
  captures) and whose geometry rests on unit tests.
  (2) was then measured the same day, on the same rig: with `Virtual-1` held at `(0,948)` — so
  its top edge is never outer under the old `r.y == 0` test — the neighbour above its RIGHT half
  gave **96** upward pressure events at the top-left corner, and above its LEFT half **0**, from
  3033 absolute samples. Same corner, same free pointer, no capture transition in either window.
  Oracle and repro: `spikes/m15-ragged-desktop/`.
- **NEEDS REPRO — the released pointer can come back invisible.** Releasing a hard grab with
  Ctrl-Opt left no cursor drawn at all: the pointer was live and moving (pushing it up revealed
  the chrome, which restored the image) but nothing rendered until then. Seen once, on the
  two-panel rig 2026-08-23, fullscreen on both panels after a click had promoted the grab. The
  blank-wear check runs every tick and the unhide fix is in (`64aee92`), so this is either a path
  that skips both or a macOS-side unhide that did not take; the wear log line did fire earlier in
  the same session, which is why it is worth a look rather than a guess. Deliberately deferred —
  catching it again is the next step, which is why every poke boot carries the trace env.
- **`notch = extend`: the band is not hidden when the reveal triggers ungrabbed.** The strip
  overlay keeps covering its band while macOS has the menu bar out, so the revealed bar sits
  behind the very thing the reveal exists to get past. The grant path is there and works
  (`InputState::menubar_observed` slaves the ask to the observed `NSMenu::menuBarVisible`); what
  is missing is the band standing down for it in the uncaptured case. Small, and known.
- **The mapping probe could not run until the pointer had already been misplaced — FIXED
  2026-08-22.** The sweep gated on `capture_range`, which a taken grab clears and only captured
  motion writes, so it waited for the very stroke it existed to place correctly: on a first
  two-display session that stroke went through the identity mapping (slot 0 runs 1.738x ahead of
  it on the rig), which is the flick that lands on the wrong display. The gate was there because
  the sweep restored to a saved device number — one computed under the mapping the sweep then
  replaced, so restoring by it was a teleport. Both are gone: the restore re-places `capture_pos`
  through the mapping as it now stands, and the sweep runs grabbed or not, since the mapping it
  learns is the *uncaptured* pointer's. Two faults the ungrabbed sweep then exposed are fixed with
  it — `follow_guest_echo` chasing the sweep's own cursor (which warped the park across displays,
  dropped the grab mid-sweep, and corrupted the restore position), and `send_device` handing the
  verifier a device unit where a display unit belongs. Rig-verified end to end 2026-08-22: sweep
  starts on the panel joining with no mouse movement, both slots learned, restore lands at
  miss=0.0px, cursor confirmed on screen by eye.
- **The probe's corner avoidance is in the wrong space.** `absfit::PROBE_SWEEP` keeps `v` inside
  `0.30..0.70` *of the union*, and a display occupying only part of the union's height can have its
  top edge inside that band (the rig's slot 1 starts 172 logical px down). A step meant to be
  mid-screen can therefore land clamped on a display's top edge, and at `u = 0.05` that is a
  top-left corner: GNOME Activities. The sweep's guard test checks device space, which proves
  nothing about where a step lands on a display. Derive the safe band per slot from the lines, or
  keep the sweep inside a band no display can have a corner in. NOT the cause of "enabling Use
  Other Screens When Fullscreen always triggers the overview": that was the identity-mapped first
  flick the sweep could not pre-empt, and it is gone. Ten rig sweeps land no step near a corner,
  so this is a latent hazard of an arrangement we have not met, not an observed failure.
- **Clicks that do not grab, with the tap installed — open.** Reported 2026-08-22: some presses
  neither take the grab nor stand it down, and printed nothing at all. Every press the tap sees now
  logs one info line with the facts behind the answer (`pointer capture: click at (x,y) — grabbed=…;
  fullscreen=… key=… space=… on-screen=… grab-enabled=… latched=…`), and the no-tap path says so in
  its own words. Read those before theorising — the cause is not yet known, and the previous
  `[CLICK]` line only printed under `LIMINA_EDGE_TRACE` *and* only after the policy had run, so a
  press refused earlier was invisible. One concrete candidate now instruments itself: the system
  disables our event tap on timeout or user input, and the re-enable used to be silent — events in
  that gap reach the app untapped, so a click there takes no grab and logs nothing, which is
  exactly the reported signature. `pointer capture: the system disabled our event tap` now says so.
- **The probe's first pass is blind.** With no lines yet the ten steps are a fixed set whatever the
  union's division, so how many samples each slot gets is luck. Once one line exists each slot's
  span is known and the remaining steps could be placed *inside* each slot deliberately — which
  also fixes the corner problem above. On the rig it currently divides 6/4, which fits both slots
  well, but nothing arranges that.
- **The free pointer never re-grabs over a secondary window — CLOSED 2026-08-21.** It was the
  explicit-release latch (`user_released`), not the sampler: one Cmd-Ctrl-G latched the policy
  out, and on a fullscreen-everything Mac neither clearing edge (the pointer leaving guest
  content, the window regaining key) can ever arrive. The `[EDGE]` trace's `latched=true`
  said so. A click on guest content now takes the grab at once and clears the latch
  (`grab_policy::free_step`); the key-regain clear is gone.
- **Pointer hotspot: clicks registered slightly right of the visual tip — FIXED 2026-08-21.**
  The guest kernel sends the cursor plane's `crtc_x/crtc_y` (pointer minus hotspot,
  `virtgpu_plane.c:503`) with the hotspot separately; the captured-mode overlay subtracted the
  hotspot a second time, so the sprite sat hot_x/hot_y px up-left of the guest's cursor. Only
  the captured path drew it (fullscreen auto-captures, which is why dogfood saw it); NSCursor
  applies the hotspot once. Rig-verified: tip and click coincide in fullscreen.
- **Secondary cover: present at final size from the first frame.** Entering fullscreen covers a
  panel by `toggleFullScreen(None)` on a small centered titled window; AppKit's zoom animates
  the window frame while CA stretches whatever surface is current to each intermediate layer
  frame, so the guest content visibly scales into place until the guest re-modesets. All our
  own layer writes are action-disabled — the stretch is purely content tracking AppKit's
  transition. Polish (deliberately after multi-display is feature-complete): pre-size the
  window/layer to the panel before the toggle, or curtain/hold the layer until the transition
  settles and the guest's mode matches, so the content lays out and draws once, at the correct
  size. Levers are all host-side (`windows.rs` restyle + refit).
- **Relay units FIXED (513fe03, verified on the rig 2026-08-18): the arrangement relay reports
  the compositor's own logical rects via `zxdg_output_v1`.** The guest spreads the absolute
  range over its monitors' **logical** extents (measured, `spikes/pointer-units-oracle/RESULTS.md`);
  `wl_output` carries only integer scale, so the old mode/scale division was wrong by 1.6x under
  fractional 1.25 — the ~20% unreachable band and the offset hit testing. Verified: guest journal
  reports 2048-logical rects, wire seam pileup moved 0.458 → 0.575, full range reachable, clicks
  land true, and the post-rearrangement seam (0.425) tracked the relay's report. Delivered as
  payload r11 to all three F44 enhanced images. Without a report the host does not guess a
  layout: each window maps onto the whole range (`arrangement::abs_through_report`), exact for
  one display and the documented stock-tier floor for two — loud, not silent (the guest-echo
  check in `input.rs`).

  **The desktop model is RETIRED; the captured cursor follows the guest (2026-08-21).** The
  captured position is a running value in the device range, scaled from host deltas by a
  mode-width gain and clamped only at the range ends; the guest crosses its own seams, its
  cursor echo names the slot, and the fit used for drawing/press/release re-bases to it
  (`docs/input-and-windows.md` §4). The retired model — a desktop-space cursor confined to the
  union of monitor rects the host laid out itself when no agent reported — made every trigger
  follow a guessed layout while the sprite followed the guest. No host-side layout guess may
  return. Remaining gaps: the per-slot gain is the pixel-row approximation (a guest at 2× on one
  display moves its cursor at a different speed there — tune from the echo if it shows);
  uncaptured multi-display on a STOCK guest has no share model (each window maps onto the whole
  range; the `guest pointer:` warnings say where the guest put it) — fit the shares from the
  echo, or offer one panel without a report; a fullscreen-all held-button drag still follows
  AppKit's mouseDown-window routing.

  Two guest cursor defects surfaced on the rig (2026-08-19), both mutter's, **no limina time
  on either** (the compositor replacement retires mutter; very-low-priority upstream-report
  candidates): the "ghost cursor" guest half — after a REARRANGEMENT (scale changes do not
  trigger it) mutter software-paints the cursor while leaving the hardware cursor plane armed
  and stale; recovers on a CRTC crossing (`spikes/pointer-units-oracle/RESULTS.md` §5, with a
  `SUBMIT_3D → ERR_UNSPEC` kernel-error flood minutes earlier as an open lead; the host half,
  AppKit unhiding the parked NSCursor behind the hide refcount, is FIXED `64aee92` — the
  captured pointer wears the transparent blank, so a stray unhide shows nothing); and over the
  Settings arrangement diagram a pre-upscaled ~2× blurry arrow uploaded into the 64×64 cursor
  buffer (23×39 vs the arrow's 13×22, §6; every output at 100%, host path invariant).

  Related routes assessed: per-display virtio tablets are measured viable for pointing but
  blocked on the wheel (`spikes/per-display-input/RESULTS.md`); libei/EIS is the wrong layer
  for a VMM's pointer though its per-region model independently vindicates per-screen absolute
  devices (`docs/research/12-libei-emulated-input.md`).

  Read `docs/input-and-windows.md` before touching any of this.

- **NEEDS-REPRO: the composited cursor sprite drawn at the wrong position within the right
  display.** Photographed under the pre-fix wrong-rects relay (guest hardware cursor at
  internal pixel (503, 878) by hit-test truth; sprite at roughly (0.35, 0.40) of the panel vs
  the truth at (0.17, 0.46) — data in `spikes/pointer-units-oracle/RESULTS.md` §3), but **did
  not reproduce on the post-fix verification run** (user eyeballed the same rig). Plausibly
  downstream of the mismapped rects; do not chase without a fresh observation. If it
  reappears, the path is the per-window compositing (`window/cursor.rs`
  `update_capture_cursor` + `secondary.rs`) and the first suspects are a units/space mix
  (guest pixel position into a point-sized layer, or slot-local vs desktop coordinates).

- **PROBABLY CLOSED 2026-08-22, NEEDS ONE GUEST REBOOT TO CONFIRM (found 2026-08-03): the
  host-derived EDID identity does NOT survive a GUEST reboot.** The cause was found on the
  cold-boot version of the same fault: an identity pushed while the FIRMWARE owns the GPU may not
  survive the guest driver's device reset during probe, leaving virtio-gpu's default. A guest
  reboot re-enters the firmware phase (`reset_to_firmware`), so it is the same race, which is why
  it reports the same fallback identity. limina now re-announces the identity on every entry into
  the OS phase, reboots included. Verify by rebooting a guest and re-reading the monitor spec
  before believing this — the fix is reasoned onto the reboot path, not yet measured there.
  Original report follows.
  Observed on a windowed venus VM whose host display never changed (BenQ LCD attached throughout,
  confirmed via `system_profiler SPDisplaysDataType`): at first boot mutter reported the connector
  as `('Virtual-1', 'LMN', 'BenQ LCD', '0x6c42fae5')` — correctly mirroring the host display — and
  after a plain `systemctl reboot` **inside the same VM session** it came back as
  `('Virtual-1', 'RHT', 'krun-display', '0x00000001')`, libkrun's generic fallback EDID. The host
  side was not restarted; only the guest was.
  **Why it matters beyond cosmetics:** GNOME keys `monitors.xml` on the `<monitorspec>`
  (connector + vendor + product + serial). When the identity changes, mutter silently *discards*
  the saved configuration and re-picks a default scale — so a user's saved resolution/scale/
  arrangement is lost on every guest reboot, and it fails silently (no error, just a different
  display). It cost a perf run here: the run aborted on its display-pin verify because the config
  written before the reboot no longer matched (`scale=1.3333` instead of the pinned 1.0).
  Per-VM window/fullscreen restore is keyed on the same identity (`limina-display-modes`), so it
  is likely affected too — unverified.
  **Workaround used:** write `monitors.xml` *after* the guest reboot, then `systemctl restart gdm`
  (a session restart applies it without another reboot, so the identity cannot change underneath).
  **Not yet investigated:** whether this is specific to an explicit `--display-resolution` boot
  (mode overridden, so the host-match path may not re-engage), whether the first boot's identity is
  set once at initial scanout and not recomputed on the guest's re-probe, and whether a host-side
  window move/display change is needed to restore it. Reproduce before fixing — a one-shot is not
  yet ruled out, though the host display demonstrably did not change.
- **Capability-scope the scanout IOSurfaces** (security) — ✅ **DONE 2026-06-23 (sw2d + venus)**.
  The worker used to export each scanout as a machine-global `IOSurfaceID` any same-user process
  could brute-force-read (`spikes/venus-draw-probe/iosdump.swift` PoC). Now **both** display paths
  create their scanout/cursor IOSurfaces **non-global** and hand each one's Mach port to the
  supervisor (`limina-surfaceport`: `SurfacePortSender`/`Receiver`, bootstrap rendezvous), keyed by
  id; the supervisor resolves ids from the Mach map. `LIMINA_GLOBAL_SCANOUT=1` re-enables global for
  the debug oracle.
  - **sw2d/baseline path** — the `limina-display` `WindowBackend` publishes its ring. Spike
    `spikes/iosurface-machport`; commits `5980de2 8cafa44 13c6428 383460e`; RED-first test
    `non_global_scanout_is_hidden_from_strangers`.
  - **venus zero-copy path** — the renderer runs in the worker process, so `vkr_mtl_iosurface_alloc`
    publishes directly to the same receiver (no rutabaga/krun_display/virtio_gpu FFI). Patch
    `spikes/virgl-zink-kk/patches/virglrenderer-venus-iosurface-scoping.patch`;
    `LIMINA_SURFACE_PORT_NAME` env from `--surface-port-name`; commit `138d7f6`. Verified live
    (dev-enh + KosmicKrisp): the exact venus-allocated scanout ids (from `SET_SCANOUT_BLOB`) return
    "not alive" to a stranger while a `LIMINA_GLOBAL_SCANOUT=1` contrast dumps the screen; pure
    zero-copy (`LIMINA_PRESENT_COPY=0`) presents them via the Mach map with 0 "unresolved" skips.
- **A held seam leaves no trace at all.** `window/seams.rs` is pure policy with no logging, so
  after the fact there is no way to tell a seam that was held (adjacent panel not fullscreen, not
  on its active Space, or off-screen) from one that was never reached. Asked directly by the
  2026-08-24 multi-session incident below: the user's own reading was "the sweep ran while the
  secondary display was not the current macOS Space on its host display", and that is exactly the
  configuration that turns a seam into an edge — by design — but nothing in the log can confirm or
  refute it. Wants the `cursor::undrawn_fault` treatment: one episode-style line on hold engage and
  another on release, naming the side, the slot the range leads to, and which of the three coverage
  answers refused it. Cheap, and it converts "the pointer would not cross" from a re-run into a grep.
- **A cursor plane left enabled just outside a display's edge.** Long-standing and NOT the
  multi-session fault below — 31 occurrences in the 2026-08-24 dogfood log starting 02:01, in a
  single-session guest: `other slots also showing a cursor: [(1, (-10, 582))]` while the pointer is
  legitimately at the far edge of slot 0. The neighbouring slot keeps a visible cursor plane at a
  negative coordinate a few pixels outside its own scanout. Harmless for placement (the echo names
  the slot the pointer is really on) but it is a second cursor as far as `shape_slot` is concerned,
  which is the input to the undrawn-cursor fault above. Worth deciding whether the guest ought to
  hide it, or whether our echo should ignore a plane whose origin lies outside its scanout.
- **CapsLock/NumLock LED parity** — surface the statusq LED feedback (libkrun `worker.rs` no-op).
  Roadmap M8.

## Guest multi-session (two seat sessions on one VM, VT switching)

Diagnosed 2026-08-24 from the dogfood pair (the host's `supervisor.log` + the guest's journal), all
read-only. The user started a second seat session — their own compositor as another uid on tty3,
`systemd-run --uid=1002 … synoik --session` — and switched between it and their tty2 session. The
pointer teleported and its movement was clamped, in bursts that fall **exactly** inside the windows
where the tty2 compositor was paused, i.e. whenever the *other* session owned the screen.

- **A guest VT switch is invisible to us, and the arrangement report we keep is then the wrong
  session's — and it cannot be corrected.** This is the load-bearing one. Nothing changes on the
  virtio-gpu wire across a seat switch: same scanouts, same modes, same resources, no event — the
  same blind spot as a monitor repositioned. Two mechanisms then keep running against the previous
  session's geometry:
  - The report goes **stale rather than absent**. `layout_gate` holds while inactive, which is
    correct *when the incoming session also runs a helper* — it claims the report on becoming
    active. The tty3 session ran none: `limina-agent-session.service` is
    `WantedBy=graphical-session.target`, and a compositor launched straight from `systemd-run`
    never reaches that target. Measured: the `report:` / `arrangement:` values in every warning are
    byte-identical through the whole incident.
  - **A present report outranks everything and is never demoted.** `absfit::abs_position` consults
    the fitted lines only `if !has_report()`, so absfit's contradiction/refit machinery — the one
    mechanism that could have noticed — was locked out for the entire episode. Hence the
    *teleporting*: sends of 2452 px off, and `we sent the pointer to slot 0 … the guest shows its
    cursor on [(1, …)] and none on slot 0`. Hence also the *clamping*: `desktop_in_range` and
    `range_shares` build the captured range's confinement and the seam rule's shares from that same
    stale report, so a captured pointer was held inside the other session's desktop rectangle.
  - It did not converge and then settle; it was simply *correct again on every return to tty2*
    (`layout_gate.poll` re-sends on the inactive→active edge) and ended for good when the second
    session was stopped.
  Fix directions, cheapest first: **demote a report the echo keeps refuting** (absfit already has
  the `CONTRADICTIONS` pattern; the report tier has no equivalent) — host-side, so it also covers
  the stock tier and any helper-less active session, which the two-tier refinement says is a normal
  partial state; **name the tier that produced the send** (report / fit / identity) in the
  echo-mismatch warning — `unit sent` is just the target re-expressed and discriminates nothing, and
  one word would have made this diagnosis a single grep; on the enhanced tier, have root
  `limina-agent` watch logind and tell the host the seat's active session changed, which is the one
  event the wire cannot carry; and reconsider the helper's `WantedBy`, or treat "no helper in the
  active session" as a reason to distrust the held report.
- **A `SUBMIT3D` storm across the DRM master handoff — unchased lead.** At 20:21:42, twenty-odd
  seconds after the first `chvt`, `ctx 25 submit_command -> Err("ErrRutabaga(ComponentError(22))")`
  repeats at ~4 KiB per command for as long as the handoff lasts. Consistent with a compositor
  continuing to submit while it no longer holds DRM master. Not investigated: it may be entirely the
  guest compositor's bug class, but the worker should be checked for what it does with a context
  whose submits fail in a run like that, and `ComponentError(22)` (EINVAL) should be traced to its
  emission site before anyone reasons from the message text.

## Lifecycle robustness
- **Windowed guest reboot** — ✅ **DONE (shipped `efa285f` 2026-06-13, verified live 2026-06-23).**
  A guest reboot in a window keeps the same NSWindow and relaunches the worker, re-wiring everything:
  input/ack fds (`WorkerConn::swap`), a fresh scanout/control reader (`spawn_reader`), gvproxy recycle,
  the resize listener (unlink+rebind), and the surface-port receiver (persists across relaunch; the new
  worker re-publishes its non-global scanouts). Verified: `systemctl reboot` over SSH → worker exit 125
  → relaunch → guest SSH back + window re-displays the desktop and stays interactible, 0 "unresolved"
  present skips, surface-port re-scoped. Regression guard: `worker_conn_swap_retargets_every_field`.
- **libkrun panic→graceful exit paths** — ✅ **DONE 2026-06-23 (libkrun patch 0028).** The aarch64
  HVF vCPU loop no longer `panic!`s on unhandled guest traps: an unknown PSCI/SMC function returns
  `PSCI_RET_NOT_SUPPORTED` and the guest keeps running (standard PSCI semantics), while every other
  unhandled trap (exception class, system register, exit reason, MMIO size) logs the specifics and
  returns `Error::Unhandled`, which `vstate::run_emulation` maps to a clean VM teardown
  (`FC_EXIT_CODE_GENERIC_ERROR`) instead of aborting the worker process. Healthy guests never hit
  these arms (L1 boot still green). RED-first: `spikes/hvf-trap-probe` (96-byte bare-metal arm64
  Image) + `crates/limina-test/tests/hvf_graceful.rs` — verified RED (SIGABRT) before, GREEN after.
- **surface-port `recv` leaks a port name on a malformed message** — cosmetic today, worth a line
  when that file is next touched. `SurfaceReceiver::recv` (`crates/limina-surfaceport/src/lib.rs`)
  returns an error on `descriptor_count != 1` **without** deallocating `msg.port.name`, so a
  complex message carrying an unexpected descriptor count would strand a right in our port space.
  Unreachable in practice: the only sender is our own worker and it sends exactly 0 descriptors
  (release) or 1 (publish), and the release path is discriminated earlier by the complex bit, so
  the branch has never run. Fix = `mach_port_deallocate` before the early return.
- **`vkr_dispatch_vkAllocateMemory` strands an IOSurface ref on one early return** — ✅ **DONE
  2026-08-08**, folded into the `VK_EXT_memory_budget` virgl commit as planned.
  `vkr_mtl_iosurface_lookup` returns a **retained** surface and only `IOSurfaceGetBaseAddress`
  sets `*out_base`, so a lookup that succeeded with a NULL base reached the
  `VK_ERROR_INVALID_EXTERNAL_HANDLE` early return still holding a +1 that nobody dropped —
  every *later* failure path already released it. Filed rather than fixed RED-first because
  the trigger cannot be produced naturally (a non-purgeable IOSurface we allocated always has
  a base address), so the fix ships as a defensive one-liner with no test. The stale
  "bind the TLS at the vrend entry" paragraph batched with it (in both `vkr_budget.h` and
  `docs/design/gpu-memory-budget.md`) is corrected in the same commit.

- **The control center snapshots VMs on the AppKit main thread** — `refresh` runs from an
  `NSTimer` on the main thread (`crates/limina/src/center/mod.rs`), so every per-VM `stat()` it
  does runs there too: `disks_line`'s existence check, and now the cheap-depth pre-flight behind
  `VmRow::blocked` (`docs/design/vm-start-preflight.md` §3.6). A dead network mount can block a
  `stat()` for seconds, which freezes the UI. Pre-existing rather than introduced — the cheap tier
  is deliberately stat-only, a few syscalls per VM — but it is the wrong thread for it. Fix =
  snapshot on a background thread and hand finished rows to the main thread; that changes the
  refresh architecture, not the pre-flight module.

## Guest-reachable aborts (a guest must never kill the VMM)

Two classes already landed as targeted fixes: the empty-clear-rect vk_meta assert
(kk 0009 + virgl 0045) and the render-pass-begin VU asserts, log-only per the user's call
(kk 0019, `spikes/kk-format-mismatch-abort/`). A THIRD instance of the clear-rect class hit
dogfood-mac 2026-08-04 (a compositor rect with a NEGATIVE offset wrapped past both shipped filters
into an inverted u32 rect → `vk_meta_draw_rects.c:163` assert) — **fixed 2026-08-04**: the
i64-math offset/overflow checks now live in both `vk_meta_clear_rect_is_empty` (mesa
`limina-kk` f7145c1263c) and the vkr sanitize (virgl `limina` 14c22c40); probe + L2 guard
extended (`spikes/kk-empty-clear-rect/`, `vkclearrect.py` — valid + empty + negative + huge
rects). Remaining:

- **Clamp (don't just log) the pass-vs-framebuffer attachment COUNT asserts** in
  `vk_render_pass.c` `begin_render_pass` (`attach_begin->attachmentCount ==
  pass->attachment_count`, `framebuffer->attachment_count >= pass->attachment_count`):
  with asserts off these are an OOB read of the attachment array, not merely undefined
  rendering — the loop must bound `a` by the *actual* array length. Deliberately left out
  of kk 0019.
- **The full "no guest-reachable aborts" audit** (scoped 2026-07-24): per-layer policy —
  asserts = internal invariants only; drivers TOLERATE (clamp/skip; `vkCmd*` can't return
  errors); vkr = THE trust boundary (validate → poison context); libkrun Rust decoders =
  error returns, not unwrap. Surface: 59 asserts in hand-written vkr, 178 in KK vulkan/,
  71 in vk_meta* (compiled into KK), 89 unwrap/panic/assert! in libkrun virtio-gpu.
- **A guest vsock connect storm kills the worker** (measured 2026-08-21: `limina-agent-session`
  wrote its `DisplayLayout` seed ahead of the HELLO, the host dropped the peer, the helper
  reconnected with no backoff — 396,747 connects in 150 s — and the worker died of `EMFILE` at
  `third_party/libkrun/src/devices/src/virtio/vsock/muxer.rs:633`, an `unwrap()` on the proxy
  socket creation; the reaper thread then died of the poisoned lock). The guest bug is fixed
  (HELLO first), but two host-side holes remain: (a) that `unwrap` — a failed proxy socket must
  refuse the one connection (RST it), never abort the VMM; (b) the control plane accepts and
  drops first-message violators at whatever rate the guest offers — consider a per-peer accept
  backoff or a cap on concurrent unauthenticated peers. Guest side, (c) the helper's
  `HostGone → try_connect` path sleeps only when `vsock_connect` itself fails, so any future
  drop-after-accept storms again; a backoff on a channel that died before its first reply
  closes that class.

## Guest app crashes (venus/KK correctness)

- **~~`vkGetPipelineCacheData` returns `VK_ERROR_OUT_OF_HOST_MEMORY`, and GTK4 aborts on it~~ —
  ROOT-CAUSED + FIXED the same day (virglrenderer 0058).** It was never a KosmicKrisp or
  pipeline-cache bug. `vkr_context_wait_ring_seqno` tested `thrd_timeout` while the c11 shim
  returns `thrd_busy` for ETIMEDOUT, so its "STUCK >500ms" branch was dead and **any ring wait
  over the threshold marked the context FATAL** — a diagnostic manufacturing the wedge it was
  added to observe. The poisoned context then refused blob creates, venus could not grow its
  reply-shmem pool, and the next reply-bearing call returned `VK_ERROR_OUT_OF_HOST_MEMORY`; GTK4,
  which never checks its pipeline-cache size query, aborted in `g_malloc` on the uninitialised
  size. A/B at a 1 ms threshold: 1 FATAL before, 0 after (3 STUCK diagnostics instead, every wait
  completing). Write-up + probe: `spikes/venus-ring-fatal-timeout/`.

  **Still open, upstream:** GTK4 aborting the process when `vkGetPipelineCacheData` fails is its
  bug — a driver is allowed to fail that call. Worth reporting; low priority now the trigger is
  gone.

## M4 venus residue
- **~~Concurrent VM makes NEW venus instance creation fail in another guest~~ ROOT-CAUSED + FIXED
  2026-07-20 — it was never a GPU bug: the TEST HARNESS ssh'ed into the WRONG VM.** With a second
  VM holding host port 2222, the supervisor correctly auto-allocated the test VM's ssh forward
  elsewhere (2224), but `Guest::boot` stored `cfg.ssh_port.unwrap_or(2222)` — so every `ssh_exec`
  landed in the BYSTANDER's guest, where identical test creds made all checks "pass" against the
  wrong guest. The `vkCreateInstance → -1` came from that bystander (the f44-kbuild guest: STOCK
  4 KiB kernel + STOCK mesa, whose venus hits the known MAP_BLOB offset-alignment gap — degraded
  exactly as the two-tier floor intends). In-guest forensics that unmasked it: `free -m` showed
  12 GiB in a "4 GiB" VM, battery-driver dmesg in a `--no-battery` VM, uptime 514 s in a 75 s-old
  test. Fix: the harness now pre-allocates an ephemeral port and ALWAYS passes `--ssh-port`
  (limina-test lib.rs); `two_vms_run_in_parallel_on_distinct_ssh_ports` upgraded to ride the
  auto-allocated path and prove per-handle guest IDENTITY (markers), which banner checks cannot.
  Multi-VM GPU coexistence verified working (two windowed KK VMs, venus live in both). LESSON:
  the assume-2222 rule ("READ the port from the log") applies to the harness too, not just
  interactive ssh — and wrong-VM crosstalk is invisible when guests share creds.
- **GLX / Xwayland apps present black on venus** (open, low priority — diagnosed 2026-06-29) — on the
  F44 enhanced tier, `glmark2` (no args → GLX) and `glxgears` show a **black window**; native-Wayland
  GL (Firefox WebGL) is fine. **Root-caused to the PRESENT path, not render/context** (verified on the
  live enhanced VM): `glxinfo -B` shows the GLX context creates and is accelerated — `direct rendering:
  Yes`, renderer `zink Vulkan 1.3(Virtio-GPU Venus … MESA_KOSMICKRISP)`, GL 3.1 (the KK
  custom_border_color cap, [[limina-kk-feature-gaps]]) — and `glxgears` renders **395 FPS** while the
  window stays black. So GL renders fine; the X11 present (zink's **kopper** DRI3/Present WSI on the
  Xwayland-backed-by-venus surface) never gets the rendered pixmaps to the window. This is the known
  "kopper X11 regression" the `venus_replay` X11 probe trips over (`venus_replay.rs:211`). User doesn't
  use GLX apps → deferred. Investigation start: the kopper DRI3 pixmap sharing / X Present on a
  venus-backed Xwayland (is the present-buffer a virtio-gpu blob that isn't flushed/attached to the
  Xwayland Wayland surface?); compare client-direct DRI3 present vs Xwayland glamor.
- **Stock/basic tier: guest Vulkan doesn't degrade to lavapipe** — **ROOT-CAUSED + FIX AUTHORED &
  VALIDATED 2026-07-01** (was: open, reported 2026-06-29 dogfooding). The earlier guesses were wrong:
  lavapipe IS installed, and the loader DOES skip ICDs that fail at *enumerate*. The real mechanism:
  on a 4 KiB-page guest with the coexist GPU, venus's **instance ring** shmem blob (132 KiB — a 4k
  multiple, not 16k) can't be `hv_vm_map`ed (`size%16k=4096`, patch 0011's alignment log), guest mmap
  fails, and **venus returns `VK_ERROR_OUT_OF_HOST_MEMORY` from `vkCreateInstance` — which the loader
  treats as fatal for the WHOLE instance** (unlike `INCOMPATIBLE_DRIVER`, which it skips), killing
  lavapipe with it (`vulkaninfo: vkCreateInstance failed with ERROR_OUT_OF_HOST_MEMORY`). Fix =
  `patches/mesa/0012` (venus: degrade to the existing STUB instance — 0 devices — when post-connect
  ring/version setup fails, mirroring the wire-version-mismatch path). Validated RED→GREEN on a stock
  F44 guest: patched venus + lavapipe → llvmpipe enumerates cleanly. Ships in `build-venus.sh` + the
  F44 mesa RPM (protects the enhanced image's **stock-kernel GRUB-fallback boot** — a 4k kernel + our
  mesa); **truly-stock guests are fixed only when 0012 lands upstream** (Wave-1 upstream candidate) and
  trickles into Fedora — until then the stock-tier Vulkan floor still fails on coexist boots (residual,
  accepted). Headless boots (no GPU device) were never affected — venus declines cleanly there.
  - **2026-07-03 update — decision: address long-term by UPSTREAMING 0012; no host-side mitigation.**
    The mapping failure itself is now two-thirds fixed: the SIZE half host-side (libkrun 0043 +
    virglrenderer 0023), the OFFSET half guest-side via the `limina-virtio-gpu` DKMS module
    (`guest/virtio-gpu-dkms/`; memory `limina-blob-map-16k-alignment`) — with the module installed
    **venus fully works on a stock 4 KiB guest**. A truly-stock guest (no module, Fedora mesa) still
    loses ALL Vulkan on coexist boots: post-0043 the ring's odd-size (0x21000) blob maps fine, but its
    node misaligns the NEXT window allocation's offset → same fatal OOM out of `vkCreateInstance`.
    Host-side mitigations were considered and rejected: an adaptive "venus quarantine" (drop the venus
    capset on the boot after seeing the alignment-failure signature) still leaves first-boot Vulkan
    dead and needs a re-enable policy; 16 KiB-rounding the vkr-reported memoryRequirements only helps
    well-behaved apps — any legal odd-size `vkAllocateMemory` re-poisons later offsets, turning
    "cleanly absent" into "randomly OOMs mid-run". Guest-side stopgap if anyone asks:
    `VK_LOADER_DRIVERS_DISABLE='*virtio*'`. Meanwhile `tests/venus_fallback.rs` (in test-boot.sh since
    2026-07-03) pins the truthful contract — explicit-lavapipe floor works, default path fails
    structuredly, session survives — and auto-tightens the day the default path starts succeeding.
- **Card widgets in the shell lose their contents on the accelerated GL path** (open, localised to
  zink/KosmicKrisp 2026-08-24) — a rounded-rect St card in gnome-shell keeps its background and
  loses text and icons inside it. Canonically: the card is pixel-identical to a healthy one except
  that the header row (app icon + name + timestamp) and the title are not painted, with background,
  large icon and body at identical positions and unchanged card height. Other shapes: shredded glyph
  outlines, and coloured specks at a text row's left origin.
  - **The fault is below vrend, in zink/KosmicKrisp.** Host-implementation locus split: host mesa
    built with `-Dgallium-drivers=zink,llvmpipe`, worker switched with `LIMINA_HOST_GALLIUM`.
    Identical guest, virgl protocol, vrend GL stream, driver script and scoring; only the host GL
    implementation differs. zink→KK→Metal **4 damaged / 4 clean**; llvmpipe **0 damaged / 16 clean**.
    Caveat: llvmpipe is much slower, so its clean half is strong evidence rather than proof
    (timing suppression); the damaged half is unambiguous.
  - **Not notification-specific, and no placement is safe.** In one frame of the open clock menu,
    plain labels on the popup background render perfectly while *every* rounded-rect card in the same
    popup loses its contents. A notification can render complete in the popover list while its banner
    form is damaged — but the popover is also a place the damage appears, so there is no calm copy.
  - **Excluded, each against a measured same-session baseline**: the cogl glyph atlas
    (`COGL_DEBUG=disable-atlas[,disable-shared-atlas]`); the per-StLabel offscreen FBOs as locus
    (`CLUTTER_PAINT=disable-offscreen-redirect` — text drawn direct-to-framebuffer still corrupts);
    the guest driver's opportunistic transfer optimizations (`VIRGL_DEBUG=xfer`); vrend-level upload
    ordering in **both** directions (`LIMINA_VREND_TRANSFER_FORCE_SYNC`, `..._SYNC_AFTER`); and the
    damage region / clip stack / buffer age / actor culling family
    (`CLUTTER_PAINT=disable-clipped-redraws`, 13/13 damaged). `--gpu-software-2d` cures it (0/40).
  - **It rides a repaint, not the first render** — *"it appears with text, then animates to grow into
    the full bubble completely empty."* Disabling GNOME animations helps without curing. The reading
    that fits: content is sampled too early, before its upload or its FBO render is visible.
  - **The label FBOs are painted and their contents are wrong** — not skipped. gnome-shell's
    screenshot renders via `clutter_stage_paint_to_buffer` with no view, so culling bails and no clip
    or buffer age applies; a merely-skipped label would have repainted correctly there. It comes out
    damaged.
  - **venus is not involved.** gnome-shell is a GL compositor and GL rides vrend on both tiers; this
    reproduces on a **stock** guest (kernel `6.19.10-300.fc44`, mesa `26.0.3-4.fc44`, no limina guest
    components). Matches dogfood experience — gnome-shell shows it, the synoik Vulkan compositor does
    not. The original 2026-06-29 attribution to venus was wrong.
  - Next: instrument zink/KK the way the venus vertex-buffer oracle was built — zink's barrier
    tracking between a staging `vkCmdCopyBufferToImage` and the sampling draw, and the
    render-to-texture → sample path for a label's FBO.
  - Rig, reproducer, evidence and the measurement traps (several produced confident wrong readings):
    `spikes/notification-text-corruption/RESULTS.md`. Earlier evidence of the same signature on the
    enhanced tier: `spikes/venus-draw-probe/notification-green-artifact-2026-06-29.png`.
  - Cosmetic but pervasive — every card widget in the shell is affected, not one notification.
- **venus TSD-destructor SIGSEGV on libtest worker-thread teardown** (open, isolated + reproduced
  2026-06-30 — fix belongs in venus/mesa) — a wgpu **Vulkan device on venus** (`libvulkan_virtio.so`),
  created and dropped inside a libtest `#[test]`, SIGSEGVs as the test's worker thread exits. The
  *identical* code as a plain binary (`cargo run`) is clean, and the same test under **lavapipe** is
  clean — so it is neither an app bug nor a general venus bug; it fires only when a venus-touching
  **non-main thread exits while the process keeps running** (exactly what a test runner does). Root
  cause (core backtrace via `coredumpctl`): venus registers a thread-specific-storage destructor
  `tss_create(&vn_tls_key, vn_tls_free)` (`vn_common.c`); the libtest worker `pthread_exit`s and glibc
  `__nptl_deallocate_tsd` calls `vn_tls_free` — but `vkDestroyInstance`/`vkDestroyDevice` already tore
  down the per-thread/instance state (and the ICD code may be unmapped) when wgpu dropped the
  device/instance, so the destructor jumps into freed/unmapped memory (frame `#0` = `n/a + 0x0`, no
  loaded module). A plain binary does the work on the **main** thread (no mid-process `pthread_exit`),
  so it is clean; lavapipe registers no such fatal destructor. **Fix layer = venus mesa**
  (`src/virtio/vulkan/vn_common.c`, the `vn_tls_key`/`vn_tls_free` lifecycle): make `vn_tls_free` safe
  to run after the instance/device it relates to is destroyed (NULL the TSD value on teardown, or guard
  against freed state) so a late thread-exit destructor is a no-op rather than a fault; a secondary
  suspect is the Vulkan loader's ICD-unload ordering (whether the ICD can be unmapped while threads
  with pending TSD destructors are alive), but the faulting destructor is venus's. **Minimal repro +
  full analysis: `spikes/venus-teardown-repro/`** (its own cargo workspace, stock crates.io wgpu 29 /
  winit 0.30 pinned to match ghost-ui, no ghost code) — `t0_headless_device_only` is the canonical case
  (no window/surface/present); `t1`/`t2` add a window + present only to show those don't matter; run
  one test at a time (a SIGSEGV takes the whole process down), then `coredumpctl info teardown` for the
  elfutils backtrace. ghost-ui's `frontends/ghost-ui/harness/tests/windowed.rs` currently works around
  it with `std::process::exit(0)` after its assertions (the real-frames-presented goal is verified
  before teardown). Low urgency (test-harness-only; the workaround holds) but it is a real venus
  lifecycle bug worth upstreaming.
- **virtio-gpu flip-completion gap** — ✅ **RESOLVED (verified 2026-06-23); item was stale.** Already
  fixed by `patches/linux/0001` (drm/virtio fence blob-scanout flushes, 2026-06-11): host3d_blob
  (venus) scanout FBs now carry the same fence the dumb path has, so `virtio_gpu_resource_flush`
  `dma_fence_wait`s (50 ms cap) before commit-tail, which gates `drm_atomic_helper_fake_vblank` →
  the (fake) page-flip-complete event fires. Verified on the enhanced tier with `kmscube -A`
  (atomic + fencing): two clean runs rendered 299 and 359 frames at a steady **30 fps**, rc=0, no
  dmesg errors — event-driven atomic clients render, they do not hang. Legacy `drmModePageFlip`
  events also work. GOTCHA that masked this: kmscube polls **stdin** alongside the DRM fd, so over a
  non-interactive SSH session it sees EOF→POLLIN, prints "user interrupted!", and bails after ~1
  frame — run it as `sleep N | kmscube …` to give it a quiet stdin.
- **Direct-KMS double-buffered clients cap at 30 fps** (investigated 2026-06-23) — understood, narrow,
  NOT fixing now. kmscube `-A` runs 31 fps regardless of `LIMINA_FENCE_LATCH_MS` (8 vs 35 ms both
  31 fps), so it is *not* the open-loop latch fallback — the present fences complete via the truthful
  CA-latch ack. Host is 60 Hz, so 31 fps ≈ 2 vsyncs/frame: a strictly double-buffered client that
  blocks on flip-complete misses every other vsync because the #8 fence-accurate present does two
  sequential waits (GPU-render-complete, then CA-latch) and that round-trip exceeds one vsync. The
  Wayland desktop + Wayland fullscreen apps hit 60 fps (mutter triple-buffers and pipelines the next
  frame while the current one latches). So only strictly-double-buffered, blocking, *direct-KMS*
  clients (kmscube, bare SDL-KMS demos) are affected — not the real workload. Fix directions if ever
  pursued: (1) decouple the atomic-KMS fake-vblank from the full CA-latch (fire at render-complete /
  on a vsync-cadence timer — mirrors real hardware, but a #8 design change that must not reintroduce
  tearing), or (2) shave the present round-trip below one vsync (needs worker instrumentation to
  quantify it first). Revisit only if direct-KMS double-buffered fullscreen clients become a target.
- **#28 coherency residue policy** — ✅ **CLOSED (2026-06-23): no action needed; keep venus feedback
  disabled.** Re-framed after a design panel over-stated it. `VN_PERF=no_*_feedback` turns off venus's
  host-visible *feedback* buffers (host writes fence/semaphore/event/query completion into a
  guest-pollable buffer so waits resolve locally with no guest→host round-trip); off, sync rides the
  virtio-gpu per-context ring fence our stack already retires. **We never want feedback on:** the
  round-trip elimination it buys only matters for fine-grained-sync-heavy GPU **compute/ML** (krunkit's
  domain), not a vsync-paced GNOME+WebGL desktop (a handful of syncs per 16 ms frame, blocked on the
  frame fence anyway — the saving is invisible under 60 Hz). And enabling feedback would *exercise* the
  #28 SLC-beyond-PoC host-visible-coherency fragility, i.e. trade robustness for perf we can't use.
  Feedback-off (ring fence) is already tier-2 GREEN — more robust **and** sufficient. So nothing to fix,
  productize, or spike. The earlier "productize or a fresh enhanced guest *hangs*" claim was an
  unverified inference; the two-tier floor is already safe via venus's graceful llvmpipe degrade
  (venus-init failure → software-2D, `VN_PERF` only read when venus actually renders = the enhanced
  tier, which is our own baked image). Two of the panel's "real fix" candidates stay dead on physics
  regardless (host-clean-to-PoC is a no-op — Shared `MTLBuffer` already host-coherent; HVF stage-2
  attrs are not expressible — `hv_memory_flags_t` is permission-only, `hvf/src/lib.rs:289`). **Revisit
  only if limina ever grows a venus-compute tier** (out of charter — it's a desktop VM).
- **Cosmetics** — ✅ **mostly DONE 2026-06-23 (libkrun 0029/0030).** Verified on the seated venus
  tier: the desktop now boots with **zero** `virtio_gpu` dmesg errors (was: a `capset_id=2` GL-probe
  EINVAL + `0x1200` responses for `CTX_ATTACH/DETACH_RESOURCE` 0x202/0x203), venus rendering
  unchanged (gnome-shell `init=0x4` contexts, tier-2 GREEN).
  - `num_capsets` hardcoded 5 → **fixed (0029):** the device hardcoded 5 while `create_rutabaga`
    passed `capset_mask=0` (registers all 9); now both derive from `virgl_flags` via one helper, so
    a `VENUS|NO_VIRGL` guest enumerates exactly the venus capset and never probes ones we can't serve.
  - `0x1200`/`0x202`/`0x203` `CTX_ATTACH/DETACH_RESOURCE`→ErrUnspec → **fixed (0030):** in coexist
    mode the 2D scanout resources (boot fb / fbcon) aren't in the 3D renderer's map, so the kernel's
    attach/detach of them to a 3D context is now an idempotent no-op (also covers the detach/teardown
    race). Real 3D resources (de)attach normally.
  - **Firefox MSAA silent non-AA** → **documented, NOT chasing (known cosmetic).** Core MSAA works on
    zink/venus (`spikes/venus-draw-probe/msaa-test.c` passes); the gap is Firefox-specific — its
    `MozFramebuffer::CreateImpl` combo (color RENDERBUFFER + DEPTH24_STENCIL8 @ samples=4) reports the
    backbuffer incomplete, so it silently falls back to non-AA. One app's AA quality, with the general
    path working — not worth a venus/zink/KK FBO-completeness rabbit-hole. Reopen only if MSAA breaks
    broadly. (`msaa-test.c` is the standing oracle; memory `limina-tier2-venus` thread 7.)
  - Remaining (untouched, genuinely low-value): KK GPU-side per-draw root re-fetch (only if GPU-bound
    workloads reappear). Roadmap M4 (~line 413).
- **Khronos VK-GL-CTS on the enhanced guest as an opt-in validation layer** (noted 2026-07-20, not
  started) — add a way to run the Khronos conformance suite
  (<https://github.com/KhronosGroup/VK-GL-CTS>: dEQP-VK for Vulkan, KHR-GL/dEQP-GLES for GL) inside
  the enhanced guest, exercising the full stack we own end-to-end: guest mesa (venus, zink) →
  virtio-gpu → virglrenderer (vkr) → KosmicKrisp → Metal. Rationale: our current oracles
  (pixel-verify probes, venus_replay, glmark) catch crashes and gross wrong-rendering; CTS is the
  conformance-grade net that catches subtle wrong results (format/precision/sync edge cases), and
  since we own every layer, each failure is actionable — same spirit as the KK feature-gap probing
  ([[limina-kk-feature-gaps]]). **Explicitly NOT in the default suite** (`test-boot.sh` stays as-is):
  a full dEQP-VK run is hours-long; this is an additional, on-demand layer. Sketch: build the CTS for
  aarch64-linux (in the limina-build container or the F44 build guest), stage it into the enhanced
  test image (or a virtiofs share), drive it over ssh via a `scripts/`/xtask runner with curated
  caselists — start with the `*-main` mustpass subsets and a smoke list sized for minutes, keep a
  known-failures baseline so runs diff against expectations rather than demand 100%. Useful
  precedent: virglrenderer/crosvm CI runs exactly this shape of guest-CTS-subset job.

## M5 hardening
- **Clipboard test-coverage gaps** — the ext-data-control (enhanced) backend is live-verified only,
  not under automation (`l1_session_helper` exercises only the RemoteDesktop fallback); plus
  stale-serial races, multi-peer broadcast + dead-peer pruning, helper reconnect after supervisor
  restart / D-Bus session death. Roadmap M5 (~line 522); memory `limina-m5`.
- **virtiofs DAX/shm window** — `VirtioShmRegion` in `fs/device.rs`; confirm shm-window alignment +
  FUSE_SETUPMAPPING/SHMCAP on 16 KiB host pages (enhanced tier already runs 16k guest = host-page;
  test stock-4k separately) + host↔guest uid mapping. Roadmap M5 (~line 502).

## M6 dynamic memory
- **`inelastic` conflates "converged" with "stranded"** (added 2026-08-13, from the io-keyed
  give-back A/B). The hold fires whenever inflating would dig into page cache, which covers two
  opposite states that the trace labels identically. Benign: on the 08-13 dogfood day, all 27
  runs longer than a minute (114 min total, the largest single hold) were entered with the
  balloon already near max — median `actual_bytes` 17.18 G of 24 G, median free 616 MB, PSI 0.
  That is the designed terminal state of a well-ballooned guest, and it is *correct*. Stranded:
  after the io give-back ladder empties the balloon (see below), the same hold fires at
  `actual_bytes` 1.12 G with the host billing 36.95 G, 24.91 G of it compressed — the balloon
  cannot refill because everything it would take is now cache. A hold at a *low* balloon level
  with a *high* footprint is a stranded state and should be distinguishable in the trace, not
  read as convergence. It is **self-limiting, not permanent**: a footprint that large drives the
  host into `warn`, which is precisely the trickle-dig condition (`balloon_policy.rs:53-57`), and
  the 08-13 run recovered 1.12 G → 5 G with footprint 37.8 → 30.4 G within ~5 minutes of
  entering it. So the cost is the transient and the ugly Activity Monitor reading during it, not
  a wedge — but note the loop only closes *because* the overshoot got bad enough to alarm the
  host, which is a poor trigger to rely on.
  NOTE the falsified hypothesis, so it is not retried: inelastic runs are NOT downstream of
  give-backs — 0 of 27 long runs on 08-13 had a give-back in the preceding 120 s, out of 103
  give-backs that day.
- **Cadence sweeps keep firing at near-zero yield on a settled idle guest** (observed overnight
  2026-08-14, low priority). On an idle dogfood guest the cadence sweep ran every ~30 min for
  hours, debiting **44 / 47 / 54 MiB** per run — against 1,893-3,042 MiB for the demand sweeps
  during the evening's activity. The sweep is cheap (13 ms for 1,752 MiB historically) so this is
  waste, not harm, and the `DemandHoldoff` yield-guard machinery already knows how to recognise a
  low-yield sweep — it just isn't applied to the *cadence* path. Cheapest fix is probably to let a
  cadence sweep that yields under `DEMAND_SWEEP_MIN_YIELD` push its own next-due time out, so a
  settled guest stops paying for a walk that reclaims nothing. Do not treat this as urgent: on a
  16 KiB-page host the walk is short, and the counters above are the whole evidence base.
- **The settled-free cooldown lift cannot fire during an io episode, by construction** (observed
  2026-08-14 01:04). The io-pain fix (3334ef1) resets the settle timer whenever `io_full_avg10`
  exceeds `IO_PRESSURE_LOW`, so an episode with sustained io pain keeps the timer at zero and the
  cooldown runs its full `RELEASE_COOLDOWN` (300 s). Measured: `cd_run` reached **226** consecutive
  cooldown decisions with >1 GiB free during the 23:15 episode. This is the tradeoff we chose
  deliberately — the alternative is the 21-give-backs-per-minute oscillation the fix removed — but
  it means the lift only helps *quiet* guests, which is worth knowing before anyone reads a long
  cooldown as a wedge. NOTE for anyone alerting on it: `cd_run` and `sweep_faults` in
  `watch-worker.sh` are HIGH-WATER MARKS over the whole trace file, not current values, so a
  threshold alert on the level fires forever once any single episode crosses it. Alert on growth.
- **The allowance path overshoots the same way the give-back did** (added 2026-08-13 evening, first
  night of the `GIVEBACK_FREE_CEILING` build). The guard worked: 6 give-backs, all at 552-699 MiB
  free, and at the two ticks where free crossed the ceiling (1358 / 1416 MiB) the decision was
  `cooldown`, **not** `giveback` — the ladder stopped at 14.50 G where the unguarded build had run
  to 1.12 G. But ~4 minutes later the balloon still reached **2.25 G with 15,993 MiB free**, this
  time through ordinary `set` decisions as the allowance target walked down after `some_avg60`
  peaked at 2.56%. So the give-back path is now bounded and the allowance path is not.
  Distinguishing it from the morning case: there WAS real memory pressure (2.56% vs 0.00%), and it
  recovered on its own (balloon back to 18.00 G, footprint 17.2 -> 9.3 G). Whether it is a defect
  at all turns on whether a guest that ends with 16 GiB free ever needed the release.
  **Leading candidate for the trigger: `localsearch-3`** (the GNOME file indexer), active through
  the whole window — an indexing pass fits both phases, read IO then memory pressure. Unconfirmed;
  catching it needs per-process memory at the time. It matters because it is an ordinary
  background job on a daily driver, not a benchmark. Trace: the dogfood `balloon-trace.jsonl`
  around 23:15-23:30 local.
- **The io-keyed give-back cannot tell disk IO from balloon thrash** (added 2026-08-13, measured).
  A cold `md5sum` of `/usr` on the dogfood guest, with **memory PSI at 0.00% throughout**, walked
  the balloon from 17.87 G to 1.12 G in 43 seconds — 1 GiB per report every 2 s — and the guest
  put the freed memory straight into page cache (buff/cache 20 G at the end). It never needed the
  memory; it was reading files. A day of ballooning undone by 100 seconds of ordinary IO. The
  obvious lever is that `some_avg10` sits in the same report and read 0.00% for every one of the
  18 give-backs, so requiring *some* memory pressure alongside io pain would have declined all of
  them — but check the original io-keying rationale first (balloon thrash plausibly shows as io
  pain *before* it shows as memory pressure, which is why it was chosen). Window data:
  `spikes/hv-ledger-gap/dogfood-2026-08-13/io-ab-window.jsonl`; scorer:
  `spikes/hv-ledger-gap/io-giveback-ab.py`.
- **Post-episode warm-read tax (~1.6×)** (added 2026-08-12, from the S3 escalating give-back
  grade): after a deep balloon dig + give-back, a *fully recovered* guest (cache re-warmed,
  kswapd idle, io-some <1%, zero stage-2 heals, no swap) reads its own page cache at ~16 GB/s
  vs ~26 GB/s pristine — 192 vs 118 ms/pass on the S3 vehicle. Cause unidentified; leading
  hypothesis is page-cache folio-order collapse (virtio-balloon inflates scattered 4 KiB pages
  → buddy fragmentation → the re-warm under duress rebuilds the cache as order-0 folios, and
  per-folio bookkeeping dominates at these speeds). Deflate pacing is exhausted as a lever —
  this is NOT a policy item. Discriminating probes: `/proc/buddyinfo` + folio-order stats in
  the bench recorder across an episode; and in the recovered state, `drop_caches` + a calm
  re-read (back to ~118 = build-conditions problem; still ~192 = persistent fragmentation).
  A real fix likely wants the custom balloon device idea (host-page-aware batched inflate
  keeping the buddy lists intact — memory `limina-balloon-bench`, FUTURE DIRECTION). Low
  priority: bites only after a real host-pressure episode, and 16 GB/s cached reads are
  still fast for desktop use; kernel compaction probably erodes it over time (unverified).

- **Settle-sweep fault handler: filter by `si_code`** (added 2026-08-13, from the sweep
  hardening pass): since fork 9390775 the handler fields ANY SIGBUS/SIGSEGV at a
  guest-region address, forever after the first sweep (deliberately not gated on
  `SWEEP_ACTIVE` — a last-window fault arriving after the flag cleared used to chain to
  SIG_DFL and permanently uninstall the `Once`-installed handler). The widened corner: a
  future *non-protection* fault at a guest address — e.g. `BUS_ADRALN` from a misaligned
  atomic in some device bug — would silently refault-loop instead of crashing honestly.
  Mitigation when it matters: field only protection faults (`SEGV_ACCERR`/`BUS_ACCERR`),
  chain the rest. Already observable in the field: such a loop spins `sweep_faults` (stats
  verb / decision trace) to millions.
- **Demand-sweep yield holdoff is too blunt** (added 2026-08-13, measured on the deployed
  build — full write-up in `docs/memos/2026-08-13-ledger-gap-field-findings.md`): a demand
  sweep debiting under `DEMAND_SWEEP_MIN_YIELD` arms a holdoff for a whole cadence period,
  on the reasoning that a low yield proves the gap was honest overhead. The *measurement*
  is right; its shelf life is not. Observed: a 449 MiB yield at 13:46 (taken mid-benchmark,
  when the resident residue genuinely was small) suppressed demand sweeps while the
  benchmark ended and the gap grew to 6.7–11.1 G; the first sweep after the holdoff expired
  debited **6.05 G**. A low yield means "the gap is honest *right now*", not "for the next
  30 min". Fix: cancel the holdoff once the gap grows materially past its level at the
  low-yield measurement, or shorten it to a few minutes.
- **Demand/idle trigger events are invisible in the managed app's log** (added 2026-08-13):
  both log at INFO and the bundled app runs at WARN, so `supervisor.log` shows neither.
  Field attribution had to infer demand pacing from sweep-counter deltas in the decision
  trace. Give them WARN lines or trace markers shaped like the scrub's `"scrub":"start"`.
- **Settle-sweep cadence needs a guest pressure report** (added 2026-08-13, accepted under
  the two-tier rule): the sweep fires from `on_pressure`, so a stock guest without
  limina-agent never sweeps and keeps the ~2× Activity Monitor inflation (degraded, not
  broken). Possible later item: a worker-side fallback timer so even agent-less guests
  settle occasionally.

## M3 networking
- **gvproxy reconnect-on-HANG_UP** — today the net worker logs FATAL and permanently disables the NIC
  on HANG_UP (`worker.rs:146`); the supervisor recreates the path. A small libkrun reconnect patch
  would survive a gvproxy restart without a full VM restart. Roadmap M3 (~line 321); memory
  `limina-m3-networking`.

## M2 / M8 polish wins (cheap, host-side)
- **`TapCtx::grab_mode` — assemble the tier once per event, in the adapter.** Today each site that
  needs [`GrabMode`] assembles it from the `captured` atomic plus the event's `soft` predicate plus
  `GrabState`. `captured` has one owner and `GrabState` is reachable, but `soft` is a predicate over
  window facts sampled per event (`window_facts`, a window-server round trip), so it has no owner to
  ask and **must not be cached** — a stale `space_visible` is exactly the bug that once left the
  keyboard pointed at a guest that had left the screen. The shape that works: the tap samples facts
  once, `TapCtx` assembles the tier from them, and the *answer* travels instead of the ingredients.
  `grab_policy` stays pure and parameterized — that is what makes its assertions possible without
  booting a VM, and it is not up for negotiation.
- **fn/aux-key buckets: settings UI + the Accessibility cliff** (added 2026-07-31, with the
  bucket policy in `crates/limina-input/src/auxkey.rs`). Two follow-ups the design review raised.
  **(a)** The buckets (`Media`/`Volume` hard-grab-only, `Brightness`/`Other` host-only)
  are meant to become per-key runtime settings; shape that config as `nx_key -> Option<GrabMode>`
  with buckets as defaults, not per-bucket overrides, or the first split *within* a bucket forces
  a refactor. **(b) The settings UI must render these toggles disabled with a "requires
  Accessibility" note when the tap isn't installed** (`TAP_PORT` null). Aux keys are delivered
  *only* to a CGEventTap — never to a local NSEvent monitor — so without the grant the whole
  feature is inert while ordinary keys keep working: a *partially* working keyboard, which reads
  as a limina bug rather than a permission problem. There is also no way to detect the press
  without the tap, so no just-in-time prompt is possible; the UI is the only place to say it.
  **(c) ANSWERED 2026-07-31 — fn+F3–F6 are not aux keys at all.** Mission Control, Spotlight,
  Dictation and Do Not Disturb arrive as **ordinary keyDowns with keycodes 0xA0/0xB1/0xB0/0xB2**
  (Globe = 0xB3), not as NX_SYSDEFINED — a third mechanism, unrelated to the buckets. So
  promoting one (Mission Control → GNOME overview) is a **`keymap.rs` entry**, not an `auxkey`
  bucket edit; `macos_special_action_keycodes_have_no_guest_mapping` fails the moment someone
  maps one, which is the point to decide the routing deliberately. These keys are **inert under a
  grab by design**: the tap drops any keycode with no guest mapping rather than handing it to
  macOS, because forwarding a key blind can fire a destructive host action (reboot/sleep/eject on
  keyboards that have one) that a grabbed user can't cancel — "classify and route on purpose, or
  drop; never forward blind". Ctrl-Opt reaches them meanwhile. Evidence + the rejected
  pass-through alternative: `spikes/fn-key-probe/RESULTS.md`, `output-f3f6.txt`.
- ~~**Pointer warps / pointer capture**~~ — **DONE** (2026-06-27). `Cmd-Ctrl-G` capture mode feeds
  the guest a separate relative-mouse virtio-input device; closes the guest-warp gap. Host cursor
  pinned by warp-to-centre (CGAssociate-false alone insufficient on macOS 26).
- **Pointer grab: review / redesign / validation** (reopened 2026-07-11, user-requested). Three
  strands. **(a) On the dogfood deployment (dogfood-mac) the Accessibility permission seemingly still
  isn't sticking, so the grab's CGEventTap can't capture Cmd-Tab** (user, 2026-07-11). This is the
  suspected tail of the ad-hoc-signing TCC churn: the fix (Apple Development identity + team-pinned
  designated requirement in `build-app.sh`, plus tap retry / AX re-prompt on Cmd-Ctrl-G, `301a702`)
  shipped 2026-07-03 but was **never validated on dogfood-mac** — the planned one-time TCC re-add there is
  still outstanding. Validate: confirm the deployed .app is identity-signed (`codesign -dr -`
  shows the team-pinned DR, not ad-hoc), have the user delete + re-add the Accessibility entry
  once, then check the grant survives a redeploy. If it *still* drops with a stable DR, that's a
  new bug worth its own root-cause pass. Diagnostics on dogfood-mac read-only; the re-add/redeploy steps
  are the user's.
  **(b) Grabbed cursor feel** — even where the grab works, captured-mode movement should be
  indistinguishable from non-grabbed. **(c) — BUILT and dogfooded:** the confinement grab sketched
  below shipped as the fullscreen pointer grab (`docs/design/fullscreen-pointer-grab.md`; five
  dogfood rounds recorded in `1021acb`). **(c) Redesign to explore: a *confinement* grab** — keep the
  exact non-grabbed pointer path (absolute coordinates through the fit rect, host cursor wearing
  the guest shape, identical movement/acceleration) and have the grab only (1) clamp the cursor to
  the window's fitted content and (2) capture system combos (Cmd-Tab etc., extending the
  CGEventTap/`match_host_shortcut` framework). That drops the relative-mouse device + warp-to-centre
  scheme from the ordinary grab entirely — sidestepping both (a) and (b) and likely the RD fight —
  while the relative device stays for guest pointer-lock (games), which genuinely needs deltas.
  Prior verified findings in the next entry still apply.
- **Pointer-capture containment — prior findings** (parked 2026-06-27). The current scheme re-pins the
  (hidden) host cursor to display-centre on *every* captured motion event. Verified facts: macOS 26
  `CGAssociateMouseAndMouseCursorPosition(false)` does NOT freeze the cursor (its `CGEventGetLocation`
  tracks the mouse 1:1), the session `CGEventTap` never disables under load, and with the per-event
  warp removed the relative deltas fed to the guest are perfectly clean. **Works well locally.** The
  warp's weakness shows only when *another* agent also drives the macOS cursor — a **remote-desktop
  client** used to operate this Mac — where the constant re-pin fights the RD cursor and reads as
  jitter/snapping (an edge-only-warp variant was tried and reverted; it didn't clearly help under RD).
  **Plan when we return:** research how VNC/RDP **servers** solve the same capture problem (they have
  this exact problem space and are far more numerous) before redesigning. Candidate angles: a real
  cursor-freeze API instead of warp, `CGDisplayHideCursor`+associate semantics, an `IOHIDEventSystem`
  relative-tap, or detecting/co-existing with an upstream RD capture. (2026-07-23: the guest-side
  half of the old scheme — the enhanced-tier flat pointer profile + `LIMINA_CAPTURE_SENS` — was
  retired: captured motion now integrates the macOS-accelerated deltas into a virtual cursor
  driving the absolute tablet, so no guest profile tweak is needed. The per-event centre re-pin,
  and thus this RD-confound item, is unchanged.)
- ~~**Non-grabbed guest cursor renders too large in a non-fullscreen window**~~ (user-reported
  2026-07-11) — **DONE.** The shape cache is now keyed on **(IOSurface id, scale_key)** with
  `cursor_scale_key(fit_w, guest_w)` and the fit-rect scale applied to the `NSImage` size and
  hotspot (`crates/limina/src/window/cursor.rs:111,132,140,215`; tests at `:284`), so the sprite
  tracks the window's fit scale instead of rendering at 1 px = 1 pt.
- ~~**Fullscreen**~~ (`Cmd-Ctrl-F`) and ~~**keymap remap / Command-Option swap**~~ (`--swap-cmd-opt`)
  — **DONE** (2026-06-27). The formerly-remaining M8 polish is done too: ~~**system-combo
  capture**~~ (the capture/soft-grab CGEventTap) and ~~**multi-display**~~ (display modes +
  per-host-display identity; `docs/design/display-modes.md`, `docs/design/display-cutouts.md`).
  Roadmap M8.

## Dogfooding / Parallels migration
Surfaced 2026-06-29 while planning the migration of a stock Fedora 44 Parallels VM onto limina on a
second Apple-Silicon Mac (full runbook: `docs/dogfooding-parallels-migration.md`).
- **Movable VM library + per-VM placement** — PLANNED 2026-08-12, design in
  `docs/design/vm-definitions.md` §8. Motivation: dogfooding on a small-disk mac mini with a big
  external APFS volume (interim: symlink `~/Library/Application Support/Limina/VMs` → the external
  disk, which `list()` already follows). Plan, in shipping order: (1) persist a library path in
  `config.toml` (precedence env > config > default; re-read per call) **plus the unmounted-volume
  creation guard** — today an unplugged external library makes `create_dir_all` silently grow a
  shadow library on the boot volume; (2) a "Change VM Library Location…" picker in the control
  center (v1 repoints, never migrates data); (3) per-VM placement via symlink-as-registration
  ("Add existing VM…" / create-at), with dangling links rendered greyed-out "volume not mounted"
  instead of silently vanishing.
- **`gvproxy` not bundled** — ✅ **DONE 2026-06-29** (`458664b`). `limina --net` resolved gvproxy only
  from `$LIMINA_GVPROXY_BIN`/Homebrew/`PATH`, so networking was dead on a Mac without Homebrew. Now
  vendored into the app (`Contents/MacOS/gvproxy`, copied + ad-hoc signed by `build-app.sh`) and
  resolved bundle-relative; priority `override > bundled > Homebrew > PATH` is a pure, unit-tested
  policy (`gateway.rs::resolve_gvproxy_bin`, RED-first test).
- **Agent install was a separate SSH flow** — ✅ **DONE 2026-06-29** (`458664b`). `install-enhanced.sh`
  now also installs `limina-agent` (+ unit + flat-pointer gschema override) when staged into the
  payload, and runs `restorecon` so it starts on a stock SELinux-**Enforcing** guest (the dev guest
  dodged this with `selinux=0`). The whole enhanced upgrade now rides the one offline virtiofs channel
  instead of also needing network + gvproxy. (`install-guest-agent.sh` remains the dev-loop SSH path.)
- **Enhanced install could brick the guest (unsafe boot-default switch)** — ✅ **DONE 2026-06-29**.
  `install-enhanced.sh` made the *unproven* 16k kernel the permanent GRUB default (Fedora's kernel
  install also auto-promotes the newest kernel). When the 16k initramfs failed to mount root (here:
  `/boot` ran low on space → an incomplete/driverless initramfs → the dracut emergency shell), the
  guest was stranded — limina has no keyboard at GRUB/emergency (see next) to pick stock. **Real
  dogfooding brick.** Fix: the installer now (1) pre-checks `/boot` free space, (2) force-includes the
  virtio/input/FS drivers in the initramfs (so root mounts *and* the emergency shell has a keyboard),
  (3) verifies the initramfs actually contains the root driver before trusting it, and (4) keeps
  **stock** as the permanent default while booting 16k **once on trial** (`grub2-reboot` next_entry)
  with an on-success systemd unit that promotes 16k only after it reaches multi-user — so a failed 16k
  boot auto-returns to stock on a power-cycle, no keyboard required. Recovery for an already-bricked
  guest: revert to a pre-install disk clone. (Two-tier guarantee: stock must always stay reachable.)
- **16k kernel can't mount a v1-space-cache btrfs (second migrated-guest brick)** — ✅ **DONE
  2026-06-29**. A 2021-origin Parallels guest's btrfs root still used the legacy **v1 free-space
  cache**, which a **16 KiB-page** kernel refuses to mount (`BTRFS error: open_ctree failed: -22`;
  "v1 space cache is not supported for page size 16384 with sectorsize 4096") → `sysroot.mount`
  fails → the (keyboard-less) dracut emergency shell. The stock 4k kernel mounts it fine and a fresh
  accessible base already uses v2, so this only bites *migrated/old* installs. Diagnosed from a
  verbose one-shot serial capture; confirmed fixed on the guest by setting `space_cache=v2` on every
  btrfs fstab entry → a reboot builds the **free-space tree** (`compat_ro 0x3` =
  `FREE_SPACE_TREE|VALID`, permanent) → the 16k then mounts (no cmdline option needed). Fix in
  `install-enhanced.sh`: `ensure_btrfs_free_space_tree` detects any mounted btrfs still on v1 (no FST
  compat_ro bit), sets `space_cache=v2` on all btrfs fstab lines, builds the tree live
  (`remount,clear_cache,space_cache=v2`) and **verifies** it. The 16k one-shot is armed only once the
  tree exists; otherwise a `limina-arm-16k.service` arms it after a plain stock boot has built it (so
  the 16k never boots onto a still-v1 fs). **NEEDS end-to-end validation on a real v1-btrfs guest**
  (only the awk + `bash -n` are checked so far).
- ~~**No keyboard at GRUB / early boot / dracut emergency shell**~~ — **(a) FIXED & user-validated
  2026-06-30** (commit `3210a36`): VirtioKeyboardDxe vendored into the GOP firmware + ConIn wiring
  (`patches/edk2/`), plus the libkrun virtio-input Inactive-on-reset fix (patch 0037) so desktop
  input survives the firmware→kernel handoff. The keyboard works at the GRUB menu; the RELEASE GOP
  firmware is rebuilt. **(b) FIXED & user-validated 2026-08-23** (a stock Debian 13 LVM-on-LUKS
  guest typed its passphrase and unlocked): between
  `ExitBootServices` and the moment the guest binds `virtio_input`, the guest had no keyboard at
  all. `install-enhanced.sh:247` forces `virtio_input` into the *enhanced* (dracut) initramfs;
  **neither stock generator ships it** — source-verified 2026-08-23: `initramfs-tools` never
  copies `kernel/drivers/virtio/`, and `dracut-ng`'s generic branch installs
  `virtio/virtio_ring/virtio_pci/virtio_blk/virtio_scsi` and no more. So an encrypted-root Debian
  guest could not answer its own LUKS prompt and was **unusable, not degraded** (a
  compatibility-floor break, confirmed on a real install 2026-08-23), and the emergency shell on
  any never-enhanced guest — Fedora included — was the same gap. The fix is not per-distro
  initramfs surgery but a **USB HID keyboard gadget** on the already-default-on xHCI: USB HID is
  in every stock initramfs because a bare-metal LUKS prompt requires it. Keys route to the gadget
  while the virtio device is not activated and to virtio otherwise —
  `docs/design/usb-hid-keyboard.md`.
- **No Parallels-import tooling** (open) — converting an existing Parallels disk (merge snapshots →
  `qemu-img -f parallels` → raw, the `virtio_mmio` initramfs regen, `console=` GRUB args, Tools
  removal) is documented in the runbook but not scripted. A guided `import` helper would de-risk the
  footgun (`virtio_mmio`, not `virtio_pci`).
- **No guest-tools distribution path** (open, architectural) — the enhanced tier can't be built or
  installed from `limina.app` alone (no RPMs, no toolchain on a second Mac). Recommended: a versioned
  out-of-band payload + a `limina install-guest-tools` subcommand that stages it into a `--share`. The
  F44 in-guest build prep addresses the *build* half; host orchestration is still open.
- **No payload↔guest version manifest check** (open) — `install-enhanced.sh` doesn't verify the
  payload's Fedora release matches the booted guest; a mismatch (esp. mutter↔gnome-shell ABI) breaks
  the desktop silently. Add a manifest + `/etc/os-release` guard.
- **KK/Metal never tested cross-machine** (open/unknown) — the host Vulkan-on-Metal stack was only
  exercised on the M1 Max / macOS 26.5 dev Mac. `--gpu-software-2d` is the degraded fallback if venus
  init aborts on different silicon/macOS.
- ~~**F44 enhanced tier blocked** — GNOME 49→50 mutter/cogl scanout regression~~ — **FALSIFIED
  2026-06-29**: the feared regression (and the mutter-50 `kk_encoder.c:299` assert) did NOT
  reproduce; the F44 enhanced desktop was validated end-to-end (16k + venus + patched mutter 50.1,
  pixel-verified; see `docs/images.md` §Component versions and [[limina-enh-delivery]]).

## M14 USB / auth

- ~~**No `--no-fido` opt-out (product-parity gap, 2026-07-25).**~~ — **DONE 2026-07-25.**
  `--no-fido` / `[hardware] fido`, defaulting true like the rest. It gates the passkey **store**,
  so the opt-out is transport-wide (no uhid capability advertised *and* no USB gadget) rather than
  just dropping the gadget — a half-disabled credential surface is not a disabled one. Independent
  of `--no-usb` in both directions, because FIDO's uhid transport rides the agent and not the
  controller. See `docs/fido-authenticator.md` §"Turning it off".

## M9 snapshot hardening (from the 2026-07-18 transport-restore removal)

- **Every restore logs ~226 `ctx 0 "HOST" IOV data size exceeds resource capacity`.** Measured
  2026-08-28 on a stock F44 guest, identical count across repeated restores of the same snapshot
  pair, and present on restores that are otherwise completely healthy. They come from
  `vrend_renderer_transfer_internal` during the classic content re-upload (task #19 phase C),
  which re-uploads each backed resource level-0/full-box from its guest backing store — a
  transfer whose IOV is smaller than the resource it is filling is rejected and that resource
  keeps whatever the replay gave it. Nothing observably broken today, but this is silent
  partial content restore, and it is the obvious next suspect for any "restored desktop looks
  subtly wrong" report. Worth deciding whether the mismatch is a stride/level assumption in the
  re-upload or genuinely unbacked resources that should be skipped rather than attempted.

- **A Vulkan compositor's desktop comes back blank.** The venus content capture reads a
  memory object with `vkMapMemory` (`vkr_device_memory_content_copy`), which device-local
  allocations refuse — the read fails, the bytes are skipped, and the snapshot's summary line
  still reads like a success. On a GL compositor nothing shows, because its compositing
  textures are classic vrend resources covered by the v6 `classic_contents` dump. On synoik the
  two skipped allocations are its double-buffered scanout images (`content read failed for
  ctx 2 mem 66/204 (14745600 bytes)` = 2560×1440×4, twice) and the restored desktop is empty
  until unrelated damage forces a full repaint. Device-local memory has to be copied through a
  host-visible staging allocation per bound resource (`vkCmdCopyImageToBuffer` /
  `vkCmdCopyBuffer`) with the symmetric path on restore. Two neighbouring shapes are NOT
  explained by this and must not be folded into it: a client that comes back **wedged** and
  stays broken through a resize (firefox nightly on the dogfood Mac), and a restored session
  that paints **nothing at all** until a new client arrives. Dossier + frames:
  `spikes/synoik-restore/RESULTS.md`.

- **`limina suspend <disk>` leaves the supervisor running.** The worker snapshots and exits,
  but the supervisor process stays alive holding its gvproxy (and its SSH port) and a stale
  window; `SIGTERM` does not end it. The next `limina suspend` on that disk then refuses with
  "multiple limina supervisors match". Reproduced on all three cycles of the 2026-08-29 synoik
  poke session.

- **`gpu restore: … 0 scanout flips` on every restore.** Same measurements. Phase C is supposed
  to re-bind the classic scanouts "so the first presented frame shows restored pixels"; the
  count being zero every time means that leg does nothing, and the first frame instead comes
  from whatever the guest redraws. Unexamined — it may be correct (blob scanouts taking a
  different path) or it may be why a restored desktop needs a redraw before it looks right.

- ~~**Guest windows are moved and resized by a restore**~~ — **CLOSED 2026-08-28.** A fresh
  worker's virtio-gpu carried virtio's default EDID, which claims a 10" panel: the restored
  guest's driver never went away, re-probed at once, read 2560x1440 on a 10" screen as a 250%
  scale, and mutter constrained every window into the resulting 1024x576 logical screen. Our real
  EDID landed a second later and the scale returned, but nothing put the windows back. The GPU
  snapshot now carries the per-scanout display configuration and restore re-applies it before the
  guest resumes. The same default-EDID hazard on *cold* boot is a race (`CLAUDE.md`); on a restore
  it was not, which is why it bit every time and why no display-push ordering could have fixed it
  — the bogus EDID was already there before either push.

- ~~**A stock Debian 13 guest cannot suspend**~~ — **CLOSED 2026-08-23, both causes.** Two
  independent faults, and neither was ours. **(a)** The quiesce oracle counted a device no driver
  had ever taken as a holdout, so a guest without a driver for one of our devices could never be
  observed to quiesce — fixed, it keys on `DRIVER_OK` now. **(b)** Before
  `528d92bfc093 "virtio_input: Improve freeze handling"` (first released in **v6.17**),
  `virtinput_freeze()` never reset the device, so the three virtio-input devices stayed
  `DRIVER_OK` through an s2idle the rest of the guest completed. It was never backported to
  6.12.y — that file is untouched on `linux-6.12.y` since 2024-07-08, branch tip 6.12.105 — so an
  LTS guest does not grow into it.

  Measured 2026-08-23 on one VM before and after: Debian 13 (6.12.101) held out on
  `virtio_i2c` at `1` plus all three inputs at `0xf` and aborted at the 20 s deadline; upgraded to
  Debian 14 / forky (7.1.8) the same VM **quiesced in 252 ms** and snapshotted — under the *older*
  `status != 0` oracle, so the guest drove every device to `INIT` with no help from (a). Resume
  from that snapshot presented its first frame 1.6 s after the play click, and the key router
  followed the guest back to virtio-input on its own after the s2idle thaw.

  The standing consequence is a support floor, recorded in
  `docs/design/m9.2-quiesced-snapshot.md`: **suspend requires a guest kernel ≥ 6.17.** A guest
  below it keeps running — the bracket wakes it and aborts — it just cannot be suspended.

- **Host sleep across a panel change is clean, in both directions (measured 2026-08-22).** Two
  VMs (F44 enhanced and F44 stock), 4-scanout pool: sleep with two panels and wake with two;
  sleep with two and wake with one (the external switched off mid-sleep); sleep with one and wake
  with two (switched on mid-sleep). Guests logged `PM: suspend entry (s2idle)` → `PM: suspend
  exit` every time, scanouts were re-published within 0.35 s of the wake, and no window was
  closed, reopened or lost its Space. The panel that vanished had its connector disconnected and
  its display swallowed into the main window; the panel that arrived got a slot and a window. No
  worker swap is involved in any of it, which is why none of the restore faults below apply.

- **An in-place resume came back on slot 0 while the host kept the panel→slot assignment it
  suspended with — the window froze on its last frame and "Resuming…" never came down. FIXED
  2026-08-22.**
  Measured 2026-08-22 on two flat `--disk` VMs (F44 enhanced and F44 stock, 4-scanout pool,
  both reproducing identically). The trigger is the primary window showing a **non-zero** slot at
  suspend time, which is what any panel change before the suspend produces: here the external
  panel was switched off during a host sleep, so the built-in swallowed guest display 1
  (`window: guest display 1 is the main window's now`) and the panel owned slot 1. On the play
  click the fresh worker re-enumerated from zero — `virtio-gpu displays: 4 scanout(s), slot 0
  connected at 3024x1896`, and the guest agreed (`Virtual-1 connected, Virtual-2 disconnected`,
  the inverse of the pre-suspend state) — while the host went on showing and watching slot 1.
  Three consequences, one fault: the window sits on its pre-suspend frame; the "Resuming…"
  overlay never comes down, because it is gated on `frames > resume_frames_baseline` for the
  **primary slot** (`window/mod.rs:2944`) and frames are landing on slot 0; and the capture aims
  at a slot the guest does not have (`we sent the pointer to slot 1 … the guest shows its cursor
  on [(0, …)] and none on slot 1`). Not frame starvation — a terminal launched in the guest over
  ssh drew, and the overlay still stood. **Recoverable, not terminal**: plugging the external back
  in forced a re-plan and both VMs came good (`resume: first frame presented 416.1s after the play
  click`). `windows.rs:796` already carries the right instinct for the worker-gone case — "the
  fresh worker re-announces whatever connectors the guest brings back" — and the restore path
  needs the same: reconcile the table against what the restored guest actually lights, rather than
  assuming continuity across the snapshot. The suite misses it because its suspend tests are
  single-display with the primary on slot 0 throughout, so the RED test is at the table.

  Two independent faults, fixed separately. **The arrangement is re-asserted onto a fresh
  worker**: the table's connector beliefs describe a device, and a swap replaces the device
  (`DisplayTable::reset_connectors_to_boot`), along with every identity, mode and position the
  host remembers having sent — a worker just spawned has been told none of them. Held until the
  new worker presents, because its display-control socket does not exist until the snapshot is
  loaded and a batch sent before that is dropped while the table records it as said. **And the
  overlay comes down on the epoch, not the frame count** (`resume_first_frame`): the dismissal
  compared against the counter at the play click on the stated premise that it survives the swap,
  which stopped being true when the swap began clearing the slots — so the fresh worker had to
  out-present the whole suspended session first, which on an idle desktop is never.

  Rig-confirmed 2026-08-22 on both shapes, reading the trace rather than only the window: two
  panels lit, suspend and resume → `re-asserting the arrangement`, `guest display 1 appeared`,
  first frame **5.5 s** after the click; and the original trigger — external switched off so the
  built-in swallowed slot 1 — → the same re-assert, slot 1's ring back, first frame **6.9 s**.
  Before the fix the same click took 416 s, and only because a panel was plugged in to force a
  re-plan.
  - **A third fault, found only by testing the panel change ACROSS the park.** The re-assert
    re-emits the slots the table believes are down, and a fresh device boots slot 0 *up* —
    carrying virtio's own default EDID, sized to whatever the worker was spawned with. Nothing
    else describes that slot when it belongs to a panel which is not the window's, because the
    primary's size-and-identity push follows the window. The guest read its built-in as virtio's
    10" default, chose a 250% scale for it (1024x576 logical from a 2560x1440 mode) and rendered
    a mode the panel cannot fill — a large letterbox. `connected` could not express it: it
    answers "does the guest see a plug", not "was it told what is on the other end", so the slot
    carries an `announced` bit now and an unannounced slot is owed its connect. The debt is owed
    to a *driver*, not to firmware — EDK2 paints head 0 and reads nothing else — so in the
    firmware phase the diff is unchanged. Rig-confirmed: slot 0 now arrives with the panel's
    refresh, DPI, VRR range and alt mode, and the guest reports 1512x948.
  - The stale absolute-pointer fit did NOT recur across either run (one 8.1 px echo-lag warning,
    no edge sticking), so it reads as a consequence of the assignment fault rather than its own.
    Not proven — two runs, and the fit is re-learned from the guest's echo either way.
  - **The host's display plan is not re-asserted onto the restored guest either.** After the
    recovery each guest had exactly ONE connector (`Virtual-1` at 2560x1440) and the second
    output's window was gone, while the host's display menu still showed that output enabled.
    The second panel is never re-lit — the host's idea of which displays are on survived the
    restore, but nothing pushes it to the fresh guest.
  - **The absolute-pointer fit is stale across the slot change.** On the enhanced VM the guest's
    cursor pinned to `x = 0` while the host sent x up to 1451 (782 px off), then wobbled as the
    fit re-learned (offsets 29 → 64 → 214 → 782 px within five seconds). The guest reported its
    monitor as 2048x1152 for a 2560x1440 mode (a 1.25 scale), so the fit and the mode changed
    together. Likely a consequence of the assignment fault rather than a separate one; re-check
    after fixing the reconciliation.

- **NOTE 2026-08-15: a VM suspended before `1e6895b` cannot be resumed after it.** Every worker
  spawn now carries an extra virtio-serial port (`com.redhat.spice.0`, the clipboard's stock-tier
  transport), so the device topology a pre-change snapshot was taken against no longer matches the
  one it would restore into. The suite is self-consistent — its snapshots are taken and restored
  on the same build — so the restore tests cover "restore works *with* the port", not this
  cross-version case. Practical rule when upgrading a machine past that commit (the dogfood Mac
  included): **resume and shut down any parked VMs first.** Generalizes beyond this commit: any
  change to the spawn-time device list has the same one-way cost, which is an argument for
  eventually versioning the topology in the snapshot header and refusing a mismatch loudly
  instead of restoring into a surprise.

- **OPEN 2026-07-28: gen-2 restore loses buffer creates → `seated_gnome_session_survives_snapshot_restore`
  FAILS INTERMITTENTLY (pre-existing, NOT the virgl-0054/0055 journal batching).** Failed
  3 consecutive runs on 2026-07-28 (including once on the pre-0054 baseline dylib), then
  passed the same evening's full-suite run with 0055 — timing-dependent, not deterministic.
  Failure shape: the gen-1 restore's replay logs ~7 tolerated `replay: entry failed` drops (buffer ids,
  `failed to look up object N of type 9`, gnome-shell ctx), meaning the FIRST live-recorded
  journal is already missing those `vkCreateBuffer` entries; the journal re-baselined during
  gen-1 replay inherits the holes, and after the SECOND restore the parked `vkpipeline.py`
  client's queue dies (`vkQueueWaitIdle` → -13 at beat 29). Bisect evidence: reproduces
  identically on the pre-0054 dylib (0053 tip) — three runs, same shape. The suite was
  reported 40/40 at 12e7fd9 the same day, so either that run's pass was environmental or the
  gen-2 leg silently skipped; treat TODAY's failure as the ground truth. Repro:
  `LIMINA_TEST_KEEP_SCRATCH=1 scripts/test-boot.sh debug seated_gnome_session_survives_snapshot_restore`
  (keep-scratch preserves all three generations' supervisor logs). Attack: find why live
  recording misses consecutive buffer creates (orphan_adds? dropped_fatal? check
  `vkr_journal_get_stats` counters at first export) before suspecting the replay side.

- **FIXED 2026-07-20 (virglrenderer 0040): the vkmark-on-resume crash — journal create-arg
  closure.** Root cause was neither candidate shape: the journal pruned a destroyed object's
  create entry even when a retained CREATE referenced the id in its wire args (pipeline ←
  destroyed shader modules/layout, legal and universal). The dropped pipeline create left a
  guest-live pipeline missing at replay; the first parked ring command referencing it after
  `replay_end` (FATAL sticky again) killed the ring → guest-visible FATAL status → vkmark
  abort. Fixed by pinning every CREATE's decoded handle refs (generalizing the blob←memory pin);
  RED/GREEN via the new `vkpipeline.py` leg in `venus_session_preserved`. Full forensics:
  `spikes/m9-vkmark-resume-crash/RESULTS.md`; design:
  `docs/design/venus-snapshot-replay.md` §"vkmark-on-resume crash FIXED".
  Remaining follow-ups from the same incident, still open:
  - guest kernel `RESOURCE_UNREF → 0x1203` right at resume (guest unref of a resource the host
    lost) — benign-looking (kernel logs and continues), unexplained.
  - ctx 4 (gnome-shell) `vkr_dispatch_vkWaitRingSeqnoMESA:399` ring FATAL 55 s post-resume
    (wait for a seqno the restored ring never reached) — desktop survived; plausibly its own
    first use of an affected pipeline (would be cured by 0040) or a seqno-epoch gap; watch
    post-deploy.
  - ~~guest-side hardening candidate (upstreamable mesa): venus failing submits with
    `VK_ERROR_DEVICE_LOST` instead of `abort()` on ring loss.~~ **DONE 2026-07-20** =
    `patches/mesa/0016-venus-ring-loss-device-lost-not-abort.diff`, shipped in guest mesa
    `26.1.4-2.limina.fc44` (both F44 enhanced images refreshed). Validated by A/B: with a
    pre-0040 host (replay gap present) the `vkpipeline.py` client now prints
    `PIPE FAIL beat7-vkQueueWaitIdle -1` and exits cleanly — zero coredumps (pre-0016 the same
    scenario SIGABRTed); with the 0040 host the full gate stays green (239 s, both generations).
    Watchdog/renderer-hang aborts deliberately unchanged. F43 pickup at its next respin.

- **Worker-quiesce during `dump_ram` (torn-dump race).** A device worker writing guest RAM while the
  RAM dump runs can tear the dump (used.idx advanced, payload half-copied). Pausing vCPUs stops new
  kicks but not asynchronous writers already in motion — loudest is **net RX from gvproxy** (delivers
  regardless of vCPU state) on the raw path; on the s2idle production path it narrows to the **GPU
  renderer** (the guest froze net/blk to INIT before we snapshot). Fix = *stop the writers, not the
  rings*: park the separate-thread writers (GPU renderer / blk) for the dump's duration. If
  `save_snapshot` runs on the main event-loop thread, EventManager-dispatched devices quiesce for free;
  verify the thread inventory vs source. Pre-dates M9.3; the removed drain accidentally masked it.
- **`Queue::len` unwraps the avail index (`queue.rs:443`).** Any spurious kick of a **not-ready** queue
  (e.g. the balloon free-page-reporting queue when `F_REPORTING` is masked, patch 0059) reaches
  `Balloon::process_frq → Queue::pop → Queue::len`, which `unwrap()`s `avail_idx` on an unconfigured
  ring → panic (exit 101). The M9.3 drain removal deleted the caller that tripped it, but the unwrap is
  a live balloon-hardening item (also on the upstreaming triage list) — `Queue::len`/`is_empty` should
  fail soft on a not-ready/invalid ring.

## Video — mesa binds a YUV→RGB matrix for an RGB→YUV post-processing pass (guest-side)

A VA post-processing conversion *into* a YUV format from an RGB source comes out with permuted
colours. Geometry is exact and the pass was fully black before the host-side VPP fixes, so this
is a residual, and it is in the guest.

The traced colour-space matrix is a textbook BT.709 **YUV→RGB** matrix -- rows
`(1.1644, 0, 1.7927, -0.9729)`, `(1.1644, -0.2132, -0.5329, 0.3015)`,
`(1.1644, 2.1124, 0, -1.1334)`. Applying it by hand to RGB inputs, clamped to `[0,1]`, reproduces
every measured output exactly: black → `Y=0 U=77 V=0`, white → `255/184/255`,
red → `48/255/7`, blue → `209/0/0`, and ffmpeg's `green` (#008000, not lime)
→ `0/49/0`. So the host delivers precisely what the guest bound; mesa picks the
wrong-direction matrix for the encode-side pass. **Do not re-diagnose this from pixels** -- the
arithmetic above already closes it, and the permutation is visually suggestive of a dozen other
faults.

Nothing we decode uses this direction (decode output is YUV and downloads either natively or via
YUV→YUV), so it costs nothing today; it would matter for a VA-API *encode* or capture path.
Fix belongs in mesa's `vl_compositor` and is upstreamable. See `docs/graphics.md` §4.5.

## GPU / venus ghost containment (from the 2026-08-13 totem crash)
- **Host-side fault injection for the tombstone path** — 📋 open. `vkr_ghost_containment.rs`
  asserts the product invariant (a refused import leaves the context alive), but on an image
  carrying mesa-guest 0007 the refusal is now *synchronous*, so the guest never mints a ghost
  and the vkr tombstone is never exercised: the test passes for the guest fix's reason alone.
  Proving the HOST half — the half that covers stock/older guests, which stay async forever —
  needs an env-gated hook in vkr that fails the Nth create for a named context
  (`LIMINA_VKR_FAIL_CREATE=<cmd>:<n>`, off by default, must not leak into normal suite runs).
  The one async path surviving the guest fix is `vkCreateImage` on a memory-requirements-cache
  HIT, so the probe shape is: create the same image key twice (miss = sync seeds the cache,
  hit = async), inject on the second, assert the context survives *and* the "tombstoned" line
  appears. Until then the host half is proven only by the 2026-08-13 manual A/B (old async
  guest + new worker → `CONTEXT ALIVE` + the SKIPPED marker in the worker log).
- **`vkr_ghost_containment` skips on `enhanced.test.raw`** — 📋 open, found 2026-08-13. The test
  needs `/dev/udmabuf`; `enhanced.raw` has it (its 7.1.6-limina16k kernel is `CONFIG_UDMABUF=y`,
  verified in-guest) but the test image comes up without it, even on the EFI vehicle (the
  guest's own kernel), so the test skips loudly instead of asserting. Suspect the test image's
  default BLS entry is an older enhanced kernel or stock — check `grubby --default-kernel` and
  `uname -r` in it first, then fold the fix into the next `enhanced.test.raw` refresh. Until
  then the seam has NO automated coverage in the suite; the 08-13 manual A/B is the evidence.
- **Zero-copy udmabuf import** — 📋 planned as **M15 wave 6** (see `docs/roadmap.md`); the
  refusal itself is no longer a correctness issue, only a copy per software-decoded frame.
  Key measured fact so the next reader doesn't re-derive it: a PRIME-imported foreign dmabuf
  never reaches the host at all (no `RESOURCE_CREATE_*`, no `RESOURCE_ATTACH_BACKING` —
  probe with `LIMINA_TRACE_ATTACH_BACKING=1`), so it needs guest-kernel work first.

## GPU — KosmicKrisp's command-allocator pool has no ceiling when the client never flushes

Measured 2026-08-26 on the host (zink-on-KK), with `spikes/notification-text-corruption/glyphmimic`.
Found in passing while chasing the notification-text bug; **it is not that bug** and does not move
its damage rate.

Our command-allocator pool in `kk_device.c` (`/Volumes/mesa-cs/mesa`, branch `limina-kk`) retires
surplus allocators **only in a call that was already served from the pool** — deliberately, so a
call that has to mint never also destroys. The consequence is that a client which submits render
passes without ever letting one complete keeps every allocator `in_use`, pass 1 never succeeds, and
the pool mints one allocator per render pass with nothing ever retired:

| workload (cards x 2 frames x 5 offscreens) | live class-0 allocators |
| --- | --- |
| 10 cards (100 passes) | 101 |
| 30 cards (300 passes) | 301 |
| 60 cards (600 passes) | 435 |
| 93 cards (930 passes) | 510 |

Tracking is 1:1 up to ~300, then sublinear as retirement finally engages — so it is **not a leak**,
it is an unbounded *in-flight* pool. Two things make it worth booking anyway: the **4 MiB budget
named in the warning does not cap anything** (it is reported, not enforced), and the count is driven
purely by submitted-but-uncompleted passes, which a guest client controls.

**What it is not.** Independent of the D24S8 depth attachment (`GM_NODEPTH` reaches the same 510)
and of the fresh-FBO-per-frame churn. It scales with render-pass count alone.

**What drains it.** Any per-frame completion or flush: `GM_FINISH=1` and `GM_PRESENT=1` both stay
**under the watermark entirely — no warning at all**. gnome-shell flushes every frame, which is why
this is invisible in normal desktop use; the exposure is a guest client that batches many frames
before flushing.

**Reproduce.** `spikes/notification-text-corruption/mimic-host.sh 93` and watch stderr for
`[LIMINA-ALLOC-POOL] class 0 grew to N`. Note the watermark warning is one-way
(`pool->watermark_warned` only ever increases), so the log shows growth but never the subsequent
drain — read the count as a high-water mark, not a live value.

## GPU / rendering perf
- **Should KosmicKrisp advertise `VK_EXT_vertex_input_dynamic_state`?** — 📋 open, raised 2026-08-26
  after the notification-text root cause. **Not a correctness item** — the bug it would have masked is
  fixed in zink (`spikes/notification-text-corruption/RESULTS.md`, §THE FIX), and adding the extension
  is not an alternative to that fix.
  - **The pitch is pipeline-permutation reduction.** Without it zink compiles vertex input into the
    pipeline, so every shader × vertex-layout combination is a separate PSO. Advertising it lets zink
    set the layout with `CmdSetVertexInputEXT` and collapse those permutations.
  - **Check this first, before any work:** Metal compiles the vertex descriptor into the pipeline state
    object (`MTLRenderPipelineDescriptor.vertexDescriptor`). If that still holds under **MTL4** — which
    is what this tree encodes with — KK can only implement the extension by caching PSO variants keyed
    on vertex input, i.e. doing zink's current job one layer down for no net gain. Verify against MTL4
    rather than assuming the classic Metal model; that answer decides whether the item is worth
    anything at all.

- **Should the enhanced tier stop forcing zink? (i.e. delete `/etc/environment.d/90-limina-zink.conf`)**
  — 📋 open, raised by the user 2026-08-01 now that vrend is well supported. Attractive for the right
  reasons: they are blunt globals hitting every process in the guest, and the baseline tier already
  runs vrend without them. **But it is not a "stop forcing" — it is a TIER SWITCH**, so measure before
  believing:
  - **Where it lands:** not llvmpipe. The guest ships both `virtio_gpu_dri.so` and `zink_dri.so`, and
    the host advertises both capsets in coexist (`GPU_COEXIST_FLAGS` in `crates/limina-vmm/src/krun/mod.rs`
    — `VENUS` plus the vrend EGL/GLES trio, `NO_VIRGL` deliberately off). Unset ⇒ GL runs on **vrend**.
  - **Our own honest numbers say venus still wins.** Post fence-honesty (`f0fe78a` + `98777bf`),
    crossmark has venus winning or tying **every** guest cell; the vrend small-frame advantage was
    fences retiring at decode (`glFinish` waited for nothing). That belief died on measurement — the
    reverse belief has to clear the same bar. See `limina-virgl-vrend-perf`.
  - **The real blocker is pacing, not throughput:** fence-accurate present for vrend is still OPEN
    (`docs/graphics.md` §9) — vrend's flush path never reaches `try_park_present`,
    so `FENCEPRESENT` never fires and the whole #24 tear/pacing arc (`c569129`, `c33d9a0`) does not
    apply. Moving the desktop to vrend today gives that up, and tearing is a human-eyeball verdict.
  - **`VK_DRIVER_FILES` is a SEPARATE knob and should stay regardless.** Unset, the loader enumerates
    `lvp_icd` beside `virtio_icd`, so a client that takes device 0 without checking silently lands on
    lavapipe. Our venus-specific guest mesa patches (0015 WSI present, 0016 ring-loss, 0017 submit
    free-list) also only pay off on venus.
  - **How to settle it:** A/B on a **clone** of an enhanced image (never in place), env file present vs
    removed. PIN display mode + scale first or the run is void (`limina-perf-display-pinning`); use
    `vkmark` as the control (Vulkan — it should not move at all), judge on the crossmark trio +
    aquarium via `scripts/perf-ledger.sh` (glmark2 swings ±10% between boots), and eyeball for tearing
    since no counter reports it. Revisit after vrend gets fence-accurate present — that is what would
    make removal a genuine simplification rather than a downgrade.
- **Stock-tier virgl (vrend GL) desktop slowness — ROOT-CAUSED 2026-07-28: a DEBUG-build present-path
  artifact, NOT a virgl regression.** The virgl present path is readback-per-frame (only venus blobs are
  IOSurface-backed; `flush_resource` falls back to `transfer_read` → staging → per-pixel RGBA→BGRA
  convert → canvas upload, `virtio_gpu.rs` / `limina-display::iosurface`). In a debug build that
  per-pixel convert (with debug asserts) costs ~60-100 ms per 2560×1440 frame → ~8-9 presents/s —
  *slower than software-2D* because sw-2D's guest framebuffer is already canvas-ordered (plain memcpy,
  cheap even unoptimized). Both sightings (2026-07-24 and 2026-07-28) were debug boots
  (`cargo xtask run` builds debug). **On a release worker the same guest animates at 60 fps** (median
  FLUSH2 gap 16.8 ms during overview animation; the apparent "repeating ~0.5/1 s stalls" were the drive
  loop's own idle gaps — user eyeball confirms "night and day").
  - **Fixed for dev boots:** `[profile.dev.package.limina-display] opt-level = 3` in the root
    `Cargo.toml`, so debug boots present at representative speed.
  - **UPDATE: zero-copy vrend scanout SHIPPED 2026-07-28** (virglrenderer 0053, plan B+A1) — the
    readback-per-frame description below is history for the scanout path. **Fence-accurate present for
    vrend is still open**, which is what the entry above turns on. Original text kept for the
    reasoning:
  - **Remaining, by design but worth fixing — zero-copy vrend scanout.** virgl presents pay
    readback (~2 ms) + convert (~4 ms release) + upload every frame, and the readback path bypasses
    the fence-accurate present (`FENCEPRESENT` never fires there). Residual jank on release (user,
    2026-07-28) is the expected symptom. The venus zero-copy chain (KK IOSurface-backed VkImage →
    `rutabaga.iosurface_id()` → `present_surface`) has a natural vrend analogue, all in layers we own:
    zink allocates `PIPE_BIND_SCANOUT` resources IOSurface-backed on KK → vrend exposes a
    resource→IOSurface query → rutabaga extends `iosurface_id()` to vrend resources → the worker's
    plain `set_scanout` resolves it exactly like `set_scanout_blob` already does. Spike first: prove
    zink-on-KK can export an IOSurface-backed scanout texture. This also puts virgl on the
    fence-accurate present path.
  - **Unverified leftover:** the 07-24 "~17 fps WebGL blob" was measured on a debug boot too — re-bench
    GL throughput on release before treating vrend shader/draw perf as a problem. The
    `>100 copy boxes` zink warning was a red herring for presents (it's upload-side, latches once per
    resource); revisit only if release GL throughput still disappoints.
  - **Benign, ruled out:** the `Mesa: error GL_INVALID_ENUM in glTexImage2D(...)` lines are one-time
    startup format probing; `vrend_decode_ctx_submit_cmd … "gst-plugin-scan" Illegal command buffer`
    (guest kernel `SUBMIT_3D → RESP_ERR_UNSPEC` at session start) is startup GL probing.
  - Probe recipe that cracked it: boot the stock clone with
    `RUST_LOG=info,limina_vmm=debug,krun_devices=trace` (worker logs now carry **µs timestamps**),
    drive the overview via `busctl --user set-property … OverviewActive b true|false` over ssh, take
    `sample <worker-pid>` during animation, and read FLUSH2 gap distributions. Memory:
    `limina-virgl-vrend-perf`.

## Clipboard (M5)
- **Whose clipboard wins when a guest has several sessions?** — 📋 open design question, raised
  2026-07-31. A guest routinely runs one `limina-agent-session` per graphical session (dogfood-guest had
  three: a GNOME session, a niri session, and the gdm greeter). All of them are clipboard-capable
  peers, so with per-peer serials (the fix for the offer-drop bug below) **any** session can push to
  the host pasteboard and the last write wins.
  - **This is not obviously wrong — it's a trade-off.** Copying in one session and pasting in another
    is a genuinely nice property, and sharing with the **greeter** is desirable too (user, 2026-07-31:
    "it's good to share clipboard with the greeter if possible"). So do *not* reflexively restrict
    peers by session class.
  - The alternative — bind guest→host to the seat's **active** session — is more predictable when a
    background session copies something the user never meant to send, but it costs the cross-session
    paste and needs new protocol: the agent would have to report its session's id/active state (the
    host cannot see `loginctl` from outside).
  - Weigh before building. Possible middle ground: keep last-write-wins, but have the host log which
    peer/session a copy came from so surprises are explainable.
  - **M12 now depends on this same seam** (2026-08-01). SPICE `vdagentd` serves only the logind-ACTIVE
    session, so "which session does vdagent cover" is the *same* active-session question wearing a
    different hat — and the per-session native-vs-SPICE arbitration (roadmap M12 task 4) has to answer
    it. Current lean: native always claims **inactive** sessions and SPICE serves only the active one,
    which keeps cross-session paste. Decide both together rather than twice.
  - Related, already fixed: the host used to ratchet ONE `guest_serial` across all peers while each
    agent numbers its offers from 1, so a long-lived session permanently silenced newer ones
    (dogfood-guest: the niri session's copies never arrived). Serials are now per-connection —
    `crates/limina-test/tests/l1_clipboard_multi_session.rs`.

---

## Suspected test flake — `l1_real_session_helper_bridges_clipboard_via_mock_mutter`

Seen **once**, 2026-08-04, during the full HVF suite run that validated virgl 0059/0060. Treated as
a flake by decision, not by analysis — recorded here so a second sighting is recognised as a repeat
rather than re-investigated from scratch.

```
mock log never contained "PASTED sess-host-to-guest-42"; current content:
CLAIMED_NAME / CREATE_SESSION / START / ENABLE_CLIPBOARD
```

`crates/limina-test/tests/l1_session_helper.rs:277`. The bridge got as far as enabling the
clipboard and then no paste arrived — consistent with a timing/wait bound rather than a logic
error, but that is a guess, not a diagnosis. Everything else in the run passed, including
`venus_desktop_pixel_verifies_through_host_capture`, and nothing connects vkr's external-memory
advertisement to a mock-mutter clipboard bridge. **If it fires again, stop treating it as noise:**
the first thing to check is whether the wait for `PASTED` is bounded generously enough under a
parallel nextest lane, since the suite has run parallel since 2026-08-03.

## Replay-under-load stall — `venus_replay` / `venus_shell_replay` never print `Rendered`

Seen **once**, 2026-08-12, during the full HVF suite over the escalating give-back commits
(102/103, everything else green). The guest-side `eglretrace --headless` shell-trace replay
never printed `Rendered`: ~14 minutes of 1 Hz `capture: configure scanout` plus a
once-a-minute `vsock muxer: unexpected dgram pkt: 3`, then the ssh command's own bound gave
up (955 s total). Venus was live in that same guest (the X11 GL probe enumerated
`zink … Virtio-GPU Venus`), the sibling `venus_vk_replay` passed right after, and an isolated
rerun passed in 65 s — flake by rerun, not by analysis. Note the failing run was ~40% slower
overall than the same-day 103/103 (3099 vs 2195 s): host load is the suspected ingredient.
**If it fires again, stop treating it as noise:** grab the worker log at the wedge timestamps
and check what eglretrace was waiting on (the per-minute dgram error is the one recurring
signal — identify pkt type 3's sender first).

**Second occurrence, 2026-08-13** (full suite over the demand-sweep commit, 103/104): same
signature to the second — the replay stalled 956.6 s and the isolated rerun passed in 63.6 s,
a 15× difference. Same host-load correlation (that suite run took 3094 s vs the same day's
2199 s green run). Two sightings, both under a loaded host, both clean solo ⇒ this is now a
*reproducible-under-load* condition rather than noise, and the next occurrence should be
debugged live rather than rerun: the cheap discriminator is whether the guest-side
`eglretrace` is starved of GPU progress (worker log at the stall timestamps) or of CPU/vCPU
time (the suite's own parallelism starving the VM's vCPU threads — the vCPU-envelope trap
from `limina-venus-replay-regression` is the obvious suspect, since the harness runs several
VMs at once on a 10-core host).

## Flaky test — `l1_silent_agent_is_reported_and_recovers` has zero timing margin

Failed in the 2026-08-14 suite (105/106) on its final assertion, that the *healthy* seed agent is
never reported silent:

```
11:24:46 WARN  agent limina-init/0.1.0 silent for 1.0s (no heartbeat)
11:24:47 INFO  agent limina-init/0.1.0 heartbeating again
```

**Reproduces solo, so it is not the load-induced family** (that was the first guess, and it was
wrong): **1 failure in 11 isolated runs**, ~9%. The test sets `LIMINA_AGENT_SILENT_SECS=1` while
the liveness sweep also runs at 1 s, so "silent for 1.0s" is a tie, not a lateness — the same
zero-margin phase race the test's own comment records losing once already, on the *other* agent.
The mechanism works (recovery logged one second later); only the never-falsely-reported invariant
trips. Fix by giving the threshold margin over the sweep interval rather than by re-running.

Independent of the balloon work: `GuestConfig::l1_from_env` sets `memory: None`, so no
`--memory MIN..MAX` reaches the supervisor, the PSI autoballoon policy is never started, and
`balloon_policy::decide` is never called in this test.

**Method note.** The first attempt to A/B this against the pre-change policy proved nothing:
`resolve_bin` runs a *pre-built* binary from `target/debug/`, and `cargo test -p limina-test`
does not rebuild it — so reverting the source file left all runs executing the identical binary.
Any A/B of shipped behaviour through this harness must rebuild **and re-codesign** between arms,
or compare something other than the binary.

**Third occurrence, 2026-08-27** (full suite over the 4 KiB-granule default, 117/118) — and it
hit the **other** trace this time: `venus_replay_matches_llvmpipe_reference` stalled 959.0 s with
the signature to the second (the venus arm's `eglretrace` context created, ~90 s of KosmicKrisp
shader work, then a wedge at the first frame boundary, the 1 Hz `capture: configure scanout` and
the once-a-minute `vsock muxer: unexpected dgram pkt: 3` for fifteen minutes, then the ssh bound).
Solo rerun passed in 163.9 s. Same load correlation a third time: 3343 s for the run, against the
~2200 s of a green one. So **the condition is not trace-specific** — it is the seated venus replay
under suite parallelism, and either trace can draw it. Not a granule effect: venus enumerated in
that same guest, the two sibling venus tests passed inside the same suite, and the log holds zero
`hv_vm_map failed` and zero `exceeds its` lines.

## Flaky test — libkrun's `sweep_fault_handler_fields_concurrent_touches`

`cargo test -p krun-hvf --lib` in `third_party/libkrun`. Measured 2026-08-27 on a clean tree
(no limina change in the run): **2 passes in 5**, failing with `no toucher write collided with a
sweep window in 50 sweeps`. The test needs a racing thread's write to land inside a sweep window
it does not control, so on a quiet or a busy machine it simply never collides. Pre-existing and
unrelated to anything we changed; it does not run in `cargo xtask test`, but it does fire for
anyone running the fork's own unit tests. Fix by driving the collision deterministically (hold
the sweep open, or count observed windows and skip when zero) rather than raising the 50.

## Configure can coarsen the granule of a suspended VM

A suspended VM has no supervisor process, so the control center reports it Stopped and offers
Configure. Flipping *Memory pages* from 4 KB to 16 KB there and resuming replays a guest layout
the coarser granule cannot express — blob maps refuse mid-replay. The `Suspended` record does not
carry the granule it was taken under, so a preflight cannot compare. For now the help text says to
shut down first; the real fix is to record the granule in the suspend metadata and refuse (or warn
on) a coarsening resume. Finer-than-saved is safe and needs no gate.

## M6 — the io give-back's availability denominator is the balloon itself

`GIVEBACK_AVAIL_CEILING_PCT` compares `mem_available_kib` against `mem_total_kib`, and
`mem_total_kib` is the *guest-visible* total, which the balloon controls. Measured on the
2026-08-14 restic ladder, balloon and total are near-perfectly anticorrelated (they sum to the
VM max):

| balloon | guest total | decline threshold = total/2 |
|---|---|---|
| 16.06 G | 7.65 G | 3.8 G |
| 8.56 G | 15.15 G | 7.6 G |

So the absolute bar for "this guest is comfortable" is **3.1 GiB when the balloon is at rest and
7.8 GiB when it is half empty** — the guard is most restrictive exactly when the balloon is
largest and so most likely to be the cause. That is the actuator-coupled-sensor defect from the
same day's `lead-lag` finding, rebuilt into the fix for it.

It is not purely a bug: the coupling is *why* the guard is self-limiting (each give-back raises
availability toward the ceiling and ends the ladder). Decoupling to `max_pages` or an absolute
figure loses that and, run against the same traces, is more permissive at rest. **Do not change
the denominator on intuition** — decide it against the stress-test trace, which will have many
episodes at different balloon fills rather than the two we have.

Also note the first give-back of any episode always fires, because availability starts below the
bar. The guard bounds a ladder; it does not prevent one starting.

## M6 — `MemFree` contributes almost nothing to the io guard

The `GIVEBACK_FREE_CEILING` doc comment and commit 7994695 both describe md5sum as the
*accumulating* reader that the free ceiling catches ("free grew 575 MiB -> 5.8 GiB"), against
restic as the *consuming* one that only availability catches. Replaying the 08-13 trace, that is
wrong: `MemFree` sat at **461-615 MiB through nearly every step of the md5sum ladder** — under
the ceiling, `free_ok = true` — and only spiked to 6.7 GiB *after* the ladder finished. The
endpoint reading was mistaken for the trajectory.

Of 103 give-backs in that trace the free guard alone would have fired 39; the combined guard
fires 17. On the restic ladder: 17 -> 17 -> 5. **`MemAvailable` is doing essentially all the work
in both**, and md5sum/restic are one shape, not two. Either re-justify the free half on evidence
or retire it — carrying a guard that never binds is worse than not having it, because the doc
comment claims coverage the code does not provide.

## GPU — a CPU write to a mapped LINEAR dmabuf stops being visible to the GPU — FIXED 2026-08-14

Reported by synoik 2026-08-14 (`vmm-issue-dmabuf-cpu-write-coherency.md` in their tree).
`gbm_bo_map` → write → unmap on a LINEAR `Argb8888` dmabuf, completed *before* the buffer is
imported and sampled, is not visible to a subsequent GPU read: the sample returns the buffer's
**previous** contents. The first write+sample always works; only a *re*-write through the mapping
goes unseen. A GPU-written producer (bind as render target, submit, fence-wait) is unaffected on
the same import cache, the same deferred acquire barrier and the same sampling path — which is
what localises this under the host-visible mapping's transfer rather than in image layout, queue
acquisition or any barrier the guest issues.

**Not urgent, but not dismissable either: CPU↔GPU coherency is a guarantee we should owe.**
Nothing in the product takes the path today — no Wayland client CPU-writes its buffer, it renders
and commits — so synoik is not exposed and has moved its test to a GPU-writing producer. What it
cost them was a day of misattributing a host bug to their compositor.

**ROOT-CAUSED 2026-08-14 — the transfer is issued and lands, it just runs too late.** Full
evidence, reproducer and fix plan: `spikes/dmabuf-cpu-coherency/RESULTS.md`. The guest's write
reaches the host as a virtio-gpu **control-queue** command executed by libkrun's gpu worker
thread; the venus read is executed by virglrenderer's **own ring thread**
(`src/venus/vkr_ring.c:811`) directly out of the shared ring memory. Two host execution paths,
no ordering between them, and the guest cannot impose one — `VIRTGPU_EXECBUFFER` is
fire-and-forget, so `gbm_bo_unmap()` returns long before the transfer is dequeued. Timestamped
on both sides: the upload *begins* 8 µs after the guest submitted the read and completes after
the read is done. The reporter's trichotomy answer is therefore **racing**, and their "the first
one always works" is the first venus submission's ~9 ms of pipeline setup letting the worker
thread win once.

**FIXED in two halves, and neither works alone.** (1) Guest mesa virgl — on unmap of a *write*
map of a `PIPE_BIND_SHARED` resource, flush and wait for the bo to go idle, which restores the
ordering; shipped as `mesa 26.1.5-8.limina.fc44`. (2) Host vrend — complete the upload before
returning, because the virtio-gpu fence the guest waits on signals when vrend *returns*, not
when the queued Metal upload executes, so half 1 alone can be satisfied with the bytes still in
flight. Measured with `LIMINA_VREND_SHARED_TRANSFER_SYNC` as the A/B lever: guest half alone was
clean warm (250 consecutive passes) but **failed the first write after a guest boot in 5 of 6
boots**; both halves, **0 of 7 boots**. Host-side *ordering* was never available (the vrend GL
context is current on the worker thread, so a ring-side barrier degenerates into
ring-waits-for-worker — the `try_park_present` deadlock shape). The stock tier keeps the bug,
since the host barrier alone does not fix it — a documented degradation; the long-term erase is
host-visible-blob backing for shared bos, which removes the transfer entirely and its upload
cost with it.

**Reproducer (cheap, local, no dogfood needed):** `spikes/dmabuf-cpu-coherency/probe.c`, built
and run in a clone of `Fedora-Workstation-44.enhanced.synoik.raw` — self-contained gbm+Vulkan,
no synoik checkout needed (the image's synoik predates the reporter's test). It is far crisper
than a rate: each failing pass returns *exactly* the previous pass's colour.

**Already ruled out — do not re-run this:** the vrend stride fix (virglrenderer `5c76245`). It was
the obvious suspect, since the failures were first noticed on the deploy that introduced it and it
changes exactly how the exported IOSurface is allocated (forcing `bytesPerRow` to Metal's minimum
linear alignment instead of IOSurface's own 256-byte choice). A/B on one guest image, one test
binary, only the host dylib swapped: **6/10 fail with the fix, 7/10 without**. Same result. The
reporter's own framing — the behaviour changed across a host *restart*, with no version change —
is the accurate one, and the deployment is not the variable.

**Trap worth keeping:** the host completion barrier, tried *first* and alone, was verified loaded
and hit and still failed 10/10 — because the problem was when the upload *starts*. It looked like
a dead end and was reverted; it only earns its place after the guest half fixes the ordering.
"This change did nothing" can mean "not yet", not "wrong".

Still open: the seam is **not transfer-specific**. Any control-queue work races the ring, so a
vrend GL render into a shared bo consumed by venus carries the same hazard for a consumer that
skips implicit sync. That the guest-side bo wait fixes this proves the guest kernel tracks the
fence on the bo, so `VIRTGPU_WAIT`/sync-file consumers are safe and a bare Vulkan importer is
not — unprobed. Formats/modifiers beyond `Argb8888` + LINEAR are also unmeasured.

## An idle guest misses frame deadlines

A Vulkan or GL client on an otherwise idle guest does not hold its refresh rate. `vkcube` alone on
a 59.885 Hz output runs at ~40 FPS, and the frame-time distribution is not scattered — it is
quantised to whole vblank periods, so frames simply miss their flip and slip one refresh. Give the
guest *any* other work and the misses stop.

The guest's timer wakeups are late, and the lateness is a **host thread scheduling** property, not
a renderer or compositor one: the load that fixes it carries no GPU work, and `nohz=off` fixes it
while adding no load. What those two share is that the guest stops waiting on a long, one-shot
timer — a busy CPU wakes on real work, a 1 kHz tick replaces the deadline. The stock tier's
per-frame CPU copy is present in every arm and is not it.

**It is not our WFI park.** On macOS 26.5 / Apple silicon a guest's `WFI` does not trap out to
libkrun: HVF parks the vCPU inside `hv_vcpu_run`
(`HvCore::Hypervisor::VcpuStateManager::wait_for_interrupt`) and serves the virtual timer from its
own `VirtualClock` thread. `vstate.rs::wait_for_event` and its `crossbeam` `after()` timeout are
dead code here — over 30 s of idle desktop, `WaitForEvent`, `WaitForEventTimeout` and
`VtimerActivated` are all zero. The `LIMINA_WFI_LATENCY` instrument stays in place to notice that
changing. `hv_vcpu_run_until` is not an escape either: `hv.h` puts it inside `#ifdef __x86_64__`.

HVF's wait nonetheless runs **on our vCPU thread**, and a scheduling band belongs to the thread.
`spikes/macos-timer-wakeup/` measures what that is worth: an ordinary macOS thread asking for a
16.667 ms deadline is served ~1.5 ms late at the median and tens of ms late in the tail, while
`THREAD_TIME_CONSTRAINT_POLICY` takes it to 18 µs median / 52 µs worst. Neither the wait primitive
nor the latency-QoS tier moves it at all.

**The band fixes the guest, and must be armed per vCPU.** Full matrix and method in
`spikes/macos-timer-wakeup/results-guest-arms.md`; the shape of it:

| policy | idle | six spinners | one spinner per vCPU |
|---|---|---|---|
| none | 43.6 / 39.2 FPS | 54.5 / 55.7 | 60.4 / 60.4 |
| band on every vCPU | 59.5 / 59.7 | 59.6 / 59.7 | **3 and 32 frames in 20 s** |
| band on vCPU 0 only | 52.1 / 53.0 | — | 59.9 / 57.8 |
| `QOS_CLASS_USER_INTERACTIVE` | 47.2 / 46.6 | 55.0 / 58.6 | 59.2 / 59.4 |
| **armed per vCPU from its CPU share** | **58.6 / 59.5** | **59.7 / 59.7** | **59.8 / 59.1** |

MangoHud counts the guest's presents, not the host's flips — 60.4 FPS on a 59.885 Hz output is more
frames than there were refreshes — so it sizes these effects but does not prove what reached the
screen. It is the right instrument for a 20 FPS difference and the wrong one for a 1 FPS difference.

The band on every vCPU thread is catastrophic once every vCPU has guest code to run: **the band is
a reservation, not a priority**, and eight real-time threads own the machine. Observed directly
during a collapse — all eight vCPU threads at 100% CPU, priority 97, every other thread in the
worker at 0.0%, and the venus ring thread's `signal->resume` at 28.7 ms average / 434.9 ms worst
against the 8-27 µs it measures unbanded. Banding one vCPU is clean under the same load; a QoS
class, which carries priority and no reservation, never collapses.

xnu's real-time fail-safe (`osfmk/kern/priority.c::thread_quantum_expire`, which demotes a
`TH_MODE_REALTIME` thread to timeshare after 1 s of computation without blocking, for 2 s) is **not**
the mechanism. What rules it out is the experiment, not the reasoning: a forced 100 µs park every
250 ms, which exists precisely to prevent the demotion, fires 1200 times in a run and changes
nothing, and declaring 15 ms of computation instead of 1 ms does not help either. The demoted state
is also the wrong size — `rt_overrun.c` measures it as ~1.7 s of few-millisecond lateness that the
OS then heals on its own, which cannot produce a 7-second frame.

The tempting shortcut — "a demoted thread is an ordinary thread, and unbanded is clean, so
demotion cannot be it" — **does not hold, and is worth remembering as a trap**: demotion is
per-thread and transient, so the steady state under that hypothesis is a mix in which several
threads are banded and busy at any instant, never the all-ordinary configuration being compared
against. A whole-system claim does not follow from a per-thread one.

So: sample each vCPU thread's own share of a core (`THREAD_BASIC_INFO`) every 200 ms, arm below
35%, disarm above 60%, one sampler thread per VM (`vcpu_sched.rs`). This is **the default** — the
supervisor sets `LIMINA_VCPU_SCHED=rt+dyn` for the worker unless the environment names a policy
(`worker_vcpu_sched` in `crates/limina/src/supervisor.rs`; an empty value turns it off, which is
how an A/B arm runs).
The vCPU that needs a punctual timer wake is the idle one, which is also the one whose reservation
costs the host nothing. The hysteresis gap matters: a policy change is the moment the present path
can lose its core, so a thread hovering at the threshold must not switch every sample. A *static*
choice will not do — banding vCPU 0 alone recovers only half the idle gap, because the deadline
that matters lives on whichever vCPU the guest scheduler put the client on, and that migrates.

**The transition window is measured, and it is benign — so this ships on by default.** A burst of
full guest occupancy costs no frame over 100 ms at any length from 250 ms to 8 s, under `rt+dyn`
*or* under the static band, and the *unbanded* arm is the one that suffers (77 and 123 frames over
33 ms at 250/500 ms bursts, against 2 and 29 banded) because it pays in the idle gaps between
bursts. Tables in `spikes/macos-timer-wakeup/results-burst-and-contention.md`. A global cap — never
more banded-and-busy threads than leaves the host a couple of performance cores, with asymmetric
hysteresis, disarming fast and arming lazily — is therefore hardening rather than a prerequisite.
The arm direction still has a cost of its own: a vCPU that has just gone idle waits a sample plus
hysteresis before it is punctual, which is a hitch exactly when a build finishes.

**Never judge the collapse from one run.** Sustained saturation under a full static band measured
60.6, 55.0 and 31.8 FPS on the same boot minutes apart; the first measurement of it, "3 and 32
frames in 20 s", is the same distribution's tail. The direction is reliable, the magnitude is not.
Host contention was the obvious explanation for that spread and is **not** it: ordinary-priority
host threads cannot preempt a banded vCPU, and sweeping 0/4/8 of them leaves the static band's
saturated case at 31.8 / 29.6 / 29.8 FPS. Dynamic arming beats the static band in every saturated
cell (58.9 / 52.2 / 35.0) and ties it in every idle one at 58-60 FPS.

**The band is not a battery cost.** Measured on battery, six-minute interleaved blocks, method
and tables in `spikes/macos-timer-wakeup/results-battery.md`: idle, the band and its 200 ms sampler
cost **under ~20 mW of package power** (banded 128/104 mW against unbanded 111 and an empty-host
floor of 98), and the pack cannot resolve them at all — every VM block lands inside the spread of
the no-VM blocks. Presenting, banded draws +154 mW (+18%) while delivering +21% frames (58.9/59.8
FPS against 47.7/49.6), so energy per frame is flat to marginally better. The pack is the blunt
instrument here — the display dominates it, and `AppleRawCurrentCapacity` is a self-refitting
estimate that once *rose* 34 mAh during a discharge, so never difference the mAh column.

**Still open before this is on by default.** The idle far tail does not fully clean up (max 31-49
ms even armed), so something rarer is still late. And the outer gate is still owed: **idle wakeups are a budget we already spent effort
winning** (`docs/design/venus-ring-idle-wakeups.md` took the worker from ~75/s to ~0/s), so the
sampler should idle entirely when nothing is presenting. Both inputs for that are available on a
**stock** guest, which keeps this off the agent's critical path: the host's own power state
(`NSProcessInfo.isLowPowerModeEnabled`, `IOPSGetProvidingPowerSourceType`) and whether anything is
reaching scanout. The guest's GNOME power profile is user *intent* rather than machine state and
belongs to the enhanced tier as a refinement, never a prerequisite.

The profile does exist on a stock F44 guest, which is worth knowing before designing around its
absence: there is no `cpufreq` and no ACPI `platform_profile`, but Fedora 44 backs the
`net.hadess.PowerProfiles` D-Bus API with **tuned** (not `power-profiles-daemon`, which is
`inactive`), offering all three profiles. How the enhanced tier should carry it: **a
`platform_profile` driver in our kernel backed by a virtio device** — not because the D-Bus route
is dead, but because a device write reaches the host without an agent in the path and works in both
directions, which is what a VM on a laptop going to battery actually wants.

**The venus ring thread does not need any of this.** Its wake path — guest doorbell → VM exit →
gpu worker → `cnd_signal` → `vkr_ring_thread` — measures 8-27 µs `signal->resume` (max 0.13-1.54 ms)
with no policy at all, flat across park-duration buckets rather than growing with the gap. That is
two orders of magnitude better than the ordinary-thread wake, and it points at what distinguishes
the two cases: the ring thread is signalled by a thread already running on a machine already busy,
while a vCPU waiting on a guest timer needs a core brought out of deep idle. The fault is specific
to **timer-driven wakeups on an idle host** — which is the rule to carry to the next candidate,
rather than banding threads on suspicion.


## An idle guest exits ~1,600 times a second reading virtio-net's interrupt status

Measured 2026-08-27 while chasing the frame-deadline misses above, on a stock Fedora 44 guest at a
settled idle desktop with `--net`: 48,489 MMIO *reads* in 30 s, 72,374 of them against `0xa01f060`
— offset `0x060`, `InterruptStatus`, on the device the guest maps as `a01f000.virtio_mmio` →
`virtio_net`. virtio-blk at `0xa01d060` is a distant second at 2,896; every other device is in the
tens. (MMIO writes are not logged, so the true exit total is higher; the read mix is the finding.)

No other logged exit kind appears at all. Whether it is a
normal consequence of gvproxy's traffic, an interrupt that is acked in a way that costs an extra
read per event, or a genuine storm has not been established — start by rerunning the same count
with `--no-net`, and against a guest with no NAT traffic to serve.

## The supervisor⇄worker control sockets should probably be Mach ports

Five UNIX sockets in the temp dir carry the whole supervisor⇄worker control plane: `limina-ctrl`
(the agent control plane), `limina-resize` (display control), `limina-balloon`, `limina-fido-usb`
and `limina-moc-usb`. Each is `srwxr-xr-x` at a **predictable path** — `$TMPDIR/limina-<kind>-<pid>.sock` —
so any process running as the same user can connect to them. That is the real objection, and it is
sharpest for the two that exist to carry authentication traffic: the FIDO gadget socket proxies
CTAPHID for a SEP-backed passkey store, and the MOC one carries fingerprint-reader protocol
gated on Touch ID. A local process that connects first, or that reaches them while a VM is up,
is talking to the authenticator path. Filesystem permissions are the only gate today.

**Mach ports would replace the gate with an unforgeable capability** and would also delete the
lifetime problem rather than managing it: a bootstrap-registered receive right dies with the
process, so there is no filesystem residue to clean up and no path for anyone else to find. Both
properties come from the same change.

There is prior art in-tree to copy: the scanout surface already does exactly this
(`window/present.rs`, `surface_rendezvous` — a per-process bootstrap name, `--surface-port-name`
to the worker, survives worker relaunches, with a graceful fallback when registration fails).

Worth checking before committing to it: whether a bootstrap name is meaningfully harder for
another same-user process to reach than a socket path (same-user bootstrap namespace lookup is
not obviously privileged); how the test harness, which connects to these sockets from outside,
would drive a port instead; and that the death/relaunch semantics survive the worker exiting via
`libc::_exit` on every guest power-off.

Until then the leak is *managed*, not gone: `tmpsock` removes what the run allocated, from all
seven `process::exit` sites via `exit_cleanup()`. A SIGKILLed supervisor still leaves a stray
socket, harmless because every binder unlinks before `bind()`. **Do not add a startup sweep that
reaps sockets whose embedded pid is dead — pids are recycled.** Sweeping the 6450 that had
accumulated here left five "live" survivors that had all been reassigned to unrelated system
daemons.

## Per-display layout memory: limina's half is fixed, one synoik fault remains

limina's side of the "only one host display's layout is kept" report is **fixed and verified
end-to-end** by dragging the window between two physical displays, in `host` and `dynamic` modes
alike: a migration is now a connector cycle and both compositors learn which display they are on
(`docs/design/stable-edid-hotplug.md`, `spikes/display-identity-hotplug/`). Per-display memory works
on mutter — each display's own scale returns on every switch, both stanzas retained.

It still does not work on synoik, for a reason that is synoik's and that **no EDID or event limina
can send reaches**: its config store keeps exactly one `<configuration>`, so configuring one display
*replaces* the other's stanza and the display you return to has nothing remembered. One stanza
cannot hold two displays. mutter keys its store on the whole monitor-spec set and accumulates.
Handed over as `LIMINA-display-identity-reread.md` in the synoik tree on the dogfood guest.

Also open, and unrelated to the above: `/sys/class/drm/card0-Virtual-1/edid` reads **0 bytes**
under both compositors while both have the full identity and mode list. They read the DRM connector
property, so nothing is broken, but anything reading EDID from sysfs sees nothing.

## GPU — data races in vrend and zink, found by ThreadSanitizer

**Fixed**, virglrenderer `7f67e9a0` and mesa `a0d96c18f02` / `c398247db94`. A boot plus a
notification workload went from **23 ThreadSanitizer reports to 1**. Kept here for the reproduce
recipe, the one that remains, and the negative result attached to all of it.

**Why this seam is ours to find.** virglrenderer's fence thread makes a *second, shared* GL context
current and calls `glClientWaitSync`, so it walks into zink's screen- and batch-level state from a
thread zink does not know exists. virgl-over-zink is an upstream-undertested combination — on Linux
virgl normally runs on radeonsi or iris — so nobody else is exercising this.

**What was wrong, and what it took.** Three kinds of fault needed three kinds of fix, and the third
is the interesting one:
- *Unsynchronised scalars*, several already half-atomic (`bs->fence.submitted` was set with
  `p_atomic_set()` but cleared with a plain store). Routed through `p_atomic_*`, writers **and**
  readers — fixing only the writers leaves the race, which is exactly what the first re-measure showed.
- *An unguarded list*: `bs->fence.mfences` is appended to by `zink_flush()` while `destroy_fence()`
  removes entries from the fence thread. No atomic fixes a dynarray; it took a lock, held only
  across the dynarray call and never across `FREE()`.
- *Two bitfields sharing a storage unit*: `ctx->blitting` and `ctx->unordered_blitting` are read on
  one thread while the threaded context writes neighbouring flags in the same unit. **Distinct
  bitfields in one storage unit are a single memory location** to the memory model, so no per-flag
  atomic can help — the fields have to be separated. Worth remembering the next time a race sits on
  a field nothing obviously shares.

**Still open — the one race left.** `zink_batch_reference_resource_move()` and its `_unsync()` twin
mutate the same batch object lists from different threads *by intent*. Locking that is a hot-path
perf decision, not a bug fix, so it is deliberately left alone.

**None of it explains the notification-text corruption.** The fixes do not move the damage rate, and
neither does `VIRGL_DISABLE_MT=1`, which deletes the fence thread and the whole cross-context seam.
Recorded so the theory is not re-opened for free; see `spikes/notification-text-corruption/RESULTS.md`.

**Reproduce.** Build mesa or virglrenderer with `-Db_sanitize=thread` into its own prefix, point the
worker at it (`MESA_PREFIX` / `LIMINA_VIRGL_PREFIX`, the latter added to `crates/limina-vmm/build.rs`
for exactly this), and boot with `TSAN_OPTIONS="halt_on_error=0 log_path=/tmp/tsan"`. Three traps:
- The TSan runtime must be **linked into the worker** (`RUSTFLAGS="-C link-arg=<tsan dylib> -C
  link-arg=-Wl,-rpath,<dir>"`). A `DYLD_INSERT_LIBRARIES` preload is stripped across the worker
  spawn and TSan then aborts with "interceptors are not working ... loaded too late".
- Instrumented mesa and instrumented virgl **cannot run together**: they link different TSan
  runtimes (Apple's vs Homebrew LLVM's) and only one may be loaded.
- **Boot alone is not enough.** Four of the races only appear once something renders, so run a
  workload before declaring a round clean.

TSan costs a lot of speed but does **not** perturb the graphics behaviour away — the
notification-text bug still reproduces at full rate under it, which makes it a rare non-destructive
oracle on this stack.

Raw reports: `spikes/notification-text-corruption/evidence/tsan-vrend-two-races.log` and
`tsan-zink-kk-boot-19races-plus-kkdraw-segv.log` (the latter also holds a hard SEGV in `kk_draw`,
reached through `vk_meta_blit` from vrend's `do_readpixels`). That SEGV is unrelated to the races:
it was a dangling read in our own poly-heap instrumentation, which held the CPU view of a
device-owned bump pointer in a global and dereferenced it on every draw. One process hosts two KK
devices (host zink-on-KK for vrend's GL, guest venus/vkr), so tearing either down left the survivor
reading freed memory. Fixed per-device; `limina-test::venus_replay` is the regression test, since it
destroys the venus device mid-test.

---

When a milestone's loose ends are all closed, fold the remainder back into the roadmap milestone
status. Greenfield milestones still ahead: **M7 USB**, **M8 audio + x86**. (**M6 dynamic memory** shipped
2026-06-26 — see `docs/design/m6-dynamic-memory.md` + memory `limina-m6-dynamic-memory`.)

## Efficiency is unmeasured except at idle

We now know two points on the curve and nothing else: an idle guest costs ~13 mW of package power
over an empty host, and a `vkcube` guest costs ~0.9 W (`spikes/macos-timer-wakeup/results-battery.md`).
Everything else — disk, network, a build, a video call, a desktop being *used* — has never been
measured in watts, so "limina is efficient" currently rests on the one workload that does least.

What a suite needs, beyond the block-and-sample harness that exists (`battery-cost.sh` +
`pm-align.py`):

- **A work-unit counter per workload**, because perf/W is meaningless without the numerator and
  every workload counts differently: frames for graphics, IOPS and MB/s for disk, packets for net,
  wall-clock for a fixed build. Watts alone rank a slow VM first.
- **Package power without a human in the loop.** `powermetrics` needs root, which is why today's
  run depended on the user starting it by hand; the pack alternative is display-dominated and
  cannot see effects under ~0.3 W. This is a use for the deferred privileged helper
  (`docs/design/privileged-helper.md`) — one shared helper exposing package power serves both a
  test suite and any runtime power policy we build later.
- **The comparison that would mean something**: the same workloads under Parallels and under
  Apple Virtualization.framework on the same host. "Replace Parallels" is a claim about perf/W as
  much as about features, and it stays unevidenced until then.

Traps already paid for, in the results file: interleave arms rather than running A then B (the
pack's voltage sags as it drains), verify per block that the differential reached the guest, and
never difference `AppleRawCurrentCapacity`.

## What the vCPU band charges the host

Measured 2026-08-28, two instruments; tables and method in
`spikes/macos-timer-wakeup/results-host-impact.md`.

**The host cost is throughput, and only throughput.** An 8-thread host job keeps 3452-3458 Miter/s
against an **idle** guest under every policy — eight reservations held by threads that are not
running take nothing, which is the design's premise. Against a **saturated** guest it keeps 2050
unbanded (roughly proportional sharing: eight host threads and eight busy vCPUs on ten cores), and
**538 with every vCPU banded** — 15% of solo throughput, a 3.8x slowdown, the job stretching from
9 s to 61 s. `rt+dyn` returns the unbanded numbers rep for rep, which is also the evidence that the
disarm is *complete* rather than partial: a sampler leaving one or two vCPUs banded would show
here, since the static arm shows what a handful of reservations costs.

**Host wake latency does not move at all.** Pooled over 5400 samples per cell, the share of
deadlines missed by more than half a frame is 19.4-19.9% for every arm against an idle guest and an
empty host's 22.0%, and 10.5-13.3% for every arm against a saturated one — the loaded cells are
*better*, the same "a busy machine wakes threads sooner" effect the guest side shows. That is what
the mechanism predicts: a real-time thread cannot be preempted, so ordinary host work is not woken
late by one, it is not run at all.

**Do not measure a tail with one sample of it.** An earlier pass here reported that banding cost
the host 8.6-25 ms of worst-case lateness. It was wrong twice over: half its cells were measuring a
guest that never loaded (see below), and the instrument — `wakeprobe`, ~80 s for one sample per
cell of a six-by-four lever matrix — produces a precise-looking number that reproduces nothing.
Reps of one arm disagreed by 2 ms to 25 ms and the ordering between policies flipped between
passes. Use `hostlate.c`: one wait, one policy, many samples, counts rather than a max.

**A load that never arrives reads as a result, not an error.** Spinners started as background
children of an ssh session take SIGHUP when the session exits, so a "saturated" cell measures an
idle guest — and it announced itself as the host's throughput under a saturated guest coming back
*identical to an empty host*. Start them `setsid nohup`, and have every loaded cell print the
guest's own idle percentage before measuring. Related: `pkill -f 'while :'` over ssh matches the
remote shell carrying the pattern and kills its own session, which under `set -e` leaves the script
dead and the VM holding the disk, so the next arm cannot boot.

Still owed: this measures one synthetic CPU-bound job. Host work that is I/O- or GPU-bound may
share differently.

## Supervisor features we decline to offer the guest (PSCI, and an inventory)

libkrun answers `PSCI_VERSION` with `2` — PSCI **0.2** (`hvf/src/lib.rs:1112`) — and lets
`PSCI_FEATURES` fall through to `NOT_SUPPORTED`. Linux only calls
`psci_init_system_suspend()` when the major version is ≥ 1, so no `PM_SUSPEND_MEM` ops are
registered and `/sys/power/mem_sleep` offers only `s2idle`. **Our guests use s2idle because
we never offered anything else**, not because s2idle was chosen.

The one that matters: PSCI 1.0 `SYSTEM_SUSPEND` (`0xC400_000E`) would give us an exact,
race-free "the guest is safely stopped" event. `timekeeping_suspend` is a syscore op, and
`syscore_suspend()` runs *before* `suspend_ops->enter()`, so the SMC arrives strictly after
timekeeping is frozen — which is precisely the moment the host-sleep bracket needs and
cannot currently observe (see `spikes/s2idle-monotonic/`). Host-side only, so it needs no
guest components. Costs: `mem_sleep_default` flips guests to `deep`, so the M9 park /
session-preservation work — which keys on the s2idle device-reset signature — needs
revalidating; and libkrun must implement the resume half (`entry_point_address` +
`context_id`, the shape `VcpuExit::CpuOn` already has).

Owed: a **full inventory** of supervisor-level features we could be exposing and are not —
the rest of PSCI 1.x (`SYSTEM_RESET2`, `CPU_FREEZE`, `PSCI_FEATURES`, `CPU_SUSPEND` idle
states), SMCCC/`ARCH_FEATURES`, PPTT/topology, and whatever else a guest probes and gets
`NOT_SUPPORTED` for. Each declined feature is a guest behaviour we inherit by default rather
than choose.

## AV1: the slice buffer can read as zeros when the host copies it

`av1_decode_bitstream()` (`virgl_video_vt.c`) recorded two of sixty `superres`
fixtures with the correct tile *size* but all-zero *contents* — the guest's slice
data was not visible to the host at the moment it was read. Intermittent: 2 of 360
captured frames, and only in that one clip.

It matters well beyond the fixtures. The capture path reads `buffers[i]` exactly as
the real decode path will, so the same window corrupts a frame handed to
VideoToolbox — silently, since a zeroed tile payload is still a structurally valid
bitstream. It also mimics a serializer defect closely enough to cost real time: the
stream fails to decode while every frame header is provably correct, which is the
same signature as a lost reference slot (see `spikes/av1-obu-serializer/RESULTS.md`).
Restoring the two payloads from the clip makes `superres` decode bit-exact, which is
what rules the serializer out.

Must be resolved before the AV1 decode path is wired up. Worth checking whether the
VP9 path, which takes the same buffers by the same route, has the same window.

## AV1: VideoToolbox does not return super-resolution frames

Measured 2026-08-30 on M4 Pro, macOS 26.5.2, driving the serializer's own output
(`spikes/av1-obu-serializer/vt-oracle.c`) against dav1d on the same rebuilt stream.

**The host's decoding is correct; only its output is not.** Every frame coded
without super-resolution comes back **bit-identical** to dav1d, and several of
those predict from super-resolution reference frames — which they could not do if
the references VideoToolbox holds were wrong. Five of the six fixtures agree
bit-exactly end to end; `superres` is the only one that diverges, on exactly the
frames that use it.

What comes back for those frames is a buffer at the frame's **coded** width whose
contents are, near enough, the **rightmost `coded_width` columns of the correctly
upscaled picture**:

| reading | pixels matching |
|---|---|
| the pre-upscale (un-upscaled) picture | **6.9 %**, mean abs diff 65 |
| the rightmost `coded_width` columns, 1:1 | **76.3 %**, mean abs diff 5.6 |
| the same, shifted one row up | **88.6 %** |

The residual sits almost entirely in the last ~64 columns (mean abs diff 22 there
against ~1 across the rest). A stride/base search over the correct frame finds
stride 640 / base 320 — the plain right crop — as the unique best fit, so the
geometry is a crop, not a resampling.

**It is not our serializer, and it needs none of our code to reproduce.** Stock
ffmpeg on the original clip, decoding the same file twice:

```
ffmpeg -hwaccel videotoolbox -i superres.mp4 -pix_fmt gray -f image2 vt/%03d.pgm
ffmpeg -c:v libdav1d          -i superres.mp4 -pix_fmt gray -f image2 sw/%03d.pgm
```

dav1d's per-frame mean luma is flat across the clip (127.00..127.43); VideoToolbox's
swings 110.44..142.76, low exactly on the super-resolution frames. That is a
two-command Radar repro that mentions no part of this stack.

**Two escapes are measured dead, not assumed:**

- *Upscale it ourselves.* Dead on geometry. The returned picture is not the
  pre-upscale frame — a coded-width frame would carry the whole image at 2:1
  (the clip's timecode box is simply absent from what comes back, and its bands
  sit at 1:1 positions, not half). Roughly half the picture is not in the buffer
  at all, so no filter can reconstruct it.
- *Ask for output buffers at the sequence's size.* Dead by measurement. Passing
  `kCVPixelBufferWidthKey`/`HeightKey` to `VTDecompressionSessionCreate` does
  return 640-wide buffers — holding the same wrong pixels, stretched: the mean is
  unchanged to four decimals (111.1663 vs 111.1662), while the non-superres frames
  stay bit-exact.

**Disposition: decode it in software, and keep refusal as the floor.** On the
first frame declaring `use_superres` the codec opens a dav1d decoder, replays
every unit since the last shown key frame into it so the reference state matches,
and stays on that decoder for the rest of the stream
(`virgl_video_dav1d.c`, `av1_route_unit` in `virgl_video_vt.c`). Superres streams
therefore play correctly, on both tiers, with no guest-side change.

Where the fallback cannot start — no dav1d in the build, a 10-bit stream, or a
frame history too long to have been kept — the frame is **refused** rather than
delivered wrong (`submit_unit`). That refusal keys on the stream's own
`use_superres`, never on the width that came back, so a host whose bug changes
shape is still caught; the width mismatch is kept as a separate sanity error.
A refused frame leaves the guest's surface untouched and the guest is not told,
because the video protocol has no reply path — see the entry on decode errors
being invisible to the guest.

The frame is still **submitted and decoded** — only its *delivery* is refused.
Because this host's internal reconstruction is correct, skipping the submission
would break the reference chain and silently corrupt every later frame. Anything
that "simplifies" this into skipping super-resolution frames at submit time is a
regression.

Super-resolution is rare in practice, which makes it likelier to surface as a
mysterious "the video looks wrong" report than as a decode failure — hence the
loud `virgl_error` and the fallback to software rather than a quiet drop. Worth an
Apple Radar: the decoder is right and only the output copy is wrong, which is a
small fix on their side.
