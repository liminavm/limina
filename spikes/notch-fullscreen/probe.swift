// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Oracle for "the limina window cuts off at the notch in fullscreen".
//
// Builds the SAME window limina builds (titled + resizable + FullScreenPrimary, a
// layer-hosting content view), toggles native fullscreen, and prints the geometry that
// decides whether the guest scanout can use the strip beside the camera housing:
//
//   screen.frame                 full panel, INCLUDING the notch strip
//   screen.visibleFrame          minus menu bar / Dock
//   screen.safeAreaInsets        top inset = notch height (0 on a notchless display)
//   screen.auxiliaryTopLeftArea  non-nil only on a notched display
//   window.frame                 what AppKit gave the fullscreen window
//   contentView.frame            what we lay the scanout layer out in (window/mod.rs)
//   contentView.safeAreaInsets   what AppKit would inset auto-layout content by
//   window.contentLayoutRect     titlebar-excluded content area
//
// If contentView.frame equals screen.frame in fullscreen, nothing is cutting us off and the
// scanout already covers the notch strip. If it is short by the notch height, AppKit is
// insetting the fullscreen window itself and the fix is a window-level opt-out.
//
// Build+run (no bundle needed):  swiftc -O probe.swift -o probe && ./probe
import AppKit

final class Probe: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var window: NSWindow!
    /// With NOTCH_FULL=1, claim the whole panel for the fullscreen window instead of accepting
    /// AppKit's below-the-notch default. `setFrame(screen.frame)` while already fullscreen is
    /// ignored, so this delegate callback — consulted during the transition — is the lever.
    let wantFull = ProcessInfo.processInfo.environment["NOTCH_FULL"] == "1"

    func window(_ window: NSWindow, willUseFullScreenContentSize proposedSize: NSSize) -> NSSize {
        let full = (window.screen ?? NSScreen.main!).frame.size
        print("  [delegate] willUseFullScreenContentSize proposed=\(proposedSize) "
              + "returning=\(wantFull ? full : proposedSize)")
        return wantFull ? full : proposedSize
    }

    func dump(_ tag: String) {
        let s = window.screen ?? NSScreen.main!
        let cv = window.contentView!
        print("=== \(tag) ===")
        print("  screen.localizedName        \(s.localizedName)")
        print("  screen.frame                \(s.frame)")
        print("  screen.visibleFrame         \(s.visibleFrame)")
        print("  screen.safeAreaInsets       \(s.safeAreaInsets)")
        if #available(macOS 12.0, *) {
            print("  screen.auxiliaryTopLeftArea \(String(describing: s.auxiliaryTopLeftArea))")
        }
        print("  screen.backingScaleFactor   \(s.backingScaleFactor)")
        print("  window.frame                \(window.frame)")
        print("  window.styleMask.fullScreen \(window.styleMask.contains(.fullScreen))")
        print("  window.contentLayoutRect    \(window.contentLayoutRect)")
        print("  contentView.frame           \(cv.frame)")
        print("  contentView.safeAreaInsets  \(cv.safeAreaInsets)")
        print("  contentView.bounds          \(cv.bounds)")
        print("  presentationOptions         \(NSApp.presentationOptions.rawValue)")
        fflush(stdout)
    }

    func applicationDidFinishLaunching(_ n: Notification) {
        let screen = NSScreen.screens.first { $0.safeAreaInsets.top > 0 } ?? NSScreen.main!
        print("all screens:")
        for s in NSScreen.screens {
            print("  \(s.localizedName) frame=\(s.frame) safeTop=\(s.safeAreaInsets.top)")
        }
        print("probing on: \(screen.localizedName)")

        let rect = NSRect(x: screen.frame.origin.x + 100,
                          y: screen.frame.origin.y + 100,
                          width: 900, height: 600)
        window = NSWindow(contentRect: rect,
                          styleMask: [.titled, .closable, .miniaturizable, .resizable],
                          backing: .buffered, defer: false)
        window.title = "notch probe"
        window.collectionBehavior = [.fullScreenPrimary]
        // Same layer-hosting setup as limina's window (window/mod.rs): we own the layer.
        let layer = CALayer()
        layer.isOpaque = true
        layer.backgroundColor = NSColor.systemGreen.cgColor
        window.contentView!.layer = layer
        window.contentView!.wantsLayer = true
        window.contentView!.layerContentsRedrawPolicy = .never
        window.backgroundColor = .black
        window.delegate = self
        print("NOTCH_FULL=\(wantFull)")
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        // Ordering front lands the window on the MAIN screen regardless of the contentRect we
        // asked for (a run with the lid open went fullscreen on the external display instead of
        // the notched one). Re-seat it explicitly, after it is on screen.
        window.setFrame(rect, display: true)

        dump("windowed")
        // Activation is asynchronous, and `toggleFullScreen:` on a not-yet-active app is a
        // no-op (the first run left styleMask.fullScreen false) — give it room to land.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            NSApp.activate(ignoringOtherApps: true)
            self.window.makeKeyAndOrderFront(nil)
            self.window.toggleFullScreen(nil)
            // The transition animates; sample well after it settles.
            DispatchQueue.main.asyncAfter(deadline: .now() + 2.5) {
                self.dump("fullscreen (default)")
                // Does asking for the full frame explicitly change anything?
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    let s = self.window.screen!
                    self.window.setFrame(s.frame, display: true)
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                        self.dump("fullscreen (after setFrame to screen.frame)")
                        NSApp.terminate(nil)
                    }
                }
            }
        }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let probe = Probe()
app.delegate = probe
app.run()
