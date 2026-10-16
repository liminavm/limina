# Metal compute in the guest: a kernel from a host-built .metallib

First step of milestone 18 (`docs/roadmap.md`, Metal compute in the guest, for MLX). A Linux
program in the guest loads a `.metallib` compiled on the host, builds a compute pipeline from it,
and runs it over limina's Metal-level transport, with nothing from KosmicKrisp involved. It uses
`guest/limina-gpu-client` (the transport's guest end, shared with `limina-gpu-smoke`) and the
`CREATE_LIBRARY_DATA` command.

Measured 2026-10-16 on an M1 Max, macOS 27, in a clone of `Fedora-Workstation-44.metal.test.raw`
booted with `LIMINA_GPU_METAL=1 cargo xtask run`, worker and GPU host from limina `f86ef30e`.

| check | result |
|---|---|
| `DeviceInfo` from the GPU host | `Apple M1 Max`, wire version 4, `features` 0x1 (`LIBRARY_DATA`) |
| `axpy.metallib` (4468 bytes, `xcrun -sdk macosx metal`) staged in an arena and loaded | status 0 |
| `axpy` from that library: `y = a·x + y` over 2²⁰ floats, `x`, `y` and the scalar `a` bound by GPU address in an argument table | 1048576/1048576 elements right, 3 runs |
| the same kernel compiled from MSL source, same process (the control) | 1048576/1048576 right, 3 runs |
| commit to completion, metallib kernel first, then the source one | 10.4, 4.6, 5.0 ms; then 0.75, 0.64, 0.66 ms |

The first dispatch of each run is the slow one. Whether that is the metallib or just a process's
first dispatch is not known: the order was not swapped. Timing on this host is noisy while other sessions' VMs run; the oracle here is
correctness.

`crates/limina-gpu-host/tests/first_light.rs` checks the host half on real Metal without a guest:
a metallib's kernel writes the right words, a malformed or oversized metallib fails its reply and
leaves the context alive, and the library is restored from its record after its staging bytes
are overwritten.

## Recipe

```
spikes/metal-compute-mlx/build.sh          # out/axpy.metallib (host) and out/metal-compute-probe (guest)
LIMINA_GPU_METAL=1 cargo xtask run --disk <clone of metal.test>
port=$(scripts/wait-guest-ssh.sh /tmp/limina-worker-<disk>.log)
spikes/metal-compute-mlx/run.sh $port     # copies out/ to /tmp/mcp in the guest and runs it
```

`build.sh` needs Xcode's Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`).

## The command

`CREATE_LIBRARY_DATA` hands a `.metallib` to `newLibraryWithData:`. The bytes sit in an arena, at
most `MAX_LIBRARY_DATA` (256 MiB), because MLX's `mlx.metallib` is 147 MB (mlx-metal 0.32.4) and the ring a few
hundred kilobytes. The host files the command with the bytes inline, so a snapshot restore
replays it with nothing from the arena; a context holds a copy of every library it loaded for as
long as the library lives.

It does not change the wire `VERSION`. KosmicKrisp's guest refuses any version but its own, and it
never sends the command. `DeviceInfo`'s trailing pad, which every host wrote as 0, is now
`features`, and `feature::LIBRARY_DATA` says the host takes the command. A guest checks the bit;
a host without it answers `UnknownOp`.

This is new host surface: Apple's library loader parsing guest-supplied bytes
(`docs/hardening-backlog.md` §Metal transport).

## What MLX needs from the transport

MLX's Metal backend (`mlx/backend/metal`, ml-explore/mlx `b6642e6`) calls about 100 metal-cpp
symbols, all compute: no render or blit encoders, no textures. Against the wire:

| MLX uses | on the wire | where it lands |
|---|---|---|
| `newLibrary` from `mlx.metallib` | `CREATE_LIBRARY_DATA` | **this spike** |
| `newLibrary` from MSL source (JIT kernels), `CompileOptions` math mode and language version | `CREATE_LIBRARY` | exists |
| `newFunction` with `FunctionConstantValues` (bool and int; 173 uses across matmul, SDPA, quantized, conv, normalization, rope) | `CREATE_COMPUTE_PIPELINE` has no constants | **missing**: needed before MLX runs |
| `ComputeCommandEncoder` with `DispatchTypeConcurrent`, `memoryBarrier(BarrierScopeBuffers)` | Metal 4 encoders are concurrent; `BARRIER` | exists |
| `setBuffer` | `SET_ADDRESSES` on an argument table | exists |
| `setBytes` | none | guest side: a scratch range in an arena, bound by address |
| `setThreadgroupMemoryLength` | none | not called (the wrapper in `device.h` has no callers) |
| `dispatchThreads`, `dispatchThreadgroups` | `DISPATCH_THREADS`, `DISPATCH_THREADGROUPS` | exist |
| `updateFence`, `waitForFence` | `FENCE` | exist |
| `encodeSignalEvent`, `encodeWait` on a command buffer | `QUEUE_EVENT`, between commits only | guest side: split the command buffer at the event |
| `SharedEvent` `setSignaledValue`, `waitUntilSignaledValue` | `SET_SHARED_EVENT`, the event's mirror | exist |
| `addCompletedHandler`, `waitUntilCompleted`, command buffer status | `RingControl::completed`, `Feedback` | guest side |
| `newBuffer` (shared and private), `newHeap`, `contents` | arenas | guest side: carve buffers from pooled arenas as KosmicKrisp's bridge does; a context has at most 4096 arenas and a loaded model thousands of buffers |
| `ResidencySet` | none | a no-op: the host keeps every arena of a context resident (`exec.rs`, `sync_residency`) |
| `LinkedFunctions`, `setPrivateFunctions` (custom kernels) | none | later |
| `CaptureManager`, `setLabel`, `NS::Bundle`, `NS::URL` | none | stubs |

## Open

- **libMetal.** A Linux library with metal-cpp's `MTL::` and `NS::` class and method names for
  the calls above, over `limina-gpu-client`, recording each command buffer into its own stream and
  copying it into the ring at commit (MLX encodes from several threads; the ring has one
  producer). No Objective-C runtime: MLX is built from source against these headers.
- **Function constants** on `CREATE_COMPUTE_PIPELINE`, as a second feature bit.
- **MLX for Linux with its Metal backend**: the backend is gated on `APPLE` in its CMake, and its
  build compiles `mlx.metallib` with `xcrun`, which the guest does not have. The metallib is built
  on the host and shipped beside the library (`MLX_METAL_PATH`, `set_metallib_path`). A fork,
  `liminavm/mlx`, when that step comes.
- **omlx** (jundot/omlx) is a Python server over `mlx-lm` and `mlx-vlm`, so the end goal also
  needs MLX's Python bindings built for Linux against libMetal. Its upstream lists macOS only.
