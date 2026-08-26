import Foundation
import IOSurface
// Cheap census of every live global IOSurface: id, size, and a sampled checksum. Run twice and
// diff to find which surface the guest is actually drawing into right now — the scanout pool
// rotates, so an id that was live at boot goes stale without any log line saying so.
let lo = UInt32(CommandLine.arguments.count > 1 ? Int(CommandLine.arguments[1])! : 1)
let hi = UInt32(CommandLine.arguments.count > 2 ? Int(CommandLine.arguments[2])! : 800)
for id in lo...hi {
    guard let s = IOSurfaceLookup(IOSurfaceID(id)) else { continue }
    let w = IOSurfaceGetWidth(s), h = IOSurfaceGetHeight(s)
    if w < 640 || h < 480 { continue }
    IOSurfaceLock(s, .readOnly, nil)
    let bpr = IOSurfaceGetBytesPerRow(s)
    let p = IOSurfaceGetBaseAddress(s).assumingMemoryBound(to: UInt8.self)
    var sum: UInt64 = 0, nz = 0
    var y = 0
    while y < h { var x = 0
        while x < w { let v = UInt64(p[y*bpr + x*4]) &+ UInt64(p[y*bpr + x*4+1]) &+ UInt64(p[y*bpr + x*4+2])
            sum = sum &* 31 &+ v; if v != 0 { nz += 1 }; x += 16 }
        y += 8 }
    IOSurfaceUnlock(s, .readOnly, nil)
    print("\(id)\t\(w)x\(h)\tnz=\(nz)\tsum=\(sum)")
}
