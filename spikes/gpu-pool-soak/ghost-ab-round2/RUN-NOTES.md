# Ghost/window A/B — round 2 run notes

Round 1 (`../ghost-ab-2026-08-13/`) produced one usable result and one void arm. This run fixes
the two things that broke it: every cycle returns to **zero windows**, and there is a full minute
of stillness at each zero point so segment means can average through the pool's idle breathing
(649 regions of swing in round 1, with the guest blob count pinned).

- **Sampler**: `../window-ab-sample.sh <dogfood-mac> 4444 <guest-user> samples.csv 60 15` — both
  ends of the pool on one clock, 15 s cadence, started 20:39.
- **Driver**: the Compositor session, on the seated gsrs session (seat0/tty5, Wayland, synoik
  pid 32874). It verifies every close twice — window count back to 0 via `synoik msg` **and** the
  recorded spawn PID gone — because a process outliving its window still holds a GPU context and
  would read as retention. That is round 1's failure mode in a different costume.
- **App**: `gnome-font-viewer` — GTK4, single window, static specimen list, no caret blink, no
  animation, no network, exits when the window closes.

## Baseline (idle gsrs seat, before any protocol window)

    host   528 MB / 3,678 regions, IOSurface 285 MB
    guest  44 blobs / 257 MB, 6 framebuffers, 6 DRM clients

## Intervals to EXCLUDE from scoring

Recorded as they are reported, so nothing gets scored that was not part of the protocol. A
known-bad interval that is excluded is worth far more than a clean-looking dataset with an
unknown defect in it.

| interval (local) | why |
|---|---|
| 20:48:20 – ~20:48:50 | Driver's one-window dry run to validate `gnome-font-viewer` teardown before committing 27 min to it. An open and a close; moves the blob count; not protocol. |

## Standing caveat: `Linger=yes` on gsrs

Linger was deliberately disabled for gsrs on 2026-08-09 — it keeps a permanent manager session
for uid 1002, which pushes GDM down a reauthentication path and produces a compositor-less
session that looks like a successful login but has no shell in it. Something re-enabled it since,
and **it is still enabled**: the driver's `loginctl disable-linger gsrs` was blocked by its own
permission layer and never ran. Do NOT record "linger fixed" against 2026-08-13.

The seat came up clean anyway, so this run is unaffected — but the zombie risk is live if the
seat is logged out and back in mid-run. If that happens, treat everything after it as a separate
segment rather than assuming continuity.
