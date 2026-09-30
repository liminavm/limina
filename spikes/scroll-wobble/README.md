# Scroll wobble: what each layer does with a two-finger scroll

Two-finger scroll in the guest wobbles, with the content moving slightly back and forth,
most visibly when a move ends. The instruments below each see one layer. All of them stamp
wall-clock time, and the guest clock is anchored to the host's, so their logs line up.

| layer | instrument | clock |
|---|---|---|
| host: touches in, frames out | `LIMINA_TRACKPAD_RECORD=<file>`, `LIMINA_POINTER_WIRE_TRACE=1` (`[WIRE] t=… dev=touchpad`) | wall-clock µs |
| guest kernel: the device's raw stream | `spikes/mt-raw-capture/guest-evdev-log.py` (root) | wall-clock µs (`EVIOCSCLOCKID` = `CLOCK_REALTIME`) |
| guest libinput: scroll it computes | `sudo libinput debug-events --device /dev/input/eventN` | seconds since start |
| compositor → client: Wayland axis events | `wev` (Fedora package), run in the seated session | protocol ms |
| Firefox: wheel events and scroll position | `scroll-probe.html` (this directory) | wall-clock ms |

`scroll-probe.html` logs every `wheel` event and the page's scroll position on every
animation frame. It plots the last 4 s and marks each direction reversal of the scroll
position in yellow. **Save JSON** downloads the full log for correlating with the other
instruments. Copy it into the guest and open it as a `file://` URL.

## Evidence

`wev/` holds `wev` logs of two-finger scrolls on the dogfood guest, running build
`Limina-2026-09-29-0`, scrolling over `wev`'s own window: `fast-flicks.log` (six flicks) and
`slow-then-release.log` (six slow scrolls). Measured from them:

- **Flicks reach the client as 1–4 axis events** (17–50 ms of scroll, then `axis_stop`), and
  none coasted. With one event GTK has no time span to compute a velocity from.
- **Slow scrolls end in sub-pixel reversals**: 3 of 6 carry 1–3 deltas of the wrong sign
  (−0.2 to −0.7 px) in their last few events.
- **Steady scrolls are uneven**: ~5 px per event with an 11–16 px event every few frames, about
  twice the step.
