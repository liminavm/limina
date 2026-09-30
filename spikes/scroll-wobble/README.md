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

## Measured with the libinput oracle (2026-09-30)

`scripts/trackpad-oracle.sh <recording> <port> [fuzz]` replays a recording through the policy
into uinput clones in a stock F44 guest and measures libinput's finger scroll. On
`battery-3` (HID-tap path: 12 flicks, 12 slow scrolls that stop before the lift):

| fuzz | wrong-way deltas | slow scrolls with a wrong-way delta | scrolls with no GTK velocity at the stop |
|---|---|---|---|
| 0 | 18 | 11 of 12 | 6 |
| 4 | 5 | 5 | 8 |
| 8 | 2 | 2 | 11 |
| 16 | 0 | 0 | 11 |

- **The wobble is finger noise at rest.** The wrong-way deltas are 0.2–0.9 px in the last
  events of a scroll, one or two device units (0.01 mm). A fuzz turns libinput's hysteresis
  on (`hysteresis enabled` in its verbose log); at 16 none are left, and the slow scrolls end
  with zero velocity, so they do not coast.
- **Flicks reach GTK with enough history to coast.** A flick moves slowly for ~80 ms, then
  accelerates and lifts at its fastest: libinput sends 2–4 events over 15–55 ms, and GTK's
  `scroll_history_finish` (GTK 3 and 4) computes 2 600–8 600 px/s from them. An app that
  implements kinetic scrolling coasts on them. By their source, ghost (winit, whose Wayland
  backend reports no momentum) and gnome-terminal 3.60 do not: VTE scrolls its history itself
  and consumes the event (`Terminal::widget_mouse_scroll`, fallback scrolling on), so the
  GtkScrolledWindow around it never runs its kinetic scrolling. Both coasted in limina when
  macOS's momentum reached the guest as wheel events.
- **Back-to-back sample pairs each carry half a step** on the local-monitor path
  (`battery-2`: −15 and −13.7 against −30 to −50 for single samples), so merging a pair into
  one frame is right; sending each as its own frame made the steps less even.

