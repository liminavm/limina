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

    /// Skip the Spaces fullscreen transition entirely and cover the panel with an ordinary
    /// window instead. `NOTCH_LEGACY=1` (or `=borderless`) uses a `.borderless` window;
    /// `NOTCH_LEGACY=titled` keeps `.titled` and adds `.fullSizeContentView` with the title bar
    /// hidden.
    ///
    /// The distinction is not cosmetic and limina cannot use the borderless one: a borderless
    /// `NSWindow` returns false from `canBecomeKeyWindow`, so it never takes keyboard focus —
    /// fatal for a VM window. The titled variant stays key-eligible. Whether the *compositor*
    /// treats them the same for the camera-housing mask is exactly the thing to measure rather
    /// than assume.
    let legacyMode = ProcessInfo.processInfo.environment["NOTCH_LEGACY"] ?? ""
    var legacy: Bool { !legacyMode.isEmpty }
    var legacyTitled: Bool { legacyMode == "titled" }

    /// Marker band drawn across the top of the content; its visibility beside the camera
    /// housing is the actual question.
    let band = CALayer()

    /// `NOTCH_AUX=1`: enter NATIVE fullscreen (so we keep the Space, the Mission Control slot
    /// and the swipe), then float a borderless `.fullScreenAuxiliary` window over the camera
    /// housing strip. If that window paints there, we can have the Space *and* the whole panel
    /// — the scanout's top rows would be drawn by a second layer over the same IOSurface.
    /// Private Space APIs are not an option: CGSSpaceCreate is a no-op for an unprivileged
    /// WindowServer connection (only Dock.app's is a universal owner), so a real Mission
    /// Control desktop needs SIP off and code injected into Dock.
    let auxMode = ProcessInfo.processInfo.environment["NOTCH_AUX"] ?? ""
    var auxArm: Bool { !auxMode.isEmpty }
    /// `NOTCH_AUX=full`: the overlay covers the WHOLE panel rather than just the housing strip —
    /// native fullscreen as a *carrier* for the Space, with all the pixels coming from an
    /// auxiliary window floating above menu-bar level. If that holds, the menu bar cannot appear
    /// over the guest at all, and top-edge resistance stops being needed.
    var auxFull: Bool { auxMode == "full" }
    var auxWindow: NSWindow?

    /// Keep the band pinned to the top of the content view (the layer is `.never`-redrawing and
    /// has no autoresizing, so every geometry change has to move it by hand).
    func layoutBand() {
        guard let v = window?.contentView else { return }
        let h: CGFloat = 80
        band.frame = CGRect(x: 0, y: v.bounds.height - h, width: v.bounds.width, height: h)
    }

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
        print("  window.canBecomeKey/isKey   \(window.canBecomeKey) / \(window.isKeyWindow)")
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
        // Always the subclass: limina's window is one from creation, and the `keyable` arm
        // restyles THAT window rather than making a new one — which is the case that matters,
        // since the question is whether a key window survives the switch to borderless.
        window = KeyableWindow(contentRect: rect,
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

        // A magenta band across the top 80 pt of the content, so the eye can answer what the
        // geometry cannot: whether anything we draw actually LANDS beside the camera housing.
        // Under native fullscreen the window's frame can be the whole panel while the system
        // still masks that strip — frames are a proxy, pixels are the fact (dogfood-mac, 2026-08-01).
        band.backgroundColor = NSColor.systemPink.cgColor
        layer.addSublayer(band)
        layoutBand()

        if legacy {
            // "Legacy" fullscreen: a borderless window covering screen.frame with the menu bar
            // and Dock hidden, instead of a Spaces fullscreen transition. This is the only
            // remaining candidate for reaching the housing strip.
            print("LEGACY fullscreen arm: \(legacyMode)")
            if legacyMode == "keyable" {
                // What limina must actually ship. Borderless is the only style the compositor
                // lets draw beside the housing (titled + fullSizeContentView is masked, measured
                // 2026-08-01), but a stock borderless NSWindow refuses key status. The window
                // being a subclass that overrides canBecomeKey/canBecomeMain keeps focus
                // available without giving up borderless.
                window.styleMask = [.borderless]
            } else if legacyTitled {
                // The shippable recipe: still titled (so it can become key), title bar folded
                // into the content area and made invisible.
                window.styleMask = [.titled, .closable, .miniaturizable, .resizable,
                                    .fullSizeContentView]
                window.titlebarAppearsTransparent = true
                window.titleVisibility = .hidden
                for b: NSWindow.ButtonType in [.closeButton, .miniaturizeButton, .zoomButton] {
                    window.standardWindowButton(b)?.isHidden = true
                }
                window.isMovable = false
            } else {
                window.styleMask = [.borderless]
            }
            NSApp.presentationOptions = [.hideMenuBar, .hideDock]
            window.level = .mainMenu + 1
            window.setFrame(screen.frame, display: true)
            window.makeKeyAndOrderFront(nil)
            print("  canBecomeKey=\(window.canBecomeKey) isKey=\(window.isKeyWindow)")
            layoutBand()
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                self.layoutBand()
                self.dump("legacy fullscreen (borderless at screen.frame)")
                print("LOOK NOW: is the pink band visible BESIDE the camera housing?")
                DispatchQueue.main.asyncAfter(deadline: .now() + 8.0) {
                    NSApp.terminate(nil)
                }
            }
            return
        }

        dump("windowed")
        // Activation is asynchronous, and `toggleFullScreen:` on a not-yet-active app is a
        // no-op (the first run left styleMask.fullScreen false) — give it room to land.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            NSApp.activate(ignoringOtherApps: true)
            self.window.makeKeyAndOrderFront(nil)
            self.window.toggleFullScreen(nil)
            // The transition animates; sample well after it settles.
            DispatchQueue.main.asyncAfter(deadline: .now() + 2.5) {
                self.layoutBand()
                self.dump("fullscreen (default)")
                if self.auxArm {
                    let scr = self.window.screen ?? screen
                    let strip = self.auxFull ? scr.frame : NSRect(
                        x: scr.frame.origin.x,
                        y: scr.frame.origin.y + scr.frame.height - 80,
                        width: scr.frame.width, height: 80)
                    let a = KeyableWindow(contentRect: strip, styleMask: [.borderless],
                                          backing: .buffered, defer: false)
                    a.collectionBehavior = [.fullScreenAuxiliary]
                    a.level = .mainMenu + 1
                    a.isOpaque = true
                    a.backgroundColor = self.auxFull ? .systemGreen : .systemPink
                    if self.auxFull {
                        // Same marker band, so "does the overlay reach the housing" is still the
                        // question the eye answers. And it must take key: an overlay carrying the
                        // whole guest has to receive the keyboard.
                        let l = CALayer()
                        l.isOpaque = true
                        l.backgroundColor = NSColor.systemGreen.cgColor
                        a.contentView!.layer = l
                        a.contentView!.wantsLayer = true
                        let b = CALayer()
                        b.backgroundColor = NSColor.systemPink.cgColor
                        b.frame = CGRect(x: 0, y: scr.frame.height - 80,
                                         width: scr.frame.width, height: 80)
                        l.addSublayer(b)
                        a.makeKeyAndOrderFront(nil)
                        NSApp.activate(ignoringOtherApps: true)
                    } else {
                        a.ignoresMouseEvents = true
                        a.orderFrontRegardless()
                    }
                    self.auxWindow = a
                    print("AUX(\(self.auxMode)) window at \(a.frame) canBecomeKey=\(a.canBecomeKey) isKey=\(a.isKeyWindow)")
                    print("LOOK: pink BESIDE the housing? and does the menu bar appear OVER it?")
                    fflush(stdout)
                    DispatchQueue.main.asyncAfter(deadline: .now() + 10.0) { NSApp.terminate(nil) }
                    return
                }
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

/// Borderless windows refuse key status by default; overriding these two is the whole fix.
final class KeyableWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let probe = Probe()
app.delegate = probe
app.run()
