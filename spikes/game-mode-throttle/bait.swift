// A stand-in "game": a games-category app (see Info.plist) that goes native-fullscreen and draws
// with Metal, which is what makes gamepolicyd start a fullscreen gaming session and turn Game Mode
// on. It quits by itself after the number of seconds given as its first argument (default 15), so
// a run can never leave the display taken over.

import AppKit
import MetalKit

final class Renderer: NSObject, MTKViewDelegate {
    let queue: MTLCommandQueue
    var frame = 0
    init(device: MTLDevice) { queue = device.makeCommandQueue()! }
    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}
    func draw(in view: MTKView) {
        frame += 1
        let g = Double(frame % 120) / 120.0
        view.clearColor = MTLClearColor(red: 0.1, green: g, blue: 0.3, alpha: 1)
        guard let pass = view.currentRenderPassDescriptor, let drawable = view.currentDrawable,
              let cb = queue.makeCommandBuffer(),
              let enc = cb.makeRenderCommandEncoder(descriptor: pass) else { return }
        enc.endEncoding()
        cb.present(drawable)
        cb.commit()
    }
}

final class Delegate: NSObject, NSApplicationDelegate {
    var window: NSWindow!
    var renderer: Renderer!
    let seconds: Double
    init(seconds: Double) { self.seconds = seconds }

    func applicationDidFinishLaunching(_ note: Notification) {
        let device = MTLCreateSystemDefaultDevice()!
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 800, height: 600),
                          styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        window.collectionBehavior = [.fullScreenPrimary]
        let view = MTKView(frame: window.contentView!.bounds, device: device)
        view.autoresizingMask = [.width, .height]
        view.preferredFramesPerSecond = 60
        renderer = Renderer(device: device)
        view.delegate = renderer
        window.contentView = view
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        window.toggleFullScreen(nil)
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { NSApp.terminate(nil) }
    }
}

let args = CommandLine.arguments
let seconds = args.count > 1 ? min(max(Double(args[1]) ?? 15, 1), 60) : 15
let app = NSApplication.shared
let delegate = Delegate(seconds: seconds)
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
