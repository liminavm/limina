import Foundation
import IOSurface
// Does IOSurface honor a forced, non-256-aligned bytesPerRow?
for w in [1968, 1974, 1976, 1980] {
    for align in [16, 256] {
        let want = ((w*4) + align - 1) / align * align
        let props: [IOSurfacePropertyKey: Any] = [
            .width: w, .height: 64, .bytesPerElement: 4,
            .pixelFormat: 0x42475241 /* 'BGRA' */,
            .bytesPerRow: want,
        ]
        if let s = IOSurface(properties: props) {
            let got = s.bytesPerRow
            print("w=\(w) align=\(align) asked=\(want) got=\(got) \(got == want ? "HONORED" : "OVERRIDDEN")")
        } else { print("w=\(w) align=\(align) asked=\(want) ALLOC FAILED") }
    }
}
