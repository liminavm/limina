// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Does macOS apply its Modifier Keys remapping BEFORE an app sees the event?
//
// The whole "read the host config and account for it" design rests on the answer. If NSEvent
// reports the POST-remap identity, limina's own swap composes with the host's and we must undo
// it to reach the physical key; if it reports the physical key, the host config is none of our
// business and there is nothing to read.
//
// Build+run:  swiftc -O probe.swift -o probe && ./probe
// Focus the window, press the modifiers it asks for, read the lines, press Escape.
import AppKit

// Unbuffered: the probe is normally read from a redirected log while it is still running.
setvbuf(stdout, nil, _IONBF, 0)

let usagePage: UInt64 = 0x7_0000_0000
func usageName(_ v: UInt64) -> String {
    switch v &- usagePage {
    case 0x39: return "CapsLock"
    case 0xE0: return "LeftControl"
    case 0xE1: return "LeftShift"
    case 0xE2: return "LeftOption"
    case 0xE3: return "LeftCommand"
    case 0xE4: return "RightControl"
    case 0xE5: return "RightShift"
    case 0xE6: return "RightOption"
    case 0xE7: return "RightCommand"
    default: return String(format: "usage 0x%llX", v &- usagePage)
    }
}

// What the host has configured, straight out of the ByHost global domain.
print("=== host modifier mapping (com.apple.keyboard.modifiermapping.*) ===")
var sawMapping = false
if let all = CFPreferencesCopyMultiple(nil, kCFPreferencesAnyApplication,
                                       kCFPreferencesCurrentUser, kCFPreferencesCurrentHost)
        as? [String: Any] {
    for (key, value) in all.sorted(by: { $0.key < $1.key })
    where key.hasPrefix("com.apple.keyboard.modifiermapping.") {
        sawMapping = true
        let device = String(key.dropFirst("com.apple.keyboard.modifiermapping.".count))
        print("  keyboard \(device):")
        for entry in (value as? [[String: Any]] ?? []) {
            let src = (entry["HIDKeyboardModifierMappingSrc"] as? NSNumber)?.uint64Value ?? 0
            let dst = (entry["HIDKeyboardModifierMappingDst"] as? NSNumber)?.uint64Value ?? 0
            print("    \(usageName(src)) -> \(usageName(dst))")
        }
    }
}
if !sawMapping { print("  (none configured — every key is itself)") }

func keycodeName(_ kc: UInt16) -> String {
    switch kc {
    case 0x37: return "kVK_Command(left)"
    case 0x36: return "kVK_RightCommand"
    case 0x38: return "kVK_Shift(left)"
    case 0x3C: return "kVK_RightShift"
    case 0x3A: return "kVK_Option(left)"
    case 0x3D: return "kVK_RightOption"
    case 0x3B: return "kVK_Control(left)"
    case 0x3E: return "kVK_RightControl"
    case 0x39: return "kVK_CapsLock"
    default: return "?"
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 560, height: 140),
                      styleMask: [.titled, .closable], backing: .buffered, defer: false)
window.title = "limina modifier probe"
let label = NSTextField(labelWithString:
    "Press LEFT CONTROL, then LEFT COMMAND, then LEFT OPTION.\nRead the terminal. Escape to quit.")
label.frame = NSRect(x: 20, y: 20, width: 520, height: 100)
window.contentView?.addSubview(label)
window.center()
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)

print("\n=== press modifiers now (Escape quits) ===")
NSEvent.addLocalMonitorForEvents(matching: [.flagsChanged, .keyDown]) { event in
    if event.type == .keyDown {
        if event.keyCode == 53 { NSApplication.shared.terminate(nil) }
        return event
    }
    print(String(format: "flagsChanged keyCode=0x%02X %-18@ flags=0x%08X",
                 event.keyCode, keycodeName(event.keyCode) as NSString,
                 UInt32(event.modifierFlags.rawValue & 0xFFFF_FFFF)))
    return event
}
app.run()
