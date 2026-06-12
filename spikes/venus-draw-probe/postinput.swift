// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Post real HID-level input events (mouse move/click, key taps) at absolute screen
// coordinates — for driving the limina window from scripts. AppleScript "key code" /
// CGEventPost-via-osascript both fail to reach the NSView reliably; this does.
//
//   postinput move <x> <y>
//   postinput click <x> <y>
//   postinput key <keycode>        (e.g. 126 = up arrow)
//
// Build: swiftc -O -o postinput postinput.swift
import Foundation
import CoreGraphics

let args = CommandLine.arguments
func die(_ msg: String) -> Never {
    print("usage: postinput move|click x y | key code  (\(msg))")
    exit(1)
}
guard args.count >= 2 else { die("no command") }

switch args[1] {
case "move", "click":
    guard args.count >= 4, let x = Double(args[2]), let y = Double(args[3]) else { die("bad coords") }
    let pt = CGPoint(x: x, y: y)
    let move = CGEvent(mouseEventSource: nil, mouseType: .mouseMoved,
                       mouseCursorPosition: pt, mouseButton: .left)
    move?.post(tap: .cghidEventTap)
    if args[1] == "click" {
        usleep(50_000)
        let down = CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown,
                           mouseCursorPosition: pt, mouseButton: .left)
        down?.post(tap: .cghidEventTap)
        usleep(60_000)
        let up = CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp,
                         mouseCursorPosition: pt, mouseButton: .left)
        up?.post(tap: .cghidEventTap)
    }
case "key":
    guard args.count >= 3, let code = UInt16(args[2]) else { die("bad keycode") }
    let down = CGEvent(keyboardEventSource: nil, virtualKey: CGKeyCode(code), keyDown: true)
    down?.post(tap: .cghidEventTap)
    usleep(60_000)
    let up = CGEvent(keyboardEventSource: nil, virtualKey: CGKeyCode(code), keyDown: false)
    up?.post(tap: .cghidEventTap)
default:
    die("unknown command \(args[1])")
}
usleep(100_000)
