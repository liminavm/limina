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
// Arms: baseline, parser-off, stop, power-off, hidtap, restore (see README.md).
// A lever engages 5 s after start and releases 5 s before the end, and on
// SIGINT/SIGTERM. The parser and power levers are kernel-driver requests, so
// a crash could leave them engaged: `--arm restore` forces both back on.

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
// Signatures read off the disassembly (macOS 26.6.2): both send a driver
// request (ids 0x11/0x12) through MTDeviceIssueDriverRequest; the bool is
// stored as one byte. PowerSetEnabled maps true/false onto PowerSetState(2/0).
let MTDeviceSetParserEnabled = need("MTDeviceSetParserEnabled", (@convention(c) (MTDeviceRef, Bool) -> Int32).self)
let MTDeviceGetParserEnabled = need("MTDeviceGetParserEnabled",
                                    (@convention(c) (MTDeviceRef, UnsafeMutablePointer<Bool>) -> Int32).self)
let MTDevicePowerSetEnabled = need("MTDevicePowerSetEnabled", (@convention(c) (MTDeviceRef, Bool) -> Int32).self)
let MTDevicePowerGetEnabled = need("MTDevicePowerGetEnabled",
                                   (@convention(c) (MTDeviceRef, UnsafeMutablePointer<Bool>) -> Int32).self)
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
var engageAt = 5.0
var argv = CommandLine.arguments.dropFirst().makeIterator()
while let a = argv.next() {
    switch a {
    case "--arm": arm = argv.next() ?? arm
    case "--seconds": seconds = Double(argv.next() ?? "") ?? seconds
    case "--engage-at": engageAt = Double(argv.next() ?? "") ?? engageAt
    default: fatalError("unknown argument \(a)")
    }
}
let arms = ["baseline", "parser-off", "stop", "power-off", "hidtap", "hidtap-2", "hidtap-3", "hidtap-3ns", "hidwatch", "restore"]
guard arms.contains(arm) else { fatalError("unknown arm \(arm); one of \(arms)") }

// MARK: - Raw frames

// Per-device state, touched only from that device's callback thread.
var lastCount: [UInt: Int32] = [:]
var lastSample: [UInt: Double] = [:]
var outOfRange = 0
var cursorAtTouchdown = NSPoint.zero
// hidtap-3: current contact count and the sequence's peak, written by the frame
// callback and read by the tap callback (a torn read only costs one event).
var liveCount: Int32 = 0
var sequencePeak: Int32 = 0
// hidtap-2 diagnostics: when the last contact lifted, and the latest
// momentum-scroll state seen by the tap.
var lastLift = 0.0
var lastMomentum = 0.0
var momentumPhase: Int64 = 0

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
    if n == 0 && liveCount > 0 { lastLift = ProcessInfo.processInfo.systemUptime }
    liveCount = n
    sequencePeak = n == 0 ? 0 : max(sequencePeak, n)
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
var signalSources: [DispatchSourceSignal] = []
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

// MARK: - Levers

func leverState(_ dev: MTDeviceRef) -> String {
    var parser = false, power = false
    let rp = MTDeviceGetParserEnabled(dev, &parser)
    let rw = MTDevicePowerGetEnabled(dev, &power)
    return "parser=\(parser) (rc \(rp)) power=\(power) (rc \(rw))"
}

var hidTap: CFMachPort?
// Touch counts AppKit reports on gesture (type 29) events, as "local raw
// count/allTouches count" → occurrences; logged once a second. Answers
// whether a forwarded (Universal Control) gesture event carries its fingers.
var touchHist: [String: Int] = [:]
// hidwatch: sample the NSTouch data on gesture events (every 10th event with
// touches, plus every count change): does a Universal Control-forwarded event
// carry positions, identities and a device size the guest MT device could use?
var touchEvents = 0
var lastLoggedCount = 0
var touchIdentities: [String: Int] = [:]
func logTouches(_ touches: Set<NSTouch>, _ n: Int) {
    touchEvents += 1
    guard n != lastLoggedCount || touchEvents % 10 == 0 else { return }
    lastLoggedCount = n
    // Touches forwarded by Universal Control have no device, a 0x0 deviceSize,
    // and normalizedPosition does not return normally for them: never read a
    // position without a device.
    if touches.contains(where: { $0.device == nil }) {
        let phases = touches.map { "\($0.phase.rawValue)" }.joined(separator: ",")
        log("NSTCH", "raw\(liveCount) ns\(n) NO DEVICE (phases \(phases))")
        return
    }
    let body = touches.sorted { $0.normalizedPosition.x < $1.normalizedPosition.x }.map { t -> String in
        let key = "\(t.identity.hash)"
        let id = touchIdentities[key] ?? { let v = touchIdentities.count; touchIdentities[key] = v; return v }()
        return String(format: "[t%d ph%lu (%.3f,%.3f) %@]", id, t.phase.rawValue,
                      t.normalizedPosition.x, t.normalizedPosition.y, t.isResting ? "rest" : "")
    }.joined(separator: " ")
    let size = touches.first.map { String(format: "%.0fx%.0f pt", $0.deviceSize.width, $0.deviceSize.height) } ?? "-"
    let dev = touches.first.map { "\(ObjectIdentifier($0.device as AnyObject).hashValue & 0xffff)" } ?? "-"
    log("NSTCH", "raw\(liveCount) ns\(n) dev\(dev) size \(size) " + body)
}

// hidtap-3ns: the sequence's peak AppKit touch count. Counts of 0–2
// interleave while fingers land and lift, so a sequence ends only after
// 150 ms without a nonzero count.
var nsPeak = 0
var nsLastNonzero = 0.0
func noteTouches(_ type: CGEventType, _ event: CGEvent) {
    guard type.rawValue == 29, let ns = NSEvent(cgEvent: event) else { return }
    let touches = ns.allTouches()
    let n = touches.count
    if arm == "hidwatch" && n > 0 { logTouches(touches, n) }
    touchHist["raw\(liveCount)/ns\(n)", default: 0] += 1
    let now = ProcessInfo.processInfo.systemUptime
    if now - nsLastNonzero > 0.15 { nsPeak = 0 }
    if n > 0 { nsLastNonzero = now; nsPeak = max(nsPeak, n) }
}
var hidSwallowed = 0
var engaged = false

// Gesture-ish CGEvent types: rotate 18, begin 19, end 20, gesture 29,
// magnify 30, swipe 31, smart magnify 32. Scroll (22) passes, so the log
// still shows whether cooked scroll survives.
let hidTypes: [UInt32] = [18, 19, 20, 29, 30, 31, 32]
let hidCallback: CGEventTapCallBack = { _, type, event, _ in
    if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
        if let t = hidTap {
            CGEvent.tapEnable(tap: t, enable: true)
            log("LEVER", "hid tap re-enabled after \(type.rawValue)")
        }
        return Unmanaged.passUnretained(event)
    }
    // hidtap-3 swallows only while exactly three fingers are down and the
    // sequence never reached four: four-finger gestures must still reach macOS.
    noteTouches(type, event)
    let wanted: Bool
    switch arm {
    case "hidtap-2": wanted = liveCount == 2 && sequencePeak == 2
    case "hidtap-3": wanted = liveCount == 3 && sequencePeak == 3
    case "hidtap-3ns": wanted = nsPeak == 3
    default: wanted = true
    }
    let now = ProcessInfo.processInfo.systemUptime
    if type == .scrollWheel {
        let mp = event.getIntegerValueField(.scrollWheelEventMomentumPhase)
        if mp != 0 { lastMomentum = now }
        if mp != momentumPhase && arm == "hidtap-2" && engaged {
            log("DIAG", "scroll momentum phase \(momentumPhase) -> \(mp)")
        }
        momentumPhase = mp
        return Unmanaged.passUnretained(event)
    }
    if arm == "hidtap-2" && engaged && hidTypes.contains(type.rawValue) {
        let touching = liveCount > 0 ? "down" : String(format: "lifted %.0f ms ago", (now - lastLift) * 1000)
        log("DIAG", String(format: "%@ type %u raw%d peak%d %@, momentum phase %lld (last %.0f ms ago)",
                           wanted ? "SWALLOW" : "PASS", type.rawValue, liveCount, sequencePeak, touching,
                           momentumPhase, (now - lastMomentum) * 1000))
    }
    if engaged && wanted && hidTypes.contains(type.rawValue) {
        hidSwallowed += 1
        if hidSwallowed % 20 == 1 { log("LEVER", "hid tap swallowed type \(type.rawValue) (#\(hidSwallowed))") }
        return nil
    }
    return Unmanaged.passUnretained(event)
}

// hidwatch: a listen-only HID tap over every event type, logging which types
// arrive from which source process (Universal Control injects remote input,
// so its pid tells remote from local). New (type, pid) pairs log at once;
// counts per pair log once a second.
var watchCounts: [String: Int] = [:]
var watchSeen = Set<String>()
func procName(_ pid: Int64) -> String {
    if pid == 0 { return "kernel/HID" }
    var buf = [CChar](repeating: 0, count: 256)
    return proc_name(Int32(pid), &buf, 256) > 0 ? String(cString: buf) : "?"
}
let watchCallback: CGEventTapCallBack = { _, type, event, _ in
    if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
        if let t = hidTap { CGEvent.tapEnable(tap: t, enable: true) }
        return Unmanaged.passUnretained(event)
    }
    noteTouches(type, event)
    let pid = event.getIntegerValueField(.eventSourceUnixProcessID)
    let key = "type \(type.rawValue) from \(procName(pid))[\(pid)]"
    watchCounts[key, default: 0] += 1
    if watchSeen.insert(key).inserted {
        var extra = ""
        if type == .scrollWheel {
            extra = String(format: " dy=%.1f dx=%.1f", event.getDoubleValueField(.scrollWheelEventPointDeltaAxis1),
                           event.getDoubleValueField(.scrollWheelEventPointDeltaAxis2))
        }
        log("WATCH", "first \(key)\(extra)")
    }
    return Unmanaged.passUnretained(event)
}

func engage() {
    engaged = true
    for dev in devices {
        let d = String(format: "dev%lx", UInt(bitPattern: dev) & 0xffff)
        switch arm {
        case "parser-off": log("LEVER", "\(d) SetParserEnabled(false) rc=\(MTDeviceSetParserEnabled(dev, false))")
        case "stop": log("LEVER", "\(d) MTDeviceStop rc=\(MTDeviceStop(dev))")
        case "power-off": log("LEVER", "\(d) PowerSetEnabled(false) rc=\(MTDevicePowerSetEnabled(dev, false))")
        default: break
        }
        log("LEVER", "\(d) engaged: \(leverState(dev))")
    }
    if arm == "hidwatch" {
        hidTap = CGEvent.tapCreate(tap: .cghidEventTap, place: .headInsertEventTap, options: .listenOnly,
                                   eventsOfInterest: ~CGEventMask(0), callback: watchCallback, userInfo: nil)
        if let t = hidTap {
            CFRunLoopAddSource(CFRunLoopGetMain(), CFMachPortCreateRunLoopSource(nil, t, 0), .commonModes)
            CGEvent.tapEnable(tap: t, enable: true)
            log("LEVER", "hid watch tap installed (listen-only)")
        } else {
            log("LEVER", "hid watch tap creation FAILED")
        }
        Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in
            guard !watchCounts.isEmpty else { return }
            log("WATCH", watchCounts.sorted { $0.key < $1.key }.map { "\($0.key): \($0.value)" }.joined(separator: "; "))
            watchCounts.removeAll()
        }
    }
    if arm.hasPrefix("hid") {
        Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in
            guard !touchHist.isEmpty else { return }
            log("TOUCH", touchHist.sorted { $0.key < $1.key }.map { "\($0.key): \($0.value)" }.joined(separator: " "))
            touchHist.removeAll()
        }
    }
    if arm.hasPrefix("hidtap") {
        // Scroll (22) is in the mask only so hidtap-2 can see momentum; it always passes.
        let mask = (hidTypes + [22]).reduce(CGEventMask(0)) { $0 | (CGEventMask(1) << CGEventMask($1)) }
        hidTap = CGEvent.tapCreate(tap: .cghidEventTap, place: .headInsertEventTap, options: .defaultTap,
                                   eventsOfInterest: mask, callback: hidCallback, userInfo: nil)
        if let t = hidTap {
            CFRunLoopAddSource(CFRunLoopGetMain(), CFMachPortCreateRunLoopSource(nil, t, 0), .commonModes)
            CGEvent.tapEnable(tap: t, enable: true)
            log("LEVER", "hid tap installed")
        } else {
            log("LEVER", "hid tap creation FAILED (TCC: Accessibility for the responsible process?)")
        }
    }
    log("LEVER", ">>> ENGAGED — gesture now")
}

func release() {
    guard engaged else { return }
    engaged = false
    for dev in devices {
        let d = String(format: "dev%lx", UInt(bitPattern: dev) & 0xffff)
        switch arm {
        case "parser-off": log("LEVER", "\(d) SetParserEnabled(true) rc=\(MTDeviceSetParserEnabled(dev, true))")
        case "stop": log("LEVER", "\(d) MTDeviceStart rc=\(MTDeviceStart(dev, 0))")
        case "power-off": log("LEVER", "\(d) PowerSetEnabled(true) rc=\(MTDevicePowerSetEnabled(dev, true))")
        default: break
        }
        log("LEVER", "\(d) released: \(leverState(dev))")
    }
    // Clear hidTap first: disabling delivers tapDisabledByUserInput to the
    // callback, which would otherwise re-enable the tap.
    if let t = hidTap { hidTap = nil; CGEvent.tapEnable(tap: t, enable: false) }
    log("LEVER", "<<< RELEASED")
}

for s in [SIGINT, SIGTERM] {
    signal(s, SIG_IGN)
    let src = DispatchSource.makeSignalSource(signal: s, queue: .main)
    src.setEventHandler { log("INFO", "signal \(s)"); release(); exit(1) }
    src.resume()
    signalSources.append(src)
}

for dev in devices {
    log("LEVER", String(format: "dev%lx initial: ", UInt(bitPattern: dev) & 0xffff) + leverState(dev))
}

if arm == "restore" {
    for dev in devices {
        let d = String(format: "dev%lx", UInt(bitPattern: dev) & 0xffff)
        log("LEVER", "\(d) restore parser rc=\(MTDeviceSetParserEnabled(dev, true)) power rc=\(MTDevicePowerSetEnabled(dev, true))")
        log("LEVER", "\(d) now: \(leverState(dev))")
        _ = MTDeviceStop(dev)
    }
    exit(0)
}

if arm != "baseline" {
    precondition(seconds >= engageAt + 15, "lever arms need --seconds >= engage-at + 15")
    Timer.scheduledTimer(withTimeInterval: engageAt, repeats: false) { _ in engage() }
    Timer.scheduledTimer(withTimeInterval: seconds - 5, repeats: false) { _ in release() }
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
// Notification Center's panel is its own process's window, watched the same way.
var lastWindows: [String: String] = [:]
Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in
    let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
    for owner in ["Dock", "NotificationCenter"] {
        let layers = info.filter { ($0[kCGWindowOwnerName as String] as? String) == owner }
            .map { "L\($0[kCGWindowLayer as String] ?? "?")" }.sorted().joined(separator: ",")
        if layers != lastWindows[owner] {
            log("HOST", "\(owner) windows: [\(layers)]")
            lastWindows[owner] = layers
        }
    }
}

Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { _ in
    release()
    for dev in devices { _ = MTDeviceStop(dev) }
    log("INFO", "done; contacts outside [0,1]: \(outOfRange)")
    exit(0)
}

log("INFO", "ready — gesture now")
app.run()
