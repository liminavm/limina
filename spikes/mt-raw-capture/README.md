# Spike: raw multitouch capture and gesture suppression

**Status: PLANNED, not run.** Design context and the hypothesis list:
`docs/design/trackpad-gestures.md` §Alternative: raw multitouch capture.

## The question

Can a userland app read the trackpad's raw contact stream *and* stop macOS from
recognizing gestures on those same contacts? The reading half is established. The
suppression half decides whether limina's contact-ownership rule ("3+ fingers are the
host's forever") can be lifted under pointer capture.

## Vehicle

One self-contained Swift binary, `mtprobe.swift`, built outside the limina workspace
(`swiftc mtprobe.swift -o mtprobe`) so it never touches the app's build graph. It
`dlopen`s `/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport`
and takes a lever name on the command line, so every arm is the same binary with the same
observation loop and only the lever differs.

Declarations come from `~/Projects/OpenMultitouchSupport`
(`Framework/OpenMultitouchSupportXCF/OpenMTInternal.h`, MIT, maintained against current
macOS) — the 96-byte `MTTouch`, the named state enum, the device-property calls. Prefer
the **accessor** APIs (`MTRegisterPathCallbackWithRefcon` + `MTPath_getPosition` /
`getForce` / `getVelocity` / `isTouching` / `wasRejected`) where they suffice; they cannot
be shifted by a layout change. The probe should print `MTDeviceGetSensorSurfaceDimensions`,
`GetGUID`, `GetFamilyID` and `IsOpaqueSurface` once at startup — that is the `abs_info.res`
input for the guest device, measured rather than guessed.

A cheap correctness check while there: with contacts down, dump both the accessor values
and the struct-cast values for the same frame. They must agree. If they do, the header is
right for this macOS; if not, the accessors win and the header needs re-deriving.

Each run prints, per frame: contact count, per-contact position/force, and — from a
parallel `NSEvent` global monitor — whether a cooked `scrollWheel`/`magnify`/gesture
event arrived for the same motion. That pairing is the whole measurement: raw in, cooked
out, one timeline.

## Arms

| arm | lever | reading it |
|---|---|---|
| `baseline` | none | raw frames arrive; cooked events arrive; Mission Control fires |
| `parser-off` | `MTDeviceSetParserEnabled(dev, false)` | do raw frames survive? do cooked events stop — for us only, or system-wide? |
| `stop` | `MTDeviceStop(dev)` | do raw frames keep flowing to our client, or is the device simply off? |
| `power-off` | `MTDevicePowerSetEnabled(dev, false)` | same question, harder lever |
| `gestureconf` | `_mthid_*GestureConfiguration` | does it reach the System-Settings-equivalent behavior, and is it global + persistent? |
| `hidtap` | `CGEventTap` at `.cghidEventTap` consuming gesture event types | does an HID-location tap sit upstream of WindowServer's recognizer, where a session tap does not? |

### Arm 0: the settings path (run this first, it may be enough)

Before any private-API arm, set `TrackpadThreeFingerHorizSwipeGesture` and
`TrackpadThreeFingerVertSwipeGesture` to 0 in System Settings (leaving the four-finger
ones on) and re-run `baseline`. Measure: do 3-finger contacts now arrive with **no**
cooked event and no host action? If yes, guest 3-finger gestures need no suppression at
all for a user willing to give macOS four fingers, and the private-API arms become an
out-of-the-box nicety rather than a prerequisite. Also confirm nothing else claims three
fingers (`TrackpadThreeFingerDrag`, `TrackpadThreeFingerTapGesture`, and "swipe between
pages" set to two-or-three).

## What each arm must report

1. **Do raw frames still arrive** with the lever engaged?
2. **Does the 3-finger Mission Control swipe still fire?** This is the verdict question
   and only a human eye answers it — the probe cannot see Mission Control.
3. **Does 2-finger scroll still reach other apps** (scroll a Safari window during the
   run)? Distinguishes "recognizer off" from "trackpad off".
4. **Is the effect per-client or device-global?** Run two probe instances; engage the
   lever in one only.
5. **Does it survive** sleep/wake, display change, and the lever's own process crashing —
   i.e. can the host's trackpad be left broken? Any lever that can leak a broken trackpad
   past our process needs a restore path before it ships.
6. **Which TCC prompt appears, if any.** `OpenMultitouchSupport` needs only an unsandboxed
   app and ships empty entitlements, so possibly none — confirm, and check whether an
   ad-hoc-signed build is refused.
7. **Does the device survive host sleep?** `OpenMultitouchSupport` stops and recreates it
   around `NSWorkspaceWillSleep`/`DidWake`. Sleep the host mid-run and see what the handle
   does — this determines whether the MT device hangs off limina's existing host-sleep
   seam.

## Cost of running it

The probe grabs the live trackpad on the dev Mac. Arms `stop`, `power-off` and
`gestureconf` can leave the machine without a working trackpad until the process exits or
the setting is restored — have a mouse or Screen Sharing reachable before starting, and
never run these while a long unattended job needs the machine.
