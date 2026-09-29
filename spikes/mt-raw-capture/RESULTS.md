# Results: raw multitouch capture

Measured 2026-09-29 on the dev Mac (M1 Max built-in trackpad, macOS 26.6.2), with
`mtprobe.swift`, launched from a terminal. **Verdict: an HID-level event tap suppresses
the host's 3- and 4-finger swipes and leaves pointer, scroll, clicks and haptics alone;
it is the lever. Filtered on the live contact count, it takes three fingers and leaves
four to macOS, with the default settings.** The private-API parser lever works too, but it is unusable.

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

## Suppression arms

Every lever run lasts 45 s. The lever engages at 5 s and releases at 40 s. The gestures
under it were a one-finger move, a two-finger scroll, 3-finger swipes and 4-finger swipes,
plus clicks where noted. The verdicts come from the automatic oracle, and the user
confirmed each one by eye.

### `hidtap` — WORKS, and is the lever

`CGEventTapCreate(kCGHIDEventTap, kCGHeadInsertEventTap, kCGEventTapOptionDefault, …)` over
types 18, 19, 20, 29, 30, 31, 32, returning NULL for those while engaged.

- **macOS on "four fingers":** the 4-finger horizontal and vertical swipes did nothing (no
  Space change, no Dock window change). The pointer moved and 2-finger scroll worked. The
  3-finger swipes still became cooked scroll, because the tap lets scroll through.
- **macOS on "three or four fingers"** (the out-of-the-box setting): six 3-finger swipes
  and two 4-finger swipes did nothing. No cooked scroll appeared either, because the host
  still claims those counts and its gesture events were swallowed. A 3-finger swipe done
  just *before* the tap engaged started a Space transition, which is the control.
- **Physical click, force click and haptics** worked normally under the tap (user).
- **Type 29 (`NSEventTypeGesture`) streams continuously** under any contact, one finger
  included: ~2960 swallowed in ~31 s, ~94/s. Type 30 (magnify) appeared only around the
  4-finger swipes. Which types are load-bearing for the suppression is not isolated.
- **Tap creation succeeded** (Accessibility, attributed to the terminal). The tap dies
  with the process, so nothing can leak.
- Probe bug fixed after the first run: disabling the tap delivers
  `tapDisabledByUserInput` to the callback, and the callback re-enabled the tap. Clear the
  handle before disabling it.

### `hidtap-3` — suppress three fingers, leave four to macOS: WORKS

The same tap, installed for the whole lever window, swallows gesture events only while the
raw stream shows exactly three contacts *and* the touch sequence has never reached four.
The count is read per event from the frame callback, so no tap engage has to race the
recognizer. Setting: "three or four fingers" (the default).

- 3-finger left/right/up/down: inert.
- Normally landed 4-finger swipes: Space switch ×2, Mission Control open/close. All
  worked.
- Deliberately slow 4-finger landings (0.2–1.5 s at fewer than four contacts): 4 of 7
  clean.
  - One did nothing, and one became a horizontal cooked scroll. The user attributes these
    to their own input: an accidental click, and moving the first two fingers before the
    rest landed (a scroll macOS would have started anyway).
  - One started a Space transition that did not complete.
  - User verdict: "putting the fingers normally - even if a bit slow - never felt like I
    was struggling to do the 4 finger gesture".

### `hidwatch` — a remote trackpad over Universal Control

A listen-only HID tap over every event type, logging each event's type and
`kCGEventSourceUnixProcessID`. For 60 s the user drove this Mac from the *other*
Mac through Universal Control, using two remote devices: a Bluetooth Magic Trackpad
paired with the other Mac (move, click, two-finger scroll, pinch, 3-finger swipes), then
the other Mac's built-in trackpad (4-finger swipes). A two-finger scroll on the local
trackpad followed, for comparison. Both remote devices behaved the same.

- **The remote trackpad produces no raw contacts here.** MultitouchSupport enumerates
  local devices only. The run's only raw frames were the local comparison scroll.
- **Its input arrives already recognized**, at this Mac's HID tap: gesture (29), magnify
  (30), scroll (22), mouse moved/dragged/down/up (5, 6, 1, 2) and system-defined (14)
  events. **This Mac's Dock acted on them**: six Space switches, plus several appearances
  of the full-screen Dock overlay (layer 18) around the pinch and the vertical swipes. The
  other Mac did nothing (user).
- **Source pid does not tell remote from local.** Remote and local events alike carry
  pid 0. What does tell them apart: gesture events flowing while the local raw stream
  shows no contacts.
- Consequences for limina:
  - The guest MT touchpad cannot be fed from a remote trackpad, because there are no
    contacts. That input stays on the tablet + cooked hi-res scroll path.
  - A tap filtering on the local raw count (`hidtap-3`) never swallows remote gestures,
    so they stay the host's. Remote input degrades gracefully rather than breaking.

### `hidtap` with remote trackpads — remote gestures are suppressible, and carry their finger count

The swallowing `hidtap` arm (all gesture types, 5–55 s). Each gesture (type 29) event's
`NSEvent(cgEvent:).allTouches().count` was logged against the local raw count.

- Local 3-finger swipes, then the remote Magic Trackpad's 3-finger swipes and pinch, then
  the other Mac's trackpad's 4-finger swipes. **Nothing acted on this Mac** (oracle and
  user). The swallowing tap covers Universal Control input as well.
- **`allTouches()` reports the finger count for forwarded events too.**
  - Local: raw 3 ↔ AppKit 3.
  - Remote, with raw 0 throughout: AppKit 3 during the Magic Trackpad's 3-finger swipes,
    4 during the other Mac's 4-finger swipes, and 2 during a remote two-finger scroll.
  - Lower counts (0–2) interleave while fingers land and lift, so a filter keyed on it
    needs the per-sequence peak, as `hidtap-3` does with the raw count.
- The local calibration had no 4-finger swipe (user forgot), so AppKit 4 ↔ raw 4 is
  unconfirmed locally; the remote 4 stands on its own.
- Consequence: a count-keyed selective tap can key on `allTouches()` instead of the raw
  stream, and then covers remote trackpads too. Not yet run as a selective arm.

### `parser-off` — suppresses everything, leaks, rejected

`MTDeviceSetParserEnabled(dev, Bool) -> OSStatus` and `MTDeviceGetParserEnabled(dev,
Bool*)`: the signatures were read off the disassembly (driver requests 0x11/0x12 through
`MTDeviceIssueDriverRequest`, the bool stored as one byte).

- While engaged: raw frames kept arriving (1–5 contacts, all phases), and there were no
  cooked events, no Space change and no Mission Control. The host cursor never moved from
  a trackpad contact (user: "dead"). Clicks did nothing in macOS, and there was no haptic
  feedback.
- **The state is device-global kernel state and survives the process.** After the probe
  was killed with SIGALRM (no cleanup) under the lever, a fresh process read
  `parser=false`, and the trackpad stayed dead until `--arm restore` set it back.
- Raw pressure during the lever: ~20–50 resting, ~300–400 on a physical click, ~650 on a
  force click. Clicks are visible in the raw stream even when macOS ignores them.

### `power-off` — unsupported

`MTDevicePowerSetEnabled`/`GetEnabled` return `0xE00002C7` (`kIOReturnUnsupported`) on this
trackpad. `PowerSetEnabled(dev, b)` is a thin wrapper over `PowerSetState(dev, b ? 2 : 0)`.

### Not run

`stop` and `gestureconf` are moot once the HID tap works; `gestureconf` would also be
global persistent state. Also not run: the host-sleep behavior of the MT device handle, the
`MTPath_*` accessor cross-check, and TCC attribution from inside the app rather than a
terminal.
