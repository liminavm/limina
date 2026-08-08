<!--
SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
Copyright © 2026 Gustavo Noronha Silva
-->

# Display-chain monitor — and the fullscreen inset feedback loop (2026-08-08)

A live-repro harness for EDID/monitor bugs, plus the two defects the first session with it
found. The user demonstrated by **physically plugging and unplugging a BenQ over HDMI** while the
VM was in native fullscreen.

## The harness

Four links, one oracle each — so a demonstration says *which* link broke rather than just "the
guest looks wrong":

| link | oracle |
|---|---|
| host: window + remembered state | `osascript` window frame, and the `--window-state-file` state.toml (whose `fullscreen_display` IS the EDID identity hash) |
| limina → guest | `display-control: pushed …` in the supervisor log — the exact wire command, EDID and all |
| guest kernel | `/sys/class/drm/card*-*/{status,enabled,modes,edid}` |
| guest compositor | mutter `GetCurrentState` over the session bus + `~/.config/monitors.xml` |

- `watch.sh <ssh-port> <out-dir> [interval]` — appends a diffed block to `timeline.log` only when
  something changed. Leave it running for the whole session; it is the scrollback.
- `sample.sh <ssh-port> <out-dir> [label]` — one deep sample (full EDID base64, journals).
- `LIMINA_DISPLAY_TRACE=1` (added by this work, `window/mod.rs`) — the *inside* view: what
  fullscreen inset was measured from which view, and what each host-mode push derived from it.

Boot used: `RUST_LOG=info LIMINA_DISPLAY_TRACE=1 LIMINA_DISK=edid-repro.raw \
LIMINA_EXTRA_ARGS="--window-state-file …/edid-repro.state.toml" \
spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`

Captured evidence: `samples/hotplug-repro-2026-08-08-{worker.log,pushes.txt,timeline.log}`.

## Defect 1 — the window and the guest resolution feeding each other (FIXED)

**Symptom.** Every host-display change drove **eight** modesets converging on a size ~114 guest px
short of the panel: `2560×1440 → 2560×1326` on the BenQ, `3024×1898 → 3024×1784` on the built-in.

```
"display id=0 size=2560x1440 … name=BenQ%20LCD"      ← correct identity pushed
"resize 2560 1382" "resize 2560 1354" "resize 2560 1340" "resize 2560 1333"
"resize 2560 1330" "resize 2560 1328" "resize 2560 1327" "resize 2560 1326"   ← the cascade
```

**Cause.** Two mechanisms in the render tick, pointed at each other:

1. the host-mode push reshaped the window to the guest's aspect (`reshape_to_aspect` +
   `setContentSize`) on **every** size change, fullscreen included — and clamped it into the
   *visible frame* (screen minus menu bar and Dock), which a fullscreen window is deliberately
   not confined to. `setContentSize` is not a no-op there: it shrinks the content view *inside*
   the fullscreen window.
2. the camera-housing inset was learned each tick as `screen.frame.height − contentView.height`
   — measured from that same, now-shrunken view.

So: shorter view → bigger apparent inset → shorter guest → reshape → shorter view. Area-preserving
reshape makes each step a geometric mean, which is the halving convergence in the log. The trace
caught it on the very first tick:

```
learn  frame_h=982 view_h=949 observed=33     ← honest: 33 pt of housing
push   want=(3024,1898) fullscreen=true reshape=(1512,949)->(1324,831)   ← shrinks a FULLSCREEN window
learn  frame_h=982 view_h=831 observed=151    ← poisoned
push   want=(3024,1662) … reshape=(1324,831)->(1415,778)
learn  frame_h=982 view_h=778 observed=204
```

**It needs no hotplug** — any fullscreen restore does it. That is what made it cheap to fix.

**Fix.** Two pure policy functions in `window/fit.rs`, unit-tested from these exact numbers:

- `migration_reshape(..., fullscreen)` → `None` in fullscreen. A fullscreen window's shape is the
  screen's; AppKit owns it.
- `fullscreen_inset_measurement(screen, view)` → `None` unless the view spans the screen's full
  width. A magnitude bound does not catch this: 151 pt of "inset" looks perfectly plausible, and
  the pre-existing `0..200` check passed it.

**After:** one push, `3024×1898`, inset steady at 33 pt for 5233 consecutive ticks, guest at the
full panel resolution.

## Defect 2 — the guest can be left one step behind a push train (OPEN)

`monitors.xml` had saved `3024x1784@120 scale 1.333`; the last push was `resize 3024 1784`; but the
guest's DRM mode list *and* mutter both settled on **1786** — one step back in the cascade. The
monitor spec then matched nothing saved, so GNOME fell back to the default 200% scale. That is the
"my scale isn't restored" half of the report.

The window is in the event-bit protocol: the worker applies an update, sets
`VIRTIO_GPU_EVENT_DISPLAY` and signals a config change
(`gpu/worker.rs:658`); the guest re-reads `GET_DISPLAY_INFO`/`GET_EDID` and then writes
`events_clear`, which the device applies as `events_read.fetch_and(!clear)`
(`gpu/device.rs:433`). An update applied *between* the guest's read and its clear has its bit
wiped and never produces another interrupt — the guest keeps the older mode.

Defect 1's fix removes the burst that made this fire (one push, not eight), so it is masked, not
fixed. A real connect/disconnect still queues several updates in quick succession —
`can_merge` deliberately refuses to merge connectivity changes — so the window stays open.
Candidate fix: the host serves `GET_DISPLAY_INFO`/`GET_EDID` itself, so it can track whether the
guest has re-read since the last applied update and re-raise the event on a clear that would
otherwise lose one.

**Left unfixed by decision (2026-08-08), after the verification below found it unreachable in
practice.** Four real plug cycles produced no mismatch at all: every guest mode equalled the last
pushed size. Revisit if a mode/scale ever disagrees with the host again.

## Verification on the real HDMI path (2026-08-08)

Four physical plug/unplug cycles of the BenQ, VM in fullscreen, protocol: set a distinctive scale
per monitor, then cycle and see whether each comes back.

- **One push per display change** (8 pushes / 8 changes), never a cascade.
- The inset sensor read only `Some(33.0)` on the built-in (28774 ticks) and `Some(0.0)` on the
  BenQ (4195 ticks) — no poisoned value, ever. `reshape=…->None` on every fullscreen push.
- Guest resolution equalled the pushed size every time: `3024×1898` / `2560×1440`.
- GNOME restored each monitor's own scale in both directions — BenQ 1.667 (later 1.333),
  built-in 1.0 — keyed on the identities limina generates (`LMN/BenQ LCD/0x6c42fae5`,
  `LMN/Built-in/0x31d7dd41`), which is defect 2's symptom gone with its cause.
