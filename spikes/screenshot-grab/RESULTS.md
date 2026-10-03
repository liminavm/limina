# Telling a live macOS screen-capture session from the outside

The pointer grab has to know when the interactive screenshot UI (Cmd-Shift-4's crosshair, the
Cmd-Shift-5 panel, Screenshot.app) is up, because a click that belongs to that UI must not take
the pointer — see `grab_policy::free_step`'s `capture_live` and `capture_tap::
screen_capture_session_live`. macOS publishes no API for it, so the oracle is assembled from
what is observable.

## What is true

- **The window server's hit test cannot see the overlay.** With the crosshair live over a
  fullscreen guest, `windowNumberAtPoint:` returns the guest's own window
  (`[HITTEST] hit=2486 guestwindows=[2486] guest=true`). The overlay intercepts at the event
  layer, not by covering anything, so `on_guest` answers honestly and uselessly.
- **The identity is the bundle id, `com.apple.screencaptureui`.** The executable is
  `screencaptureui`, its windows report owner `Screenshot`; matching either name is a way to
  find nothing.
- **The overlay window lives with the process, not with the session.** One window, layer 24,
  roughly display-sized (`(0,0 2560x1440)` full, `(13,7 2534x1426)` inset while it animates
  in), appears on the first Cmd-Shift-4 or Cmd-Shift-5. It stays on screen through Esc, through
  later sessions run by the same process (one window number across all of them), and through
  a completed shot. It goes only when the process exits: ~5 s after an Esc, ~12 s after a shot
  (measured 2026-10-03, macOS 26.6.2). A session started inside that window reuses the process;
  one started after it gets a fresh one. So an on-screen overlay means "a session is running or
  ran within the last few seconds", not "a session is running".
- **A process that does not exit leaves the overlay up indefinitely.** Observed on the dogfood
  Mac: one `screencaptureui` process alive for 30 hours with its layer-24 display-sized window
  on screen and no session running. A presence-only check refuses every grab for as long as
  that lasts.
- **The process is not the session.** A process serves every session started while it is
  alive, and outlives the last one. Its presence is a filter, never a verdict.
- **The Cmd-Shift-5 panel has windows of its own, above the overlay.** The control bar is layer
  1499 (`800x50`, bottom centre), with a second layer-1499 window beside it; hover tooltips
  are layer 1000. They come and go with the panel, so unlike the overlay they do mark a live
  session.
- **`kCGWindowMemoryUsage` tells nothing apart.** Every `screencaptureui` window, overlay and
  panel alike, live or lingering, reports the same value (2368 here, 2432 on the dogfood Mac).
- **A session can stay live indefinitely if something else eats its events** — that is what
  the grab bug did, and what makes "the process has been up for minutes" not mean "it is
  lingering idle".

- **The session outlives the shot by a few seconds.** After a region capture completes, a click
  2.7 s later still reads as a live session; one 8.6 s later does not. The gate refuses the
  grab for that window, and the click still reaches the guest as an ordinary button press — it
  is the *capture* that is withheld, not the click.

## The cursor tells a live selection from a lingering overlay

`NSCursor.currentSystem` (the system-wide cursor, whichever process set it) reads from an
ordinary accessory process with no grant. Measured 2026-10-03, macOS 26.6.2, 2x display:

| state | size (pt) | hot spot | pixels |
|---|---|---|---|
| arrow (idle, lingering overlay, over the Cmd-Shift-5 bar) | 28x40 | (5,5) | stable hash |
| Cmd-Shift-4 crosshair | 64x40 | (15,15) | **changes with every move** |
| camera (Space in Cmd-Shift-4; Cmd-Shift-5 window mode) | 28x25 | (14,11) | stable hash |

- **Fingerprint the crosshair by size and hot spot, never by pixels.** Its image carries the
  live coordinate readout, so its hash changes on every sample while the pointer moves.
- **Over the Cmd-Shift-5 bar the cursor is the plain arrow.** The cursor cannot see that part
  of a live session; the panel's layer-1499 windows can.
- **During a lingering overlay the cursor is the arrow too**, which is what separates it from a
  live Cmd-Shift-4 selection.
- **Focus does not see a session at all.** With the probe's own window key and the pointer over
  it, Cmd-Shift-4 (crosshair and camera), the Cmd-Shift-5 panel and a completed region shot all
  left the probe active, its window key and the frontmost application unchanged:
  `screencaptureui` never activates. So `isActive`, `isKeyWindow` and
  `NSWorkspace.frontmostApplication` cannot stand in for the cursor.
- **The API is on its way out.** The SDK marks `currentSystemCursor` deprecated and says it
  "will always be nil in a future version of macOS". A nil has to fall back to the
  presence-only answer, never to "no session".

## Why presence, not size

Discriminating the post-capture tail by bounds — count only display-sized windows, so a small
corner thumbnail does not — would also let Cmd-Shift-5's small control bar through, and that
one is a live session whose clicks must not be stolen. A few seconds of "the click acts but
does not capture" is the cheaper side of that trade.

Triggering the tail synthetically is not possible from inside limina: a scripted Cmd-Shift-3
aimed at a focused VM window is consumed by the soft keyboard grab and reaches the guest.

## The probes

`cursor-probe.swift` — opens a window standing in for limina's, then samples the system cursor
(size, hot spot, pixel hash), the focus state (frontmost app, active, key) and the
`screencaptureui` windows (number, layer, bounds, memory) at 10 Hz, printing a line whenever any
of them changes. Click into its window before starting a session.

    swiftc -O cursor-probe.swift -o cursor-probe && ./cursor-probe

`window-list-probe.swift` — samples `CGWindowListCopyWindowInfo(.optionOnScreenOnly)` at 3 Hz
and prints the whole list whenever a window appears or disappears, with owner, pid, layer,
alpha, bounds and front-to-back index.

    swiftc -O window-list-probe.swift -o window-list-probe && ./window-list-probe

The probe prints owner names for legibility, which is the Screen-Recording-gated part of the
list; without that grant the names degrade while pid, layer, bounds and order stay readable.
The shipped check reads only the pid, so it needs no grant.
