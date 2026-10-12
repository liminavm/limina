// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Which HID devices report relative pointer motion, and what their reports look like, for the
// guest-controlled pointer: unaccelerated mouse counts read with IOHIDManager instead of
// macOS's accelerated event deltas.
//
//   swiftc -O hidprobe.swift -o hidprobe && ./hidprobe [seconds] [--disassociate]
//
// Prints Input Monitoring's access state, the IOHIDManagerOpen result, every matched device
// with its usages and transport, then one line per input value on GenericDesktop X/Y/Wheel
// and Consumer AC Pan (device, usage, value, report timestamp), and a per-device summary.
// `--disassociate` detaches the cursor from the mouse for the run (as capture does), to see
// whether reports keep coming.
import Foundation
import IOKit.hid
import CoreGraphics

let args = CommandLine.arguments
let seconds = args.count > 1 ? Double(args[1]) ?? 20 : 20
let disassociate = args.contains("--disassociate")
// Print nothing per value or report: a slow callback lets IOHID's queue overflow and drop.
let quiet = args.contains("--quiet")

func prop(_ d: IOHIDDevice, _ k: String) -> Any? {
    IOHIDDeviceGetProperty(d, k as CFString)
}

func describe(_ d: IOHIDDevice) -> String {
    let product = prop(d, kIOHIDProductKey) as? String ?? "?"
    let transport = prop(d, kIOHIDTransportKey) as? String ?? "?"
    let vid = prop(d, kIOHIDVendorIDKey) as? Int ?? 0
    let pid = prop(d, kIOHIDProductIDKey) as? Int ?? 0
    let builtIn = prop(d, "Built-In") as? Bool ?? false
    let page = prop(d, kIOHIDPrimaryUsagePageKey) as? Int ?? 0
    let usage = prop(d, kIOHIDPrimaryUsageKey) as? Int ?? 0
    var pairs: [String] = []
    if let ps = prop(d, kIOHIDDeviceUsagePairsKey) as? [[String: Any]] {
        for p in ps {
            let pg = p[kIOHIDDeviceUsagePageKey] as? Int ?? 0
            let u = p[kIOHIDDeviceUsageKey] as? Int ?? 0
            pairs.append(String(format: "%x:%x", pg, u))
        }
    }
    let touchpad = IOHIDDeviceConformsTo(d, 0x0D, 0x05)
    let location = prop(d, kIOHIDLocationIDKey) as? Int ?? 0
    var entry: UInt64 = 0
    IORegistryEntryGetRegistryEntryID(IOHIDDeviceGetService(d), &entry)
    let maxIn = prop(d, kIOHIDMaxInputReportSizeKey) as? Int ?? 0
    return "\"\(product)\" transport=\(transport) vid=\(String(format: "%04x", vid)) "
        + "pid=\(String(format: "%04x", pid)) builtin=\(builtIn) "
        + "primary=\(String(format: "%x:%x", page, usage)) pairs=[\(pairs.joined(separator: ","))] "
        + "conforms_touchpad=\(touchpad) location=\(String(format: "%x", location)) "
        + "registry=\(String(format: "%llx", entry)) max_input_report=\(maxIn)"
}

let access = IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)
let accessName = [kIOHIDAccessTypeGranted: "granted", kIOHIDAccessTypeDenied: "denied",
                  kIOHIDAccessTypeUnknown: "unknown"][access] ?? "\(access)"
print("input-monitoring access: \(accessName)")

let manager = IOHIDManagerCreate(kCFAllocatorDefault, IOOptionBits(kIOHIDOptionsTypeNone))
let matches: [[String: Int]] = [
    [kIOHIDDeviceUsagePageKey: 0x01, kIOHIDDeviceUsageKey: 0x02],  // Mouse
    [kIOHIDDeviceUsagePageKey: 0x01, kIOHIDDeviceUsageKey: 0x01],  // Pointer
]
IOHIDManagerSetDeviceMatchingMultiple(manager, matches as CFArray)

let t0 = Date()
var names: [Int: String] = [:]
var counts: [String: (n: Int, sum: Int, maxAbs: Int)] = [:]

let valueCb: IOHIDValueCallback = { _, _, _, value in
    let el = IOHIDValueGetElement(value)
    let page = IOHIDElementGetUsagePage(el)
    let usage = IOHIDElementGetUsage(el)
    let tag: String
    switch (page, usage) {
    case (0x01, 0x30): tag = "X"
    case (0x01, 0x31): tag = "Y"
    case (0x01, 0x38): tag = "Wheel"
    case (0x0C, 0x238): tag = "Pan"
    case (0x09, _): tag = "Btn\(usage)"
    default: return
    }
    let v = IOHIDValueGetIntegerValue(value)
    if tag.hasPrefix("X") || tag.hasPrefix("Y") || tag == "Wheel" || tag == "Pan" {
        if v == 0 { return }
    }
    let dev = IOHIDElementGetDevice(el)
    let key = Int(bitPattern: Unmanaged.passUnretained(dev).toOpaque())
    let name = names[key] ?? "?"
    let ts = IOHIDValueGetTimeStamp(value)
    let ms = Date().timeIntervalSince(t0) * 1000
    if !quiet { print(String(format: "%9.1f ms  %@  %@=%d  ts=%llu", ms, name, tag, v, ts)) }
    let k = "\(name) \(tag)"
    var c = counts[k] ?? (0, 0, 0)
    c.n += 1; c.sum += v; c.maxAbs = max(c.maxAbs, abs(v))
    counts[k] = c
}

let matchCb: IOHIDDeviceCallback = { _, _, _, dev in
    let key = Int(bitPattern: Unmanaged.passUnretained(dev).toOpaque())
    let product = IOHIDDeviceGetProperty(dev, kIOHIDProductKey as CFString) as? String ?? "?"
    names[key] = "\(product)#\(names.count)"
    print("device \(names[key]!): \(describe(dev))")
}
var reports: [String: Int] = [:]
let reportCb: IOHIDReportCallback = { _, _, sender, _, reportID, report, length in
    let dev = unsafeBitCast(sender, to: IOHIDDevice.self)
    let key = Int(bitPattern: Unmanaged.passUnretained(dev).toOpaque())
    let name = names[key] ?? "?"
    let bytes = (0..<length).map { String(format: "%02x", report[$0]) }.joined(separator: " ")
    let ms = Date().timeIntervalSince(t0) * 1000
    reports[name, default: 0] += 1
    if !quiet && (reports[name]! <= 40 || reports[name]! % 50 == 0) {
        print(String(format: "%9.1f ms  REPORT %@ id=%d len=%d  %@", ms, name, reportID, length, bytes))
    }
}
IOHIDManagerRegisterInputReportCallback(manager, reportCb, nil)
IOHIDManagerRegisterDeviceMatchingCallback(manager, matchCb, nil)
IOHIDManagerRegisterInputValueCallback(manager, valueCb, nil)
IOHIDManagerScheduleWithRunLoop(manager, CFRunLoopGetMain(), CFRunLoopMode.defaultMode.rawValue)
let r = IOHIDManagerOpen(manager, IOOptionBits(kIOHIDOptionsTypeNone))
print(String(format: "IOHIDManagerOpen: 0x%08x%@", r, r == kIOReturnSuccess ? " (success)" : ""))

if disassociate {
    let e = CGAssociateMouseAndMouseCursorPosition(0)
    print("cursor disassociated: \(e == .success)")
}
print("listening \(seconds) s — move the mouse, then the trackpad, scroll each")
RunLoop.main.run(until: Date().addingTimeInterval(seconds))
if disassociate { CGAssociateMouseAndMouseCursorPosition(1) }
print("== reports per device: \(reports)")
print("== summary (device usage: values, sum, max |v|)")
for (k, c) in counts.sorted(by: { $0.key < $1.key }) {
    print("\(k): n=\(c.n) sum=\(c.sum) max=\(c.maxAbs)")
}
