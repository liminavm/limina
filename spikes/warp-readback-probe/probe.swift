import AppKit
import CoreGraphics

func cgLoc() -> CGPoint { CGEvent(source: nil)!.location }
func nsLoc() -> CGPoint {
    let h = CGDisplayBounds(CGMainDisplayID()).size.height
    let p = NSEvent.mouseLocation
    return CGPoint(x: p.x, y: h - p.y)
}
func displays(_ p: CGPoint) -> [CGDirectDisplayID] {
    var ids = [CGDirectDisplayID](repeating: 0, count: 8)
    var n: UInt32 = 0
    CGGetDisplaysWithPoint(p, 8, &ids, &n)
    return Array(ids.prefix(Int(n)))
}
let start = cgLoc()
print("start cg=\(start) ns=\(nsLoc()) displays=\(displays(start))")
// Warp across a large distance: to the centre of the OTHER display if there is one,
// else 200 pt away — the readback question is about display identity, so test a
// cross-display warp when possible.
var ids = [CGDirectDisplayID](repeating: 0, count: 8); var n: UInt32 = 0
CGGetActiveDisplayList(8, &ids, &n)
let all = Array(ids.prefix(Int(n)))
print("active displays: \(all.map { "\($0):\(CGDisplayBounds($0))" })")
let here = displays(start).first ?? CGMainDisplayID()
let other = all.first { $0 != here }
let target: CGPoint
if let o = other { let b = CGDisplayBounds(o); target = CGPoint(x: b.midX, y: b.midY) }
else { target = CGPoint(x: start.x + 200, y: start.y) }
let t0 = DispatchTime.now()
CGWarpMouseCursorPosition(target)
let a = cgLoc(); let b = nsLoc()
let dt = Double(DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds) / 1e6
print("warped to \(target) (display \(displays(target))); immediate readback after \(dt) ms: cg=\(a) ns=\(b) displays(cg)=\(displays(a)) displays(ns)=\(displays(b))")
CGWarpMouseCursorPosition(start)
print("restored; readback cg=\(cgLoc()) ns=\(nsLoc())")
// Also: the same with association off, as the broker does while captured.
CGAssociateMouseAndMouseCursorPosition(0)
CGWarpMouseCursorPosition(target)
let c = cgLoc(); let d = nsLoc()
print("assoc OFF: warped to \(target); readback cg=\(c) ns=\(d) displays(cg)=\(displays(c))")
CGWarpMouseCursorPosition(start)
CGAssociateMouseAndMouseCursorPosition(1)
print("restored+assoc ON; readback cg=\(cgLoc())")
