// mtprobe — raw multitouch vs cooked gesture timeline, one lever per run.
//
//   swiftc -O mtprobe.swift -o mtprobe
//   ./mtprobe [--arm baseline] [--seconds 60] > run.log
//
// Reads the trackpad's raw contact frames through the private
// MultitouchSupport.framework (dlopen/dlsym, nothing linked) and, on the same
// clock, the cooked events macOS posts to other apps (NSEvent global monitors)
// plus Space switches (the one host gesture action observable from userland).
// Mission Control / App Exposé are consumed by the Dock and never reach an
// event monitor — a human eye answers those.
//
// Only the `baseline` arm (no lever) exists so far; see README.md for the rest.

import AppKit
import Foundation

// MARK: - Private API binding

// Layout from OpenMultitouchSupport's OpenMTInternal.h (MIT); 96-byte stride.
struct MTPoint { var x: Float; var y: Float }
struct MTVector { var position: MTPoint; var velocity: MTPoint }
struct MTTouch {
    var frame: Int32
    var timestamp: Double
    var identifier: Int32
    var state: Int32
    var fingerId: Int32
    var handId: Int32
    var normalizedPosition: MTVector
    var total: Float
    var pressure: Float
    var angle: Float
    var majorAxis: Float
    var minorAxis: Float
    var absolutePosition: MTVector
    var field14: Int32
    var field15: Int32
    var density: Float
}

typealias MTDeviceRef = UnsafeMutableRawPointer
// The touch array is untyped here: a Swift struct is not C-representable.
typealias FrameCallback = @convention(c) (MTDeviceRef?, UnsafeMutableRawPointer?, Int32, Double, Int32) -> Void

let fwPath = "/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport"
guard let fw = dlopen(fwPath, RTLD_NOW) else {
    fatalError("dlopen: \(String(cString: dlerror()))")
}

func sym<T>(_ name: String, _: T.Type) -> T? {
    guard let p = dlsym(fw, name) else { return nil }
    return unsafeBitCast(p, to: T.self)
}

func need<T>(_ name: String, _ t: T.Type) -> T {
    guard let f = sym(name, t) else { fatalError("missing symbol \(name)") }
    return f
}

let MTDeviceCreateList = need("MTDeviceCreateList", (@convention(c) () -> Unmanaged<CFArray>?).self)
let MTRegisterContactFrameCallback = need("MTRegisterContactFrameCallback",
                                          (@convention(c) (MTDeviceRef, FrameCallback) -> Void).self)
let MTDeviceStart = need("MTDeviceStart", (@convention(c) (MTDeviceRef, Int32) -> Int32).self)
let MTDeviceStop = need("MTDeviceStop", (@convention(c) (MTDeviceRef) -> Int32).self)
let MTDeviceIsBuiltIn = sym("MTDeviceIsBuiltIn", (@convention(c) (MTDeviceRef) -> Bool).self)
let MTDeviceIsOpaqueSurface = sym("MTDeviceIsOpaqueSurface", (@convention(c) (MTDeviceRef) -> Bool).self)
let MTDeviceGetSensorSurfaceDimensions = sym("MTDeviceGetSensorSurfaceDimensions",
    (@convention(c) (MTDeviceRef, UnsafeMutablePointer<Int32>, UnsafeMutablePointer<Int32>) -> Int32).self)
let MTDeviceGetSensorDimensions = sym("MTDeviceGetSensorDimensions",
    (@convention(c) (MTDeviceRef, UnsafeMutablePointer<Int32>, UnsafeMutablePointer<Int32>) -> Int32).self)
let MTDeviceGetFamilyID = sym("MTDeviceGetFamilyID",
    (@convention(c) (MTDeviceRef, UnsafeMutablePointer<Int32>) -> Int32).self)
let MTDeviceGetDeviceID = sym("MTDeviceGetDeviceID",
    (@convention(c) (MTDeviceRef, UnsafeMutablePointer<UInt64>) -> Int32).self)

// MARK: - Logging (callbacks arrive off-main; one lock serializes the timeline)

let t0 = ProcessInfo.processInfo.systemUptime
let logLock = NSLock()
func log(_ tag: String, _ msg: String) {
    let t = ProcessInfo.processInfo.systemUptime - t0
    logLock.lock()
    print(String(format: "%9.3f %-6@ ", t, tag) + msg)
    fflush(stdout)
    logLock.unlock()
}

// MARK: - Arguments

var arm = "baseline"
var seconds = 60.0
var argv = CommandLine.arguments.dropFirst().makeIterator()
while let a = argv.next() {
    switch a {
    case "--arm": arm = argv.next() ?? arm
    case "--seconds": seconds = Double(argv.next() ?? "") ?? seconds
    default: fatalError("unknown argument \(a)")
    }
}
guard arm == "baseline" else { fatalError("arm \(arm) not implemented; only baseline") }

// MARK: - Raw frames

// Per-device state, touched only from that device's callback thread.
var lastCount: [UInt: Int32] = [:]
var lastSample: [UInt: Double] = [:]
var outOfRange = 0
var cursorAtTouchdown = NSPoint.zero

func describe(_ touches: UnsafeMutablePointer<MTTouch>, _ n: Int32) -> String {
    (0..<Int(n)).map { i in
        let c = touches[i]
        if c.normalizedPosition.position.x < 0 || c.normalizedPosition.position.x > 1
            || c.normalizedPosition.position.y < 0 || c.normalizedPosition.position.y > 1 {
            outOfRange += 1
        }
        return String(format: "[id%d st%d f%d (%.3f,%.3f) p%.1f maj%.2f]",
                      c.identifier, c.state, c.fingerId,
                      c.normalizedPosition.position.x, c.normalizedPosition.position.y,
                      c.pressure, c.majorAxis)
    }.joined(separator: " ")
}

let frameCallback: FrameCallback = { dev, touches, n, ts, frame in
    let key = UInt(bitPattern: dev)
    let prev = lastCount[key] ?? 0
    let body = touches.map { describe($0.assumingMemoryBound(to: MTTouch.self), n) } ?? ""
    let cursor = NSEvent.mouseLocation
    if n != prev {
        lastCount[key] = n
        if prev == 0 { cursorAtTouchdown = cursor }
        let moved = hypot(cursor.x - cursorAtTouchdown.x, cursor.y - cursorAtTouchdown.y)
        log("RAW", String(format: "dev%lx n %d->%d frame %d cursor(%.0f,%.0f) drift %.1f ",
                          key & 0xffff, prev, n, frame, cursor.x, cursor.y, moved) + body)
        lastSample[key] = ts
    } else if n > 0 && ts - (lastSample[key] ?? 0) >= 0.1 {
        lastSample[key] = ts
        let moved = hypot(cursor.x - cursorAtTouchdown.x, cursor.y - cursorAtTouchdown.y)
        log("raw", String(format: "dev%lx n %d cursor drift %.1f ", key & 0xffff, n, moved) + body)
    }
}

// MARK: - Startup

let app = NSApplication.shared
app.setActivationPolicy(.accessory)

log("INFO", "arm \(arm), \(seconds) s, MTTouch stride \(MemoryLayout<MTTouch>.stride) (expect 96)")
precondition(MemoryLayout<MTTouch>.stride == 96, "MTTouch layout wrong for this build")

for domain in ["com.apple.AppleMultitouchTrackpad", "com.apple.driver.AppleBluetoothMultitouch.trackpad"] {
    let d = UserDefaults(suiteName: domain)
    let keys = ["TrackpadThreeFingerHorizSwipeGesture", "TrackpadThreeFingerVertSwipeGesture",
                "TrackpadFourFingerHorizSwipeGesture", "TrackpadFourFingerVertSwipeGesture",
                "TrackpadThreeFingerDrag", "TrackpadThreeFingerTapGesture"]
    let vals = keys.map { k in "\(k.replacingOccurrences(of: "Trackpad", with: ""))=\(d?.object(forKey: k).map { "\($0)" } ?? "-")" }
    log("PREFS", "\(domain): " + vals.joined(separator: " "))
}

var devices: [MTDeviceRef] = []
guard let list = MTDeviceCreateList()?.takeRetainedValue() else { fatalError("MTDeviceCreateList returned NULL") }
for i in 0..<CFArrayGetCount(list) {
    let dev = UnsafeMutableRawPointer(mutating: CFArrayGetValueAtIndex(list, i)!)
    var sw: Int32 = 0, sh: Int32 = 0, rows: Int32 = 0, cols: Int32 = 0, fam: Int32 = 0
    var id: UInt64 = 0
    _ = MTDeviceGetSensorSurfaceDimensions?(dev, &sw, &sh)
    _ = MTDeviceGetSensorDimensions?(dev, &rows, &cols)
    _ = MTDeviceGetFamilyID?(dev, &fam)
    _ = MTDeviceGetDeviceID?(dev, &id)
    log("DEV", String(format: "dev%lx builtin=%@ opaque=%@ family=%d id=0x%llx surface=%dx%d (0.01 mm) sensor=%dx%d",
                      UInt(bitPattern: dev) & 0xffff,
                      MTDeviceIsBuiltIn.map { $0(dev) ? "yes" : "no" } ?? "?",
                      MTDeviceIsOpaqueSurface.map { $0(dev) ? "yes" : "no" } ?? "?",
                      fam, id, sw, sh, rows, cols))
    MTRegisterContactFrameCallback(dev, frameCallback)
    let rc = MTDeviceStart(dev, 0)
    log("DEV", String(format: "dev%lx start rc=%d", UInt(bitPattern: dev) & 0xffff, rc))
    devices.append(dev)
}

// MARK: - Cooked events and host actions

let cookedMask: NSEvent.EventTypeMask = [.scrollWheel, .magnify, .swipe, .rotate,
                                         .beginGesture, .endGesture, .gesture, .smartMagnify]
var scrollRun = 0
_ = NSEvent.addGlobalMonitorForEvents(matching: cookedMask) { e in
    switch e.type {
    case .scrollWheel:
        // Log phase edges and a count, not every event.
        scrollRun += 1
        if !e.phase.isEmpty && e.phase != .changed || !e.momentumPhase.isEmpty && e.momentumPhase != .changed {
            log("COOKED", String(format: "scroll phase=%lu momentum=%lu dy=%.1f dx=%.1f precise=%@ (#%d)",
                                 e.phase.rawValue, e.momentumPhase.rawValue,
                                 e.scrollingDeltaY, e.scrollingDeltaX,
                                 e.hasPreciseScrollingDeltas ? "y" : "n", scrollRun))
        }
    case .magnify:
        log("COOKED", String(format: "magnify phase=%lu m=%.3f", e.phase.rawValue, e.magnification))
    case .swipe:
        log("COOKED", String(format: "swipe dx=%.1f dy=%.1f", e.deltaX, e.deltaY))
    case .rotate:
        log("COOKED", String(format: "rotate phase=%lu r=%.2f", e.phase.rawValue, e.rotation))
    default:
        log("COOKED", "type=\(e.type.rawValue)")
    }
}

NSWorkspace.shared.notificationCenter.addObserver(
    forName: NSWorkspace.activeSpaceDidChangeNotification, object: nil, queue: .main
) { _ in log("HOST", "active Space changed") }

// Dock windows appear/disappear when Mission Control / App Exposé open. A
// heuristic only (owner + layer need no Screen Recording grant); the human
// confirms.
var lastDock = ""
Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in
    let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
    let dock = info.filter { ($0[kCGWindowOwnerName as String] as? String) == "Dock" }
        .map { "L\($0[kCGWindowLayer as String] ?? "?")" }.sorted().joined(separator: ",")
    if dock != lastDock {
        log("HOST", "Dock windows: [\(dock)]")
        lastDock = dock
    }
}

Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { _ in
    for dev in devices { _ = MTDeviceStop(dev) }
    log("INFO", "done; contacts outside [0,1]: \(outOfRange)")
    exit(0)
}

log("INFO", "ready — gesture now")
app.run()
