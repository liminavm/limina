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
- **The session's window comes and goes with the session.** One overlay window, layer 24,
  roughly display-sized (`(0,0 2560x1440)` full, `(10,6 2540x1428)` inset), appearing on
  Cmd-Shift-4 and gone on Esc.
- **The process is not the session.** Each session starts a fresh process, and the process
  outlives the session's end. Its presence is a filter, never a verdict.
- **A session can stay live indefinitely if something else eats its events** — that is what
  the grab bug did, and what makes "the process has been up for minutes" not mean "it is
  lingering idle".

## Unmeasured

Whether the post-capture floating thumbnail is an on-screen window owned by the same process.
If it is, it reads as a live session for its few seconds of life — mild, and separable by
bounds (display-sized overlay vs. small corner window) if it ever matters. Triggering it
needs a real keystroke: a synthetic Cmd-Shift-3 aimed at a focused VM window is consumed by
the soft keyboard grab and reaches the guest instead.

## The probe

`window-list-probe.swift` — samples `CGWindowListCopyWindowInfo(.optionOnScreenOnly)` at 3 Hz
and prints the whole list whenever a window appears or disappears, with owner, pid, layer,
alpha, bounds and front-to-back index.

    swiftc -O window-list-probe.swift -o window-list-probe && ./window-list-probe

The probe prints owner names for legibility, which is the Screen-Recording-gated part of the
list; without that grant the names degrade while pid, layer, bounds and order stay readable.
The shipped check reads only the pid, so it needs no grant.
