# RESULTS — pointer units oracle

Measured 2026-08-18, two-panel rig (BenQ 2560x1440 + built-in 3024x1896), F44 enhanced clone
(`units-oracle.raw`), fullscreen across both panels, pointer captured (2354 `[CAP]` lines, one
grab transition). Vehicle: `boot-enhanced-efi-kk.sh`, which passes `--display-size 2560x1440`
(fixed mode). Raw logs kept beside this file (`wire*.log`, `guest*.log`,
`layout-snapshot-round2.txt`); human sweeps by the user, correlation by `correlate.py` + ad-hoc
fits recorded here.

## Measurement 1 — UNITS: the guest spreads the absolute range over LOGICAL extents

The rig accidentally provided the mixed scales the question needed: mutter chose **scale 1.25**
for the fixed-mode 2560x1440 connector and 2.0 for the internal panel (D-Bus GetCurrentState,
`layout-snapshot-round2.txt`): guest logical layout = Virtual-1 (BenQ) 2048x1152 at (0,0),
Virtual-2 (internal) 1512x948 at (2048,0).

- logical model predicts the seam at 2048/(2048+1512) = **0.5753** of the range
- pixel model predicts 2560/(2560+3024) = **0.4585**
- measured (round 2, layout pinned by the snapshot, 324 correlated samples):
  crtc-0 = BenQ occupies ABS_X ∈ [0, **0.5768**], linear fit residual **5 px**
- round 1 (layout inferred post hoc): same seam 0.5765, residual 7 px

**Verdict: LOGICAL.** The pixel model is dead by >0.11 of the range. The mapping is linear
within each monitor (5 px residual over 2560 px), and a live point-check closed the loop on
both axes: last sent ABS (21196, 12527) → predicted internal pixel (510, 881), observed cursor
plane (503, 878).

## Measurement 2 — the "unreachable band" is the host and guest running different rects

The host's relay-fed Desktop believed BenQ-logical = 2560/2 = **1280** (wl_output carries only
integer scale; the agent divides the mode by it — `wayland_outputs.rs`), giving a host seam of
1280/(1280+1512) = 0.4585 while the guest's truth was 0.5753. The 0.4585 is itself measured,
not inferred from the code: the ABS_X histogram of the captured-phase wire trace piles 125
events into the single 0.001 bin at u=0.458 (the clamp edge of sweeping against the band),
while the rival gap-preserving composition (seam 0.3596) has zero events near it.
Consequences, all felt by the user and matching the arithmetic:

- sweeping the whole physical BenQ emits u ≤ 0.4585, which the guest maps to x ≤ 2036 of 2560:
  the right **20.3%** of the BenQ is unreachable (user's blind estimate: "~20% early");
- positions on the internal panel are offset by the same disagreement on the other side of the
  seam (hit testing lands far from where the user aims).

So the open captured-pointer symptom on THIS rig is not the two-device fight and not the
captured state machine: it is **wrong logical rects under fractional scale** feeding a correct
logical-model mapping. Fixed in `513fe03` (the relay reports `zxdg_output_v1` logical rects) and
re-measured on the same rig: the guest journal reports Virtual-2 at x=2048, the wire seam pileup
sits at 0.575 (118 events; 2 near the old 0.4585), max u = 1.0, and the user confirms clicks land
true with the whole BenQ reachable. After a live rearrangement the seam pileup moved to 0.425 =
1512/3560 — the mapping tracks the relay's report.

## Measurement 3 — the sprite is drawn far from where the guest's cursor is

Photographed (user's camera; screenshots don't include the cursor): with the guest's hardware
cursor at internal pixel (503, 878) — confirmed by the hover highlight on gnome-control-center's
Notifications row — the composited sprite is visible at roughly (0.35, 0.40) of the panel,
versus the truth at (0.17, 0.46). Clicks land where the guest says; the sprite lies. Separate
defect in the per-window cursor compositing path (16a2f77), cause not yet identified; also the
likely mechanism behind "the cursor vanishes entering the internal panel" (drawn out of view).

## Also observed

- The per-panel identity DID reach both connectors in fullscreen (guest Displays names them
  "LMN 23"" / "LMN 14""); the windowed fixed-`--display-size` state keeps the generic
  `RHT krun-display` identity on the primary (monitors.xml) — the vehicle's fixed mode is a
  mixed-identity configuration worth keeping in mind when reading guest-side monitor matching.
- The guest default-arranged the two monitors opposite the host's physical arrangement
  (Virtual-1 left of Virtual-2 by connector order). That is the booked host→guest arrangement
  relay gap, not a bug in the mapping.
- mutter's per-CRTC hardware-cursor model shows cleanly in the DRM state: exactly one cursor
  plane bound at a time, alternating as the pointer crosses.

## Protocol notes (for the next run)

- `ssh 'sudo bash -s' < script` produced no output; copy the script into the guest and run
  `sudo bash /tmp/poll.sh` instead.
- Snapshot the guest layout DURING the phase (GetCurrentState) — round 1 was nearly
  uninterpretable without it, and Settings visits change the regime mid-recording.
- The poller samples only cursor planes; monitor liveness must come from the layout snapshot,
  not from "two cursor planes bound" (the per-CRTC model means that almost never happens).

## Measurement 4 — desktop-space capture verified on the rig (2026-08-19)

Same rig, `sweep-verify.raw` clone, desktop-space capture (`ba0cde7`, the reveal fix folded in),
guest layout re-confirmed identical (Virtual-1 2048x1152@(0,0) scale 1.25, Virtual-2
1512x948@(2048,0) scale 2.0; seam 0.5753). Human sweep + held-button drag, wire histogram over
9435 ABS_X events:

- full range reachable (u spans 0.0000..1.0000);
- **no seam pileup**: 22 events spread evenly within ±0.004 of 0.575 (the r11 bug class was
  125 in a single 0.001 bin); the only pileups are at 0.0 and 1.0 — the true union edges,
  pushed deliberately during the release tests;
- zero `park warp is` lines (conservation held);
- user-verified: sweep and held drag cross the seam cleanly, hover feedback on panel 2
  persists after clicks there (the r11 hover-loss dissolved with the regime that carried it),
  the composited sprite sits where clicks land, Ctrl-Opt frees, the hot corner fires, and the
  chrome ask arms/releases on the primary. Outer-edge release is untestable with both panels
  covered (every edge is a dead edge) — exercised by the grab_policy tests instead.

## Measurement 5 — the "ghost cursor" is TWO defects, split by the [CURSOR] oracle

Trigger (user-found): changing the **display arrangement** in guest Settings (a scale change
does not trigger). Two mechanisms observed the same evening:

1. **Guest: mutter leaves the hardware cursor plane armed while software-painting.** After the
   rearrangement the [CURSOR] trace went silent for minutes of motion (2171 move lines before,
   then one idle-sync), while the DRM state kept crtc-0's cursor plane bound (fb=63) at the
   stale position — our composite faithfully showed it; a physical monitor would too.
   Recovered on a CRTC crossing. Guest-side; possibly related: a flood of
   `SUBMIT_3D → ERR_UNSPEC` kernel errors a few minutes earlier (mutter 50.1-1.limina).
2. **Host: AppKit unhid the parked NSCursor behind the hide refcount** (the arrangement change
   reshapes our windows). Static ghost wearing the LIVE guest shape (apply_cursor kept dressing
   it), floating over the overview animation, immune to guest repaints, cleared only by a grab
   toggle. FIXED `64aee92`: while captured the pointer wears the transparent blank, whatever
   the guest sends (pure WearState + tests).

## Measurement 6 — the arrangement-diagram cursor growth is in the guest's own pixels

Symptom (user-found): hovering the arrangement diagram in guest Settings → Displays, the
cursor "either disappears or grows larger"; the grown glyph is blurry, ~2× the arrow.
Reproduces with the BenQ at 100% and with BOTH outputs at 100% — display scale exonerated.

Pixel ground truth (VM relaunched with `LIMINA_GLOBAL_SCANOUT=1`, live cursor IOSurfaces
dumped with `iosdump` while the user parked on the grown cursor): the normal arrow (id 245,
hot 3,1) fills **13×22** of its 64×64 buffer; the grown cursor (id 238, hot 5,5) is the same
arrow at **23×39** — pre-upscaled (soft edges) *inside the buffer the guest uploaded*. The
host path is invariant across both: identical 64×64 buffers, identical composited frame
(no [CURSORLAYER] change through the repro), worker copies the virtio cursor buffer verbatim.

Verdict: **guest-side** (mutter's client-cursor path serving the cursor GNOME Settings sets
over the diagram at ~2× — upscaled from a 1× render, independent of output scale). Our stack
displays exactly what it is handed. Booked with the other mutter cursor defect (§5.1).

Poller cross-check: 33109/33112 bound samples show exactly one cursor plane; the 3 exceptions
are transitional crossings.
