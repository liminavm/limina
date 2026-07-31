// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// fnprobe — dump every keyboard-ish event a SESSION CGEventTap can see, so we can tell
// exactly which physical press lands in which event class on this Mac.
//
// The question it answers: on a MacBook top row, does a given press arrive as an ordinary
// keyDown with a virtual keycode (which limina already forwards), or as an NX_SYSDEFINED
// aux-key event (CGEventType 14, subtype 8) whose key is packed into NSEvent.data1 (which
// limina did NOT see before the fn-key buckets landed)? The fn translation happens in the
// HID layer BELOW a session tap, so by the time we see the event the decision is already
// made and we can only observe it — hence this probe.
//
// LISTEN-ONLY: it never consumes anything, so your keyboard keeps working normally while
// it runs. Ctrl-C to stop.
//
// Build + run:
//   swiftc -O spikes/fn-key-probe/fnprobe.swift -o spikes/fn-key-probe/fnprobe
//   spikes/fn-key-probe/fnprobe | tee spikes/fn-key-probe/output.txt
//
// A key that produces NO line at all is a real result (it never reaches a session tap) — so
// stdout is line-buffered below, or a redirect + Ctrl-C would fake that result by losing the
// buffer.
//
// Needs Accessibility for the app that RUNS it (Terminal/iTerm/…): System Settings →
// Privacy & Security → Accessibility. Without it CGEventTapCreate returns nil and the
// probe exits saying so.

import AppKit
import Foundation

// Line-buffer stdout. Swift's `print` is BLOCK-buffered when stdout is a file rather than a
// terminal, so `fnprobe > out.txt` + Ctrl-C throws away everything still in the 4 KiB buffer —
// producing an empty file that reads exactly like "the keys generated no events", which is the
// single most important thing this probe can report. Cost us one run (2026-07-31).
setvbuf(stdout, nil, _IOLBF, 0)

// IOKit/hidsystem/ev_keymap.h — NX_KEYTYPE_*. Read from the SDK header, not from memory.
let nxKeyNames: [Int: String] = [
    0: "SOUND_UP", 1: "SOUND_DOWN", 2: "BRIGHTNESS_UP", 3: "BRIGHTNESS_DOWN",
    4: "CAPS_LOCK", 5: "HELP", 6: "POWER", 7: "MUTE", 8: "UP_ARROW", 9: "DOWN_ARROW",
    10: "NUM_LOCK", 11: "CONTRAST_UP", 12: "CONTRAST_DOWN", 13: "LAUNCH_PANEL",
    14: "EJECT", 15: "VIDMIRROR", 16: "PLAY", 17: "NEXT", 18: "PREVIOUS", 19: "FAST",
    20: "REWIND", 21: "ILLUMINATION_UP", 22: "ILLUMINATION_DOWN",
    23: "ILLUMINATION_TOGGLE", 25: "MENU",
]

func flagsDescription(_ flags: CGEventFlags) -> String {
    var parts: [String] = []
    if flags.contains(.maskShift) { parts.append("shift") }
    if flags.contains(.maskControl) { parts.append("ctrl") }
    if flags.contains(.maskAlternate) { parts.append("opt") }
    if flags.contains(.maskCommand) { parts.append("cmd") }
    if flags.contains(.maskAlphaShift) { parts.append("caps") }
    // The one we care most about here: kCGEventFlagMaskSecondaryFn (1 << 23). It rides along
    // on real F-key and arrow keyDowns; it is NOT how fn+top-row brightness/media arrive.
    if flags.contains(.maskSecondaryFn) { parts.append("FN") }
    if flags.contains(.maskNumericPad) { parts.append("numpad") }
    return parts.isEmpty ? "-" : parts.joined(separator: "+")
}

let callback: CGEventTapCallBack = { _, type, event, _ in
    switch type.rawValue {
    case 10, 11:  // keyDown / keyUp
        let kc = event.getIntegerValueField(.keyboardEventKeycode)
        let rep = event.getIntegerValueField(.keyboardEventAutorepeat)
        let kind = type.rawValue == 10 ? "keyDown" : "keyUp  "
        print(
            String(
                format: "%@  keycode=0x%02X (%3d)  flags=[%@]%@", kind, kc, kc,
                flagsDescription(event.flags), rep != 0 ? "  (autorepeat)" : ""))
    case 12:  // flagsChanged
        let kc = event.getIntegerValueField(.keyboardEventKeycode)
        print(
            String(
                format: "flagsChg keycode=0x%02X (%3d)  flags=[%@]", kc, kc,
                flagsDescription(event.flags)))
    case 14:  // NX_SYSDEFINED — the aux-key class limina was missing
        guard let ns = NSEvent(cgEvent: event) else { break }
        // subtype 8 = NX_SUBTYPE_AUX_CONTROL_BUTTONS; anything else here is window-server
        // bookkeeping (screen changed, app activated, …) and not a key at all.
        guard ns.subtype.rawValue == 8 else { break }
        let d1 = ns.data1
        let nxKey = (d1 & 0xFFFF_0000) >> 16
        let state = (d1 & 0xFF00) >> 8  // 0x0A = down, 0x0B = up
        let repeating = (d1 & 0x1) != 0
        let name = nxKeyNames[nxKey] ?? "UNKNOWN"
        // Raw data1 is printed too: it's the wire value the decoder parses, so an observed
        // line can be pasted straight into a test fixture instead of hand-deriving one.
        print(
            String(
                format: "SYSDEF   NX=%2d %-20@ %@  data1=0x%08lX  flags=[%@]%@", nxKey,
                name as NSString,
                state == 0x0A ? "down" : (state == 0x0B ? "up  " : "state=\(state)"),
                d1, flagsDescription(event.flags), repeating ? "  (repeat)" : ""))
    default:
        break
    }
    return Unmanaged.passUnretained(event)  // listen-only: never consume
}

let mask: CGEventMask =
    (1 << 10) | (1 << 11) | (1 << 12) | (1 << 14)  // keyDown, keyUp, flagsChanged, NX_SYSDEFINED

guard
    let tap = CGEvent.tapCreate(
        tap: .cgSessionEventTap, place: .headInsertEventTap,
        options: .listenOnly,  // never consumes — your keyboard behaves normally
        eventsOfInterest: mask, callback: callback, userInfo: nil)
else {
    FileHandle.standardError.write(
        Data(
            """
            fnprobe: CGEventTapCreate returned nil.
            Grant Accessibility to the app running this (Terminal/iTerm/…):
              System Settings → Privacy & Security → Accessibility

            """.utf8))
    exit(1)
}

let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .commonModes)
CGEvent.tapEnable(tap: tap, enable: true)

print(
    """
    fnprobe: listening (listen-only, nothing is consumed). Ctrl-C to stop.

    Press, in order, and watch which class each lands in:
      1. F1 and F2 BARE
      2. F1 and F2 with fn HELD
      3. the volume and mute keys (bare, then with fn)
      4. play/pause, next, previous
      5. fn ALONE (tap and release)

    keyDown/keyUp with a keycode  -> limina already forwards this (keymap.rs function row)
    SYSDEF NX=…                   -> the aux class; routed by bucket (media/brightness/volume)

    """)
CFRunLoopRun()
