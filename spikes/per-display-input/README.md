# Per-display input — what binds an absolute device to one monitor?

limina wants a click aimed at the second virtual display to land on the second virtual
display. Today there is one absolute pointer normalised over one scanout; with several
connectors the guest's compositor lays the monitors out in a desktop space *it* chooses, so
a single absolute device is only correct while the guest's arrangement happens to match the
host's.

"One absolute device per display" removes that coupling — *if* the compositor binds each
device to one output. Reading the three compositors says the class of the device decides,
and that a pointer-class device is never bound:

| | binds a pointer-class absolute device? | binds touch/tablet? |
|---|---|---|
| mutter | no — the input mapper is handed only touch/tablet/pen/eraser/cursor/pad (`meta-backend.c:490`), and absolute pointer motion is transformed over the whole stage (`meta-seat-impl.c:2471`) | yes |
| KWin | no — `applyScreenToDevice` returns unless touch or tablet tool (`connection.cpp:627`); absolute motion maps to workspace geometry (`:388`) | yes |
| sway/wlroots | it would, but the automatic path is dead: sway reads `wlr_pointer->output_name` (`seat.c:711`) and wlroots' libinput backend never fills it (`backend/libinput/pointer.c:11`) | only via `map_to_output` config |

This is **not** a virtio limitation. Real touchscreens have the same problem — nothing links
a digitizer to a connector at the bus level, and libinput says so where the output name is
exposed: "Use of this function is discouraged… the caller must implement monitor-to-device
association heuristics" (`libinput.h:4385`). What compositors do instead is guess, and
mutter's guesses are the measurable ones: EDID vendor/serial/product substring in the device
*name*, a physical-size match within 10%, the builtin panel, or explicit config
(`meta-input-mapper.c:323`, `:371`, `:389`, `:419`).

We are better placed than real hardware for those heuristics, because we author the EDID
*and* the input device. What we have that hardware doesn't is churn: the panel behind a
connector changes as the user migrates a window, enters fullscreen or switches a display
off — while a virtio-input device's name and axes are config-space state read once at probe.

## What is being measured

1. **Class.** Does a tablet-class device bind to one output where a pointer-class one does
   not? The pointer is the control: it should span the desktop.
2. **Which heuristic fires.** mutter prints the candidate scores; the bits are
   `1` EDID vendor, `2` EDID partial, `4` EDID full, `8` size, `0x10` builtin, `0x20` config
   (enum at `meta-input-mapper.c:65`). Knowing *which* one fired decides what limina must
   put in the device name versus the axis resolution.
3. **Cohesion.** Does a tablet share the pointer's cursor and position, so that scroll — which
   a tablet cannot carry — still lands under the visible cursor? And do ordinary applications
   receive tablet input at all, or only clients that speak `zwp_tablet_tool_v2`?
4. **Churn.** Does the binding follow when the monitor behind a connector changes, or go
   stale? This is the half hardware never faces and the half limina lives in.

## The vehicle

`vtablet.c` creates one absolute device through uinput, of a chosen class, with a chosen
name and physical size, and moves it from stdin. uinput is faithful here because everything
the binding depends on — name, capability bits, axis resolution, the udev tags derived from
them — is identical whether events arrive over uinput or virtio-input.

Oracle: mutter logs the decision itself. Run the session with `G_MESSAGES_DEBUG=libmutter`
and the journal carries `Output candidate '<monitor>', score <bits>` and
`Matched input '<device>' with output '<monitor>'` (`meta-input-mapper.c:594`, `:619`).

Findings land in `RESULTS.md`.
