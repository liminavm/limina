import AppKit
import CoreGraphics

func stamp() -> String { String(format: "%.1f", Date().timeIntervalSince1970.truncatingRemainder(dividingBy: 10000)) }

var prevKeys = Set<Int>()
func sample() {
    guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] else { return }
    let keys = Set(list.compactMap { $0[kCGWindowNumber as String] as? Int })
    if keys == prevKeys { return }
    let added = keys.subtracting(prevKeys), gone = prevKeys.subtracting(keys)
    prevKeys = keys
    print("[\(stamp())] CHANGE added=\(added.sorted()) gone=\(gone.sorted()) total=\(list.count)")
    for (i, w) in list.enumerated() {
        let num = w[kCGWindowNumber as String] as? Int ?? -1
        let pid = w[kCGWindowOwnerPID as String] as? Int32 ?? -1
        let owner = (w[kCGWindowOwnerName as String] as? String) ?? "(nil)"
        let layer = w[kCGWindowLayer as String] as? Int ?? -999
        let alpha = w[kCGWindowAlpha as String] as? Double ?? -1
        let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
        let x = b["X"] as? Double ?? -1, y = b["Y"] as? Double ?? -1
        let ww = b["Width"] as? Double ?? -1, hh = b["Height"] as? Double ?? -1
        let mark = added.contains(num) ? " <== NEW" : ""
        print("  idx=\(i) win=\(num) owner=\(owner) pid=\(pid) layer=\(layer) alpha=\(alpha) bounds=(\(Int(x)),\(Int(y)) \(Int(ww))x\(Int(hh)))\(mark)")
    }
    fflush(stdout)
}

print("probe3 start")
sample()
var n = 0
Timer.scheduledTimer(withTimeInterval: 0.3, repeats: true) { _ in
    n += 1; sample()
    if n >= 200 { print("probe3 done"); exit(0) }
}
RunLoop.main.run()
