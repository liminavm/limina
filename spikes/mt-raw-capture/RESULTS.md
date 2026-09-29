# Results: raw multitouch capture

Measured 2026-09-29 on the dev Mac (M1 Max built-in trackpad, macOS 26.6.2), with
`mtprobe.swift`, arm `baseline`. Only Arm 0 (the settings path) has been run; the
private-API suppression arms have not.

## Arm 0: give macOS four fingers, take three — WORKS, with one catch

Two 60 s runs, identical gesture script (2-finger scroll; 3-finger left/right and
up/down; 4-finger left/right and up/down), gestured over Safari:

| | control: Mission Control/Spaces/App Exposé on "three or four" | arm 0: set to "four" |
|---|---|---|
| 3-finger horizontal | Space switch | **no host action**; cooked horizontal scroll + momentum |
| 3-finger vertical | Mission Control open / close | **no host action**; cooked vertical scroll + momentum |
| 4-finger horizontal | Space switch | Space switch |
| 4-finger vertical | Mission Control open / close | Mission Control open / close |
| 2-finger scroll | cooked scroll | cooked scroll |

- **Freed three-finger contacts are not dead — macOS turns them into ordinary scroll.**
  Every 3-finger swipe produced a `scrollWheel` phase began→ended sequence plus a
  momentum tail in the app under the cursor, and the user saw the Safari page scroll.
  This happened even when all three fingers landed in the same frame, so it is not the
  first two fingers scrolling before the third arrives. Consequence for limina: a forwarded
  3-finger sequence needs the **same cooked-scroll swallow** as the 2-finger one, and the
  window has to include the momentum tail. The last momentum-end event arrived up to ~1 s
  after the final finger lifted (lift 6.518 s, momentum end 7.472 s).
- **Cooked scroll cannot say how many fingers made it.** The discriminator has to be the
  contact count from the touch source (raw frames or `NSTouch`), not the event itself.
- **Claimed system gestures emit no cooked events at all.** In the control run no 3- or
  4-finger swipe produced a single `NSEvent` in the global monitor; the Dock consumes
  them upstream.
- **Multi-finger contacts never moved the host cursor**: drift was 0.0 pt across every 2-,
  3- and 4-finger contact in both runs.
- **The prefs are a live, readable oracle.** `UserDefaults(suiteName:
  "com.apple.AppleMultitouchTrackpad")` (and the `...AppleBluetoothMultitouch.trackpad`
  domain) read the new values right after the user changed them in System Settings, with
  no logout, and the behavior had already changed. Toggling to four fingers writes
  `TrackpadThreeFinger{Horiz,Vert}SwipeGesture = 0` and leaves
  `TrackpadFourFinger{Horiz,Vert}SwipeGesture = 2` in both domains. "Three or four" is
  both at `2`. `TrackpadThreeFingerDrag` and `TrackpadThreeFingerTapGesture` were
  already `0` on this machine.

## Binding and device facts

- `MTDeviceCreateList` → `MTRegisterContactFrameCallback` → `MTDeviceStart(dev, 0)` from
  an unsandboxed command-line binary works. **No TCC prompt** appeared. Caveat: the probe
  was launched from a terminal, so any TCC responsibility would have been attributed to the
  terminal, not the probe; recheck from the app.
- `MemoryLayout<MTTouch>.stride == 96` with the OpenMultitouchSupport layout. Every
  `normalizedPosition` fell inside [0,1] in both runs. Positions, pressures and major
  axes look plausible. The `MTPath_*` accessor cross-check was not done.
- Built-in trackpad: family 108 (0x6c), `MTDeviceIsBuiltIn` = yes,
  `MTDeviceIsOpaqueSurface` = no. `MTDeviceGetSensorSurfaceDimensions` = 12480 × 7680 in
  0.01 mm, i.e. **124.8 × 76.8 mm**. That is the `abs_info.res` input: report positions
  in 0.01 mm and `res` = 100 units/mm. Sensor grid 24 × 18.
- Frame rate while touching: ~124 Hz (391 frames in 3.14 s).
- Contact state sequence seen: 1 StartInRange / 2 HoverInRange → 3 MakeTouch → 4 Touching
  → 5 BreakTouch → 6 LingerInRange / 7 OutOfRange. Pressure 0 on non-touching states.

## Automatic host-action oracle

The probe can see the host act without a human watching:

- `NSWorkspace.activeSpaceDidChangeNotification` fires on a Space switch.
- **Dock-owned on-screen windows** (owner and layer via `CGWindowListCopyWindowInfo`; no
  Screen Recording grant needed) change shape during the transition:
  - Mission Control / App Exposé open: layer-18 and extra layer-20 windows appear, and
    disappear when it closes.
  - Space switch: transient windows at layers -2147483601 and -2147483603 appear, then
    `activeSpaceDidChange` fires.

In the control run this oracle matched every gesture the user performed, which makes it
usable for automated checks of the private-API arms.

## Not yet run

The suppression arms (`parser-off`, `stop`, `power-off`, `gestureconf`, `hidtap`) and
the host-sleep behavior of the MT device handle.
