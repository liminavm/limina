# Results — per-display absolute input

Measured 2026-08-18, F44 enhanced guest (kernel 7.1.8-limina16k, GNOME/mutter), two host
panels: a BenQ 2560x1440 and the MacBook built-in, VM fullscreen across both, connectors
`Virtual-1` (BenQ, `LMN`/`BenQ LCD`/`0x6c42fae5`) and `Virtual-2` (built-in, `LMN`/`Built-in`/
`0x31d7dd41`). Devices created with `vtablet.c` through uinput; oracle is mutter's own mapping
debug (`G_MESSAGES_DEBUG=libmutter`) plus a GTK4 client (`evprobe.py`) that prints what it
actually receives, plus the user's eyes on the two panels.

## The class rule is real, and the binding works

A tablet-class device bound to exactly the monitor whose identity its name carried:

    Applying mapping 0 to input device 'limina display LMN 0x6c42fae5', type 4
    Output candidate 'LMN 23"', score c
    Matched input 'limina display LMN 0x6c42fae5' with output 'LMN 23"'
    Applying mapping 0 to input device 'limina display LMN 0x31d7dd41', type 4
    Output candidate 'LMN 14"', score c
    Matched input 'limina display LMN 0x31d7dd41' with output 'LMN 14"'

Score `0xc` = `META_MATCH_EDID_FULL` (`0x4`) | `META_MATCH_SIZE` (`0x8`): the device name
contained the monitor's EDID serial string, *and* the axis resolution gave it a physical size
within 10% of the monitor's. Both heuristics fire, so either alone would have sufficed.

**The control settles the class question.** A pointer-class device created with an
*identical* matching name (`…0x6c42fae5 ptr`, same size) produced **no mapping lines at all** —
mutter never considers it. That is `meta-backend.c:490` observed rather than inferred.

**Both tablets drive their own display.** With one sweeping on each, the user saw two cursors
moving independently, one per panel. A tablet cursor exists only while the tool is in
proximity (`meta-wayland-tablet-tool.c:907-935` creates the cursor renderer on proximity-in and
destroys it on proximity-out), and libinput drops proximity 50 ms after the last event — so the
device must stream at motion rates. A sweep driven one `echo` per step was invisible however
slowly it moved; the same sweep at 60 Hz from inside the process was immediately visible. Not a
bug, and worth knowing: **proximity is the mechanism for "the pointer is on this display"**.

## Ordinary apps see ordinary pointer events

The compatibility worry — that a tablet tool is a separate Wayland device (`zwp_tablet_tool_v2`)
that only tablet-aware clients handle — does not survive contact. mutter gives **each device its
own logical pointer**, and a plain GTK4 client received:

    MOTION 250,84 from 'Logical pointer for limina display LMN 0x6c42fae5' source=mouse
    PRESS  250,84 n=1 from 'Logical pointer for limina display LMN 0x6c42fae5' source=mouse

Motion and buttons arrive as normal pointer input, correctly positioned within the window, on
the right display. No client changes, no tablet awareness needed.

This also explains why XWayland's core pointer never moved during any sweep
(`xdotool getmouselocation` stayed at 1280,720 throughout): the tablet's logical pointer is a
*different* pointer from the core one, not a lack of delivery.

## Scroll is the wall

A tablet-class device cannot carry a wheel. Declaring `REL_WHEEL` + `REL_WHEEL_HI_RES`
alongside the tablet axes changed nothing: libinput still reports `Capabilities: tablet` only,
and in the same run that delivered `MOTION` and `PRESS` to the GTK client, **no scroll event
arrived at all**.

Scroll therefore has to come from a pointer-class device. Scrolling the host trackpad over the
guest window — limina's ordinary path — delivers it, and shows which pointer carries it:

    SCROLL +0.00,+0.09 from 'Wayland Wheel Scrolling' source=mouse

`Wayland Wheel Scrolling` is GDK's device for `wl_pointer` axis events: the **seat's core
pointer**, the same one whose motion arrives as `Core Pointer` — not one of the per-device
logical pointers a tablet gets. A wheel event therefore applies at the core pointer's position,
in whole-desktop space, unbound to any output. So a scroll sent while the user is pointing via a
tablet lands wherever that other pointer happens to be, not under the visible cursor.
Per-display tablets solve pointing and break scrolling.

## Churn: it re-maps, and the fallback is ours to control

Leaving fullscreen removed the built-in's monitor. mutter did not leave a stale binding — it
re-ran the mapping and bound that tablet to the *surviving* monitor at score `1`
(`META_MATCH_EDID_VENDOR`), because every EDID limina synthesises carries vendor `LMN`, so the
vendor bit matches every monitor. Re-entering fullscreen restored both to score `c`.

Benign in itself (limina would not be sending events for a display it is not showing), but it
means a device is never left unbound, and the vendor string is doing more work than intended.

## What this means for limina

Per-display tablets are viable on mutter as a *pointing* mechanism and would need no guest
configuration — the device name and the axis resolution are both ours to set, and either match
alone is enough. What they are not is a drop-in replacement for the pointer, because scroll
cannot ride them. Any design using them needs a separate answer for the wheel, and that answer
has to put the wheel at the cursor's position — which is the arrangement problem the tablets
were introduced to avoid.

Not measured here, and still open if this route is taken: KWin (its tablet path is separate
plumbing from `applyScreenToDevice`), and whether a virtio-input device can even be named after
a panel it does not yet own, since names are config-space state read once at probe.

## Incidental findings (not the spike's subject)

Both are limina bugs found while setting the rig up; see the session's write-up.

1. **The window's own panel keeps libkrun's boot EDID until it migrates.** Slot 0 boots
   connected carrying the device's default EDID (`krun/mod.rs:291`), and the guest's DRM driver
   probes it long before limina's first push — which arrives only after the guest presents a
   frame. Nothing then makes the guest re-read, because slot 0 never cycles. Measured: the BenQ
   showed as `Red Hat, Inc. 10"`, 21x12 cm, until the window was dragged to the other panel and
   back, after which it correctly read `BenQ LCD`, 52x29 cm.
2. **A secondary display's window covers the visible frame, not the panel.** On the built-in the
   window sat at y=780 height 949 against a panel spanning 747..1729 — the menu bar strip was
   left uncovered, so that display is not really fullscreen.
3. **The secondary window forwards no input at all** — already known as owed work, but measured
   here as a hard block rather than a gap: with the probe on `Virtual-2`, host motion and scroll
   produced nothing, and the window could not even be dragged. Meanwhile motion *did* reach a
   window on `Virtual-2` from the host cursor moving on the **BenQ**, because the one absolute
   device spans the whole guest desktop — the arrangement coupling, observed.
