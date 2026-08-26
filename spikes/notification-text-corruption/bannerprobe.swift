// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Measure the ink in a GNOME notification banner, read straight off the live global scanout
// IOSurface (needs the VM booted with LIMINA_GLOBAL_SCANOUT=1). No Screen Recording permission,
// and — unlike LIMINA_WINDOW_CAPTURE — no dependence on scanout surface swaps, which this guest
// path barely does: mutter flushes the SAME framebuffer, so the window capture's per-apply counter
// almost never advances while the screen is updating fine.
//
//   bannerprobe <surface-id|auto> [--png <path>]
//
// Prefer "auto". The scanout is a rotating pool AND the pool is re-created over the life of the
// VM (observed 183 -> 255 -> 260 in one session), so any id hard-coded from a log line goes stale
// silently: the probe keeps reading a frozen copy and every trial reports identical ink. "auto"
// sweeps the live global IOSurfaces and takes the one that is actually being written to.
//
// Liveness is decided by sampling each surface's content TWICE and keeping the ones that changed —
// NOT by "which one has a card on it", and NOT by IOSurfaceGetSeed. Both of those were tried and
// are wrong here: picking by content deterministically selects the frozen pool member (it still
// holds the last card it ever saw), and the seed only advances for CPU lock-based writes, so it
// stays flat while the GPU writes the scanout blob every frame.
//
// Prints: TOTAL <n> BANDS <count> ROWS <r1:n1,r2:n2,...>
// A band is a run of consecutive rows carrying ink — one per rendered text row (header / summary /
// body). A card that rendered correctly shows three; a row that failed to render drops a band.
import Foundation
import IOSurface
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

// The banner card, in scanout pixels at 2560x1440 (calibrated against a settled banner).
var X0 = 960, W = 660, Y0 = 25, H = 190
let INK_LUMA = 150      // text on the dark card (the header row is gray, not white)
let ROW_MIN  = 6        // rows with fewer bright px than this count as blank
let CARD_LO   = 40      // card chrome is mid-grey: darker than wallpaper, LIGHTER than
let CARD_HI   = 125     // the near-black top panel, which must not be mistaken for the card
let CARD_FRAC = 60      // % of a row that must be dark for it to be card, not wallpaper
let CARD_MIN_ROWS = 40  // fewer card rows than this means no banner is up

let args = CommandLine.arguments
func applyRect() {
    if let r = rectOverride { X0 = r.0; Y0 = r.1; W = r.2; H = r.3 }
}
guard args.count >= 2 else {
    FileHandle.standardError.write("usage: bannerprobe <surface-id> [--png <path>]\n".data(using: .utf8)!)
    exit(2)
}
var pngPath: String? = nil
if let i = args.firstIndex(of: "--png"), i + 1 < args.count { pngPath = args[i + 1] }
// --sigs writes a signature of every candidate surface; --since reads one back and treats "changed
// since that snapshot" as the definition of live. Sampling liveness inside the probe cannot work:
// by the time it runs the banner is already static, nothing differs over a 300ms window, and the
// probe falls back to whichever stale surface still holds an old card.
var sigsOut: String? = nil
if let i = args.firstIndex(of: "--sigs"), i + 1 < args.count { sigsOut = args[i + 1] }
var sinceIn: String? = nil
if let i = args.firstIndex(of: "--since"), i + 1 < args.count { sinceIn = args[i + 1] }
// --rect overrides the measured region, and --raw reports plain ink in it with no card gating.
// Used to watch the whole notification-list panel of the open clock menu, where the interesting
// quantity is "did this panel lose some of its text", not "where is one card".
var rectOverride: (Int, Int, Int, Int)? = nil
if let i = args.firstIndex(of: "--rect"), i + 1 < args.count {
    let f = args[i + 1].split(separator: ",").compactMap { Int($0) }
    if f.count == 4 { rectOverride = (f[0], f[1], f[2], f[3]) }
}
let rawMode = args.contains("--raw")

applyRect()
var candidates: [UInt32] = []
if args[1] == "auto" {
    for cand in UInt32(1)...UInt32(1400) {
        guard let c = IOSurfaceLookup(IOSurfaceID(cand)) else { continue }
        if IOSurfaceGetWidth(c) >= 1280 && IOSurfaceGetHeight(c) >= 720 { candidates.append(cand) }
    }
} else if let one = UInt32(args[1]) {
    candidates = [one]
}
func sig(_ c: IOSurfaceRef) -> UInt64 {
    IOSurfaceLock(c, .readOnly, nil)
    defer { IOSurfaceUnlock(c, .readOnly, nil) }
    let w = IOSurfaceGetWidth(c), h = IOSurfaceGetHeight(c), bpr = IOSurfaceGetBytesPerRow(c)
    let p = IOSurfaceGetBaseAddress(c).assumingMemoryBound(to: UInt8.self)
    var acc: UInt64 = 0, y = 0
    while y < h { var x = 0
        while x < w { acc = acc &* 31 &+ UInt64(p[y*bpr + x*4+1]); x += 16 }
        y += 8 }
    return acc
}
if let out = sigsOut {
    var lines: [String] = []
    for cand in candidates {
        if let c = IOSurfaceLookup(IOSurfaceID(cand)) { lines.append("\(cand) \(sig(c))") }
    }
    try? lines.joined(separator: "\n").write(toFile: out, atomically: true, encoding: .utf8)
    print("SIGS \(lines.count)")
    exit(0)
}
var live: [UInt32] = []
if let inp = sinceIn, let text = try? String(contentsOfFile: inp, encoding: .utf8) {
    var base: [UInt32: UInt64] = [:]
    for l in text.split(separator: "\n") {
        let f = l.split(separator: " ")
        if f.count == 2, let a = UInt32(f[0]), let b = UInt64(f[1]) { base[a] = b }
    }
    for cand in candidates {
        if let c = IOSurfaceLookup(IOSurfaceID(cand)), base[cand] != sig(c) { live.append(cand) }
    }
} else {
    var sig0: [UInt32: UInt64] = [:]
    for cand in candidates { if let c = IOSurfaceLookup(IOSurfaceID(cand)) { sig0[cand] = sig(c) } }
    Thread.sleep(forTimeInterval: 0.30)
    for cand in candidates {
        if let c = IOSurfaceLookup(IOSurfaceID(cand)), sig(c) != sig0[cand] { live.append(cand) }
    }
}
let pool = live.isEmpty ? candidates : live
var chosen: IOSurfaceRef? = nil
var chosenID: UInt32 = 0
var bestRows = -1
for cand in pool {
    guard let c = IOSurfaceLookup(IOSurfaceID(cand)) else { continue }
    IOSurfaceLock(c, .readOnly, nil)
    let cw = IOSurfaceGetWidth(c), ch = IOSurfaceGetHeight(c), cbpr = IOSurfaceGetBytesPerRow(c)
    let cp = IOSurfaceGetBaseAddress(c).assumingMemoryBound(to: UInt8.self)
    var rows = 0
    for ry in 0..<H {
        let y = Y0 + ry; if y >= ch { break }
        var dark = 0, wid = 0
        for rx in 0..<W {
            let x = X0 + rx; if x >= cw { break }
            let o = y * cbpr + x * 4
            let luma = (Int(cp[o+2]) * 299 + Int(cp[o+1]) * 587 + Int(cp[o]) * 114) / 1000
            wid += 1; if luma >= CARD_LO && luma <= CARD_HI { dark += 1 }
        }
        if wid > 0 && dark * 100 / wid >= CARD_FRAC { rows += 1 }
    }
    IOSurfaceUnlock(c, .readOnly, nil)
    if rows > bestRows { bestRows = rows; chosen = c; chosenID = cand }
}
guard let surf = chosen else { print("SURFACE_DEAD"); exit(1) }
func writePNG(_ path: String) {
    let cs = CGColorSpaceCreateDeviceRGB()
    rgba.withUnsafeMutableBytes { raw in
        let ctx = CGContext(data: raw.baseAddress, width: W, height: H, bitsPerComponent: 8,
                            bytesPerRow: W * 4, space: cs,
                            bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)
        if let img = ctx?.makeImage(),
           let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: path) as CFURL,
                                                      UTType.png.identifier as CFString, 1, nil) {
            CGImageDestinationAddImage(dest, img, nil)
            CGImageDestinationFinalize(dest)
        }
    }
}
let id = chosenID
IOSurfaceLock(surf, .readOnly, nil)
let sw = IOSurfaceGetWidth(surf), sh = IOSurfaceGetHeight(surf)
let bpr = IOSurfaceGetBytesPerRow(surf)
let p = IOSurfaceGetBaseAddress(surf).assumingMemoryBound(to: UInt8.self)

var rowInk  = [Int](repeating: 0, count: H)
var rowDark = [Int](repeating: 0, count: H)
var rowW    = [Int](repeating: 0, count: H)
var rgba = [UInt8](repeating: 0, count: W * H * 4)
for ry in 0..<H {
    let y = Y0 + ry
    if y >= sh { break }
    for rx in 0..<W {
        let x = X0 + rx
        if x >= sw { break }
        let s = y * bpr + x * 4
        let b = Int(p[s]), g = Int(p[s+1]), r = Int(p[s+2])
        let luma = (r * 299 + g * 587 + b * 114) / 1000
        rowW[ry] += 1
        if luma >= INK_LUMA { rowInk[ry] += 1 }
        if luma >= CARD_LO && luma <= CARD_HI { rowDark[ry] += 1 }
        let o = (ry * W + rx) * 4
        rgba[o] = p[s+2]; rgba[o+1] = p[s+1]; rgba[o+2] = p[s]; rgba[o+3] = 255
    }
}
IOSurfaceUnlock(surf, .readOnly, nil)

if rawMode {
    var ink = 0
    for ry in 0..<H { ink += rowInk[ry] }
    print("RAW \(ink) SURF \(id) LIVE \(live.count)")
    if let path = pngPath { writePNG(path) }
    exit(0)
}
// A row belongs to the card when most of it is dark chrome. Text ink is only meaningful there:
// with no banner up, this region is bright wallpaper and every row would read as saturated "ink".
var cardRows = 0
for ry in 0..<H where rowW[ry] > 0 && rowDark[ry] * 100 / rowW[ry] >= CARD_FRAC { cardRows += 1 }
if cardRows < CARD_MIN_ROWS { print("NO_CARD cardRows \(cardRows)"); exit(0) }

// Anchor the three text rows to the top of the card rather than to absolute scanout rows: the
// banner shifts vertically while it slides in, but its internal layout does not.
var cardTop = -1
for ry in 0..<H where rowW[ry] > 0 && rowDark[ry] * 100 / rowW[ry] >= CARD_FRAC { cardTop = ry; break }
func bandInk(_ lo: Int, _ hi: Int) -> Int {
    var n = 0
    for ry in (cardTop + lo)...(cardTop + hi) where ry >= 0 && ry < H {
        let isCard = rowW[ry] > 0 && rowDark[ry] * 100 / rowW[ry] >= CARD_FRAC
        if isCard { n += rowInk[ry] }
    }
    return n
}
let header = bandInk(24, 46)   // app icon + app name + "Just now"
let title  = bandInk(60, 86)   // the summary line
let body   = bandInk(88, 116)  // the body line
var profile: [String] = []
for ry in 0..<H where rowInk[ry] > 0 { profile.append("\(ry):\(rowInk[ry])") }
print("LIVE \(live.count) SURF \(id) HEADER \(header) TITLE \(title) BODY \(body) CARDTOP \(cardTop) CARDROWS \(cardRows) ROWS \(profile.joined(separator: ","))")

if let path = pngPath { writePNG(path) }
