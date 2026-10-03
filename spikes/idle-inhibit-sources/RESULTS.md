# Where each desktop keeps its idle inhibitors

What `limina-agent-session` (`guest/limina-agent-session/src/idle_inhibit.rs`) reads to tell the
host that a guest application wants the display kept on, and the measurements behind each
source. Measured 2026-10-03 on CoW clones of `Fedora-Workstation-44.enhanced.test.raw`, with
Plasma or sway installed from the F44 repos, and of `Fedora-Workstation-44.enhanced.synoik.raw`.

## The vehicle

- `idle-paths.sh <sway|kde|gnome|synoik>`, run as the seated user over ssh with no guest input,
  drives these paths in order and prints the helper's reports after each step:
  - nothing inhibiting;
  - an mpv Wayland inhibitor started while the session is already idle, then stopped while idle;
  - an `org.freedesktop.ScreenSaver.Inhibit` held from Python;
  - Firefox playing an audible clip.
- `ff-audible.sh` runs only the Firefox step, with `dbus-monitor` on the session bus, to show
  which inhibit calls Firefox makes and which of them anything answers.
- The host half is the worker log of the boot vehicle: one
  `display: keeping the host display awake (an application in the guest is inhibiting idle)` and
  one `display: the host display may sleep again` per inhibitor. The second comes about 10 s
  after the report clears, which is the host's `WAKE_HOLD`.

## Results

Every path below reached the host as one hold and one release, on every desktop listed for it.

| Desktop | Sources the helper follows | mpv (Wayland, start/stop while idle) | `org.freedesktop.ScreenSaver` | Firefox 150, audible |
|---|---|---|---|---|
| sway 1.11 / wlroots 0.19 | compositor | yes | nothing owns the name | yes (Wayland inhibitor) |
| Plasma 6.7, KWin 6.7.5 | compositor, PowerDevil | yes | yes (via PowerDevil) | yes (via PowerDevil) |
| GNOME (mutter 50.1, the enhanced build) | gnome-session | yes | yes (gsd-screensaver-proxy) | yes |
| synoik | compositor, gnome-session | yes | yes (synoik owns it) | yes |

## Facts the design rests on

- **Firefox only takes its wake lock for audible video.** A silent clip playing in a visible
  window produced no inhibit call of any kind.
- **Firefox stops at the first route that answers, in this order:**
  1. `org.freedesktop.ScreenSaver.Inhibit`;
  2. `org.freedesktop.PowerManagement.Inhibit`;
  3. the portal's `Inhibit` (flag 8);
  4. `org.gnome.SessionManager.Inhibit`;
  5. a Wayland `zwp_idle_inhibit` inhibitor, only once all of those have failed.

  On sway the D-Bus routes fail or do nothing, so the inhibit ends up in the compositor. On KDE
  the first one answers: KWin owns the name.
- **KWin's ext-idle-notify ignores D-Bus inhibits.** With an `org.freedesktop.ScreenSaver` inhibit
  held, swayidle's notification still went idle after its timeout. KWin passes those inhibits to
  PowerDevil 3–5 s later, and from then on
  `org.kde.Solid.PowerManagement.PolicyAgent.HasInhibition(4)` (ChangeScreenSettings) is true
  and the inhibit is listed in `ActiveInhibitions`. KDE's truth is therefore split, and the
  helper reads both halves.
- **KWin does not resume an idle notification when an inhibitor appears, and wlroots does.** mpv
  started after both notifications had gone idle left KWin's inhibitor-respecting notification
  idle. The helper therefore replaces that notification every 20 s while input is idle.
- **On GNOME, a Wayland inhibitor ends up in gnome-session too.** mpv's inhibitor showed up as an
  idle inhibit there, read by the helper's gnome-session source alone.
- **synoik offers ext-idle-notify-v1 version 2** (smithay `2a1aab2`,
  `src/wayland/idle_notify/mod.rs:119`), and its `refresh_idle_inhibit` feeds both its D-Bus and
  Wayland inhibitors into it. gnome-session runs alongside it and says "no" throughout, which is
  why the sources are OR-ed rather than one being chosen.

## Traps met on the way

- `pkill -f firefox` over ssh kills the ssh session's own shell, because its command line contains
  "firefox". Use `pkill -x firefox-bin`.
- Plasma with GL compositing never finished starting on the enhanced image. The test ran with
  `KWIN_COMPOSE=Q` in `/etc/environment`. Under QPainter compositing mpv's GPU outputs abort
  (`egl: failed to create dri2 screen`), so the script plays mpv through `--vo=wlshm`.
