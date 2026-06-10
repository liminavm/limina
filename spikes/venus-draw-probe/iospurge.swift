// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Query (and optionally flip) the purgeable state of global IOSurfaces.
// Usage: iospurge <id> [<id>...]        — print current purgeable state (non-destructively)
// IOSurfaceSetPurgeable with KeepCurrent returns the current state without changing it.
import Foundation
import IOSurface

let names: [UInt32: String] = [0: "NonVolatile", 1: "Volatile", 2: "Empty(PURGED)"]
for arg in CommandLine.arguments.dropFirst() {
    guard let id = UInt32(arg) else { continue }
    guard let surf = IOSurfaceLookup(IOSurfaceID(id)) else {
        print("id=\(id) not alive"); continue
    }
    var old: UInt32 = 99
    // kIOSurfacePurgeableKeepCurrent = 3: query without modifying.
    let kr = IOSurfaceSetPurgeable(surf, 3, &old)
    let w = IOSurfaceGetWidth(surf), h = IOSurfaceGetHeight(surf)
    let state = names[old] ?? "?(\(old))"
    print("id=\(id) \(w)x\(h) kr=\(kr) purgeable=\(state) useCount=\(IOSurfaceGetUseCount(surf)) inUse=\(IOSurfaceIsInUse(surf))")
}
