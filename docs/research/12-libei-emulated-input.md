# libei / EIS — assessed, not adopted

Assessment of libei (freedesktop's Emulated Input library/protocol,
`gitlab.freedesktop.org/libinput/libei`) as an input channel for limina. Grounded in the
upstream README and the ei protocol (`proto/protocol.xml`), read 2026-08-18.

## What it is

A compositor-level channel for *emulated* input — the XTEST replacement for Wayland. `libei`
(client) speaks a binary protocol over a UNIX socket to `libeis` inside the compositor
(mutter ≥ GNOME 45, KWin ≥ Plasma 6); the connection is brokered by the XDG RemoteDesktop
portal (`liboeffis`). Intended users: xdotool-alikes, remote desktop, software KVM
(input-leap). A pure-Rust implementation exists (the `reis` crate), and `limina-agent-session`
already lives in the session, so integration would be mechanically feasible.

## Why it maps onto our problem space

The protocol's coordinate model is exactly the shape the units oracle
(`spikes/pointer-units-oracle/RESULTS.md`) established we need: an EIS sender is given
**per-monitor absolute devices**, each carrying a `region` (x/y offset + size in the desktop
layout), and `motion_absolute` takes **float logical pixels within a region**, interpreted by
the same compositor that advertised the region. On that channel the host/guest rect
disagreement bug class (the measured 0.4585-vs-0.5753 seam) cannot exist: there is no
`0..ABS_MAX` range spread over extents the host must guess, and the regions are a
compositor-authoritative statement of the logical layout.

## Why it is the wrong layer for a VMM's pointer

- **Emulated input is second-class by design.** The protocol's stated benefits — separation,
  distinction, control — are compositor powers *over* the client: it "can filter input or
  discard at will", explicitly including password prompts and the lock screen. For a VM's only
  pointer that is fatal (the user must be able to unlock). virtio-input is *hardware* from the
  guest's view, which is the correct standing for a machine's input.
- **Session-scoped.** No EIS before the session exists: nothing at GDM, the console/VT, an
  installer ISO, or when the agent/compositor is dead. virtio-input works in all of those.
- **Enhanced-tier-only and consent-gated.** Needs a current compositor plus a RemoteDesktop
  portal grant (interactive dialog; restore tokens make it once-per-guest, but it is still
  consent UX for something a machine should simply have). The stock floor keeps virtio-input
  regardless, so both paths would live forever with no code retired.
- It addresses none of the other open input defects (sprite compositing, arrangement relay for
  display purposes, the grab).

The measured relay bug has a cheaper, floor-compatible fix (`zxdg_output_v1` logical size in
the agent's relay) that delivers the same "compositor states its own logical rects" property
without a new channel.

## The design it vindicates

EIS models multi-monitor absolute input as **one absolute device per screen**, never one
device spread over the desktop. The evdev-layer analog — one virtio-input tablet per connector
— is measured viable *for pointing* on mutter (`spikes/per-display-input/RESULTS.md`: EDID-name
+ size binding both fire, ordinary clients receive normal pointer events), with the wheel as
the open wall: scroll cannot ride a tablet-class device and lands at the seat's core pointer in
whole-desktop space. Upstream's convergence on the per-screen model is independent evidence
for that route if the wheel gets an answer; it does not remove the wheel problem.

## Adjacent, noted for completeness

- The companion **InputCapture portal** (pointer barriers for KVM screen-crossing) is the
  guest-side mirror of our host-side grab — not applicable; our capture decision lives on
  macOS.
- Xwayland ≥ 23.2 routes XTEST through ei. Irrelevant to us.
