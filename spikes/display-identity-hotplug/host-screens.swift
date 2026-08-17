// Print each host screen's global frame and localized name, so a test can move a window onto a
// chosen display by coordinate. AppKit's global space has the main display's lower-left at (0,0)
// and other displays offset from it, including negative origins.
//
//   swift host-screens.swift
import AppKit

for screen in NSScreen.screens {
    let f = screen.frame
    let main = screen == NSScreen.main ? " MAIN" : ""
    let scale = screen.backingScaleFactor
    print("\(screen.localizedName)\tframe=\(Int(f.origin.x)),\(Int(f.origin.y)) \(Int(f.size.width))x\(Int(f.size.height))\tbacking=\(scale)\(main)")
}
