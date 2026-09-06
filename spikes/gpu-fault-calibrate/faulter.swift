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
      let addr = (args[2] == "self") ? UInt64(0) : UInt64(args[2].replacingOccurrences(of: "0x", with: ""), radix: 16) else {
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
let out = dev.makeBuffer(length: 4096, options: .storageModeShared)!

// "self" points the load at a known-good address -- the cfg buffer itself -- so a
// run that reads back the value we planted proves the kernel really executed.
// Without that control, "no fault" is indistinguishable from "the dispatch did
// nothing", and this program's whole purpose is to be a control.
let c = cfg.contents().bindMemory(to: UInt64.self, capacity: 2)
// Point the load at cfg[1] and plant a magic there, so a working dispatch reads
// back 0xcafebabe. Pointing it at cfg[0] would read a zero either way and prove
// nothing.
let target = (args[2] == "self") ? cfg.gpuAddress + 8 : addr
c[0] = target
c[1] = (args[2] == "self") ? 0xcafebabe : target
print("cfg gpuAddress 0x\(String(cfg.gpuAddress, radix: 16)), out gpuAddress 0x\(String(out.gpuAddress, radix: 16))")

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
   let r = out.contents().bindMemory(to: UInt32.self, capacity: 4)
   print("command buffer completed, status \(cb.status.rawValue) -- no fault taken")
   print("out[0..3] = \(r[0]) \(r[1]) \(r[2]) \(r[3])")
}
