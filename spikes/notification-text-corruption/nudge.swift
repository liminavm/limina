// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Jiggle the host cursor a few pixels. GNOME withholds notification BANNERS while its idle monitor
// says the user is away — they go straight to the tray, nothing repaints, and an automated repro
// loop silently measures nothing. A warp resets the guest's idle timer without synthesising clicks
// or keystrokes (and CGWarpMouseCursorPosition needs no Accessibility grant).
import CoreGraphics
import Foundation
// Warp INSIDE the limina window: limina only forwards pointer motion while the cursor is over the
// guest surface, so a jiggle at wherever the host cursor happens to sit never reaches the guest and
// the idle timer keeps climbing.
var target = CGPoint(x: 400, y: 400)
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
for w in list where (w[kCGWindowOwnerName as String] as? String ?? "").lowercased().contains("limina") {
    if let b = w[kCGWindowBounds as String] as? [String: Any],
       let x = b["X"] as? Double, let y = b["Y"] as? Double,
       let ww = b["Width"] as? Double, let hh = b["Height"] as? Double {
        // Low in the window: away from the top-centre banner, so the pointer never hovers the card
        // under measurement (hover repaints it and would confound the very thing we sample).
        target = CGPoint(x: x + ww * 0.5, y: y + hh * 0.85)
    }
}
let p = target
for dx in [6.0, -6.0, 3.0, -3.0] {
    CGWarpMouseCursorPosition(CGPoint(x: p.x + dx, y: p.y))
    Thread.sleep(forTimeInterval: 0.04)
}
CGWarpMouseCursorPosition(p)
