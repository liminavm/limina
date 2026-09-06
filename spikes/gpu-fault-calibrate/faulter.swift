// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Fires one deliberate GPU address fault and reports what Metal says about it.
// The kernel's own gpuEvent-faulter-*.ips report, which lands in
// /Library/Logs/DiagnosticReports hours later, is the actual output: it names the
// requestor for a known cause and shows whether `address` keeps the byte we asked
// for. See fault.metal for why that matters.
//
//   faulter load    0x9000000007      -- a shader load from an unmapped, misaligned VA
//   faulter texload 0xdeadbeef00000000 -- a read through a resource id that names nothing

import Metal
import Foundation

let args = CommandLine.arguments
guard args.count >= 3,
      let addr = UInt64(args[2].replacingOccurrences(of: "0x", with: ""), radix: 16) else {
   FileHandle.standardError.write("usage: faulter <load|texload> <hex-address>\n".data(using: .utf8)!)
   exit(2)
}
let mode = args[1]

guard let dev = MTLCreateSystemDefaultDevice() else { fatalError("no Metal device") }
let srcPath = (args.count > 3) ? args[3]
   : (URL(fileURLWithPath: args[0]).deletingLastPathComponent().path + "/fault.metal")
let src = try String(contentsOfFile: srcPath, encoding: .utf8)

let opts = MTLCompileOptions()
opts.languageVersion = .version3_2
let lib = try dev.makeLibrary(source: src, options: opts)
guard let fn = lib.makeFunction(name: mode) else { fatalError("no kernel named \(mode)") }
let pso = try dev.makeComputePipelineState(function: fn)

// cfg[0] is the address the load kernel dereferences; cfg[1] is the bogus
// resource id the texture kernel reads through. Both are filled either way so
// the two modes share one buffer layout.
let cfg = dev.makeBuffer(length: 16, options: .storageModeShared)!
cfg.contents().bindMemory(to: UInt64.self, capacity: 2)[0] = addr
cfg.contents().bindMemory(to: UInt64.self, capacity: 2)[1] = addr
let out = dev.makeBuffer(length: 4096, options: .storageModeShared)!

let queue = dev.makeCommandQueue()!
let cb = queue.makeCommandBuffer()!
let enc = cb.makeComputeCommandEncoder()!
enc.setComputePipelineState(pso)
enc.setBuffer(cfg, offset: 0, index: 0)
enc.setBuffer(out, offset: 0, index: 1)
enc.dispatchThreads(MTLSize(width: 64, height: 1, depth: 1),
                    threadsPerThreadgroup: MTLSize(width: 64, height: 1, depth: 1))
enc.endEncoding()

print("pid \(getpid()): dispatching \(mode) against 0x\(String(addr, radix: 16))")
cb.commit()
cb.waitUntilCompleted()

if let e = cb.error {
   print("command buffer error: \(e)")
} else {
   print("command buffer completed with no error (status \(cb.status.rawValue)) -- no fault taken")
}
