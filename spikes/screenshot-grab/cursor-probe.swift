// Samples the system-wide cursor and the screenshot UI's on-screen windows side by side, and
// prints a line whenever either changes. The question it answers: does the cursor tell a live
// interactive capture (Cmd-Shift-4 crosshair, its space-bar camera, the Cmd-Shift-5 panel) apart
// from an idle `screencaptureui` window left on screen? And do the focus signals — which app is
// frontmost, whether this process is active, whether its own window is key — tell them apart
// without the cursor, whose system-wide read is deprecated?
//
// The probe opens a window standing in for limina's: click into it before starting a session.
//
//     swiftc -O cursor-probe.swift -o cursor-probe && ./cursor-probe
//
// Each line: wall time, cursor fingerprint (image size in points, hot spot, pixel size, FNV-1a of
// the pixels), the focus state (`front=<frontmost bundle id> active=<this app> key=<its window>`),
// then every on-screen window owned by com.apple.screencaptureui as `#number layer bounds
// mem=<bytes>`.

import AppKit
import CoreGraphics

func fnv1a(_ data: UnsafeRawBufferPointer) -> UInt64 {
    var h: UInt64 = 0xcbf2_9ce4_8422_2325
    for b in data {
        h ^= UInt64(b)
        h = h &* 0x0000_0100_0000_01b3
    }
    return h
}

// Renders into a fixed RGBA layout so the hash depends on the pixels, not on how the image's
// backing store happens to be laid out.
func cursorFingerprint() -> String {
    // Deprecated, and the SDK header warns it "will always be nil in a future version of macOS";
    // a nil here is itself a finding.
    guard let cursor = NSCursor.currentSystem else { return "cursor=nil" }
    let image = cursor.image
    let hot = cursor.hotSpot
    var rect = NSRect(origin: .zero, size: image.size)
    guard let cg = image.cgImage(forProposedRect: &rect, context: nil, hints: nil) else {
        return String(format: "cursor size=%.0fx%.0f hot=(%.1f,%.1f) cg=nil",
                      image.size.width, image.size.height, hot.x, hot.y)
    }
    let w = cg.width, h = cg.height
    var pixels = [UInt8](repeating: 0, count: w * h * 4)
    let ok = pixels.withUnsafeMutableBytes { buf -> Bool in
        guard let ctx = CGContext(data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8,
                                  bytesPerRow: w * 4, space: CGColorSpaceCreateDeviceRGB(),
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return false }
        ctx.draw(cg, in: CGRect(x: 0, y: 0, width: w, height: h))
        return true
    }
    let hash = ok ? pixels.withUnsafeBytes { String(format: "%016llx", fnv1a($0)) } : "draw-failed"
    return String(format: "cursor size=%.0fx%.0f hot=(%.1f,%.1f) px=%dx%d hash=%@",
                  image.size.width, image.size.height, hot.x, hot.y, w, h, hash)
}

func captureWindows() -> String {
    let pids = Set(NSRunningApplication.runningApplications(
        withBundleIdentifier: "com.apple.screencaptureui").map { $0.processIdentifier })
    if pids.isEmpty { return "capture=none" }
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID)
        as? [[String: Any]] ?? []
    let mine = list.filter { pids.contains(($0[kCGWindowOwnerPID as String] as? pid_t) ?? -1) }
    if mine.isEmpty { return "capture=process-only(pid \(pids.sorted()))" }
    return "capture=" + mine.map { w in
        let num = w[kCGWindowNumber as String] as? Int ?? -1
        let layer = w[kCGWindowLayer as String] as? Int ?? -1
        let mem = w[kCGWindowMemoryUsage as String] as? Int ?? -1
        let b = w[kCGWindowBounds as String] as? [String: CGFloat] ?? [:]
        return String(format: "#%d L%d (%.0f,%.0f %.0fx%.0f) mem=%d", num, layer,
                      b["X"] ?? 0, b["Y"] ?? 0, b["Width"] ?? 0, b["Height"] ?? 0, mem)
    }.joined(separator: " ")
}

let stamp = DateFormatter()
stamp.dateFormat = "HH:mm:ss.SSS"
setvbuf(stdout, nil, _IOLBF, 0)

// A regular app with one window, so it can be frontmost and key the way limina's window is when
// a capture session starts over it.
let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(contentRect: NSRect(x: 200, y: 200, width: 900, height: 600),
                      styleMask: [.titled, .resizable], backing: .buffered, defer: false)
window.title = "cursor-probe: click here, then start a capture"
window.makeKeyAndOrderFront(nil)
app.activate()

func focusState() -> String {
    let front = NSWorkspace.shared.frontmostApplication?.bundleIdentifier ?? "nil"
    return "front=\(front) active=\(app.isActive) key=\(window.isKeyWindow)"
}

var last = ""
Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in
    let line = cursorFingerprint() + "  " + focusState() + "  " + captureWindows()
    if line != last {
        print(stamp.string(from: Date()) + "  " + line)
        last = line
    }
}
app.run()
