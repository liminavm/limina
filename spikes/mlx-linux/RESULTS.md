# MLX on Linux, its Metal backend over limina's transport

Milestone 18 (`docs/roadmap.md`): MLX built for Linux with its Metal backend, in a guest, running
on the host's GPU. MLX keeps Apple's metal-cpp, unmodified. limina-metal
(`spikes/metal-cpp-shim/shim`) supplies the Objective-C runtime entry points metal-cpp calls, and
Metal's compute classes over `guest/limina-gpu-client`.

Measured 2026-10-17 on an M1 Max, macOS 27, in a clone of `Fedora-Workstation-44.metal.test.raw`
(8 vCPUs, 16 GiB, clang 22.1.8, OpenBLAS for MLX's CPU backend), booted with
`LIMINA_GPU_METAL=1 cargo xtask run`. MLX is `ml-explore/mlx` `b6642e6` plus the fork's two commits
below; the GPU host is from limina `5000e7d0`.

| check (`test/check.cpp`, unless named) | result |
|---|---|
| `libmlx.so`, MLX's C++ tests and examples, and `check` build against limina-metal | yes |
| MLX's C++ test suite (`build/tests/tests`, doctest), all of it | 297 of 298 test cases, 4531 of 4532 assertions. The one failure checks that an array made from a user's buffer aliases it (`array_tests.cpp:640`), which needs the GPU to reach the guest's own memory |
| `mx::gpu::is_available()`, and the default device | yes; the GPU |
| `2i+1` over 2²⁰ floats on the GPU, read back on the CPU | every element right |
| 12 ops on the GPU against the same ops on MLX's CPU backend: elementwise, fp32 and fp16 matmul, row and full sums, softmax, argmax, sort, transpose, conv2d, layer norm, 4-bit quantized matmul | all within tolerance. matmul and conv2d come out bit-identical to the CPU backend's OpenBLAS sgemm, which is consistent with both accumulating along K in order (not verified); the sums, softmax and layer norm differ by 1e-8 to 4e-4 of their scale. The utilisation and custom-kernel rows, not these, are the evidence that the GPU ran it |
| a kernel of our own, `mx::fast::metal_kernel` from MSL source, compiled on the host | 4096/4096 right |
| 2048³ fp32 matmul, mean of 5 after a warm-up | GPU 2.5 to 6.0 ms (2.9 to 6.9 TFLOP/s, varying with what else the host ran); CPU backend 185 to 189 ms (91 to 93 GFLOP/s) |
| host GPU utilisation (`ioreg`, `Device Utilization %`) while the guest runs 2048³ matmuls for 10 s | idle 8–9 %, during 89–92 %; 3686 matmuls in the 10 s |
| `examples/cpp/linear_regression` | loss 4.5e-5, \|w − w*\| = 0.0033, 2237 it/s |
| `examples/cpp/logistic_regression` | loss 0.028, accuracy 1.0, 2055 it/s |
| worker log over all of the above | nothing from the GPU host |

`examples/cpp/tutorial` also runs, but checks nothing: its `assert`s compile out of a Release build.
The tests run as `cd ~/mlxl/build/tests && LD_LIBRARY_PATH=~/mlxl/prefix/lib:~/mlxl/build ./tests`.

## Recipe

```
spikes/mlx-linux/host.sh               # out/prefix: limina-metal, stand-in headers, metal-cpp,
                                       # mlx.metallib, the JIT sources; out/mlx-src.tar
LIMINA_GPU_METAL=1 cargo xtask run --disk <clone of metal.test> --cpus 8 --ram-mib 16384
port=$(scripts/wait-guest-ssh.sh /tmp/limina-worker-<disk>.log)
spikes/mlx-linux/guest.sh $port        # copy, deps, so, build (about 3 minutes on 8 vCPUs)
spikes/mlx-linux/guest.sh $port check linear_regression logistic_regression
```

`host.sh` builds from the MLX tree at `third_party/mlx`, the `liminavm/mlx` fork pinned in the
manifest (`cargo xtask vendor` materializes it), and refuses a dirty one:
the artifacts name the commit they came from. The test image's 11 GB root has too little room for
the build; grow the raw file (`truncate -s +24G`), then `growpart /dev/vda 3` (the disk is MBR) and
`btrfs filesystem resize max /` in the guest.

## What MLX needed

Two commits on the fork, both small.

- **`metal: build the backend on Linux over limina's GPU transport`.** Everything sits under one
  CMake option, `MLX_METAL_LIMINA=<prefix>`, so Darwin builds are unchanged.
  - Metal is `liblimina_metal.so` from the prefix. metal-cpp also comes from there, so the guest
    build fetches nothing, and the build adds `-fblocks`.
  - **Host-built artifacts.** `mlx.metallib` and the JIT sources need Apple's Metal toolchain, so
    the host builds them, with MLX's own CMake and `make_compiled_preamble.sh`.
    `prefix/mlx/config.cmake` records the commit and the `MLX_METAL_JIT` setting they came from,
    and the guest's configure refuses a mismatch with either.
  - **Two source changes.** The allocator rounds to the host's 16 KiB page, which Metal allocates
    in whatever page the guest kernel uses. `device_info` reads memory from `sysconf` rather than
    `sysctlbyname`.
- **`metal: return a pair, not a tuple, from compute_bluestein_constants`.** libc++ converts a
  `std::tuple` to the `std::pair` the function returns; GCC's libstdc++, which clang uses on
  Fedora, does not. This one is upstreamable as it stands.

## What limina-metal needed beyond the metal-cpp spike

Each gap aborted with its class and selector (`limina-metal: -[MTLDevice respondsToSelector:] is
not implemented`). That makes the next missing method one run away, and a new shim needs only the
`.so` relinked, not MLX.

- **`respondsToSelector:`**, answered from the method tables, and `setLabel:` as a no-op.
- **Device queries**, all from the host's `DeviceInfo`: `architecture`, `supportsFamily:` (Apple1
  up to the host's, Mac2, Common, Metal3, Metal4), `maxBufferLength`,
  `recommendedMaxWorkingSetSize`, `maxThreadgroupMemoryLength` and `registryID`.
  `newLibraryWithFile:` loads the metallib MLX names by path.
- **Shared events.** The host mirrors each event's signaled value into a u64 of the main arena.
  `signaledValue` and `waitUntilSignaledValue:timeoutMS:` read that mirror, and `setSignaledValue:`
  is `SET_SHARED_EVENT`. MLX encodes event waits and signals between encoders, in the middle of a
  command buffer. The wire takes `QUEUE_EVENT` only between commits, so commit splits the command
  buffer there. One `MTLCommandBuffer` becomes several commits, and its status and handlers follow
  the last of them.
- **`MTLFence` is a no-op.** Every command buffer goes through one queue, and each encoder starts
  after the last one's work is done, so a fence between two encoders always holds.
- **Residency sets are no-ops**, because every arena is resident while the guest maps it.
  **`newHeapWithDescriptor:` returns nil**, which Metal allows, and MLX then allocates small
  buffers from the device instead. `MTLCopyAllDevices` returns nil, which MLX takes as "use the
  default device".
- **Compile options.** `setMathMode:` is carried as is, and `languageVersion` is read back.
  `setEnableLogging:` is ignored, because shader logging needs a log stream back from the host
  that the wire does not carry.
- **Pushes wait for ring space.** An MLX command buffer can hold more than the 256 KiB ring, so a
  push that finds the ring full publishes, rings the doorbell and retries, commits included
  (`Gpu::commit_nowait` would panic instead). No run here filled the ring, because MLX commits every
  few dozen dispatches, so this retry path has not run yet.
- **Autorelease pools outlive the thread's other thread-local state.** MLX's per-thread
  destructors use pools at thread exit. A Rust `thread_local!` that owns the pool stack is
  destroyed in no particular order relative to them, so it panicked in MLX's suite. The stack now
  hangs off a raw pointer that registers no destructor, so pools work until the thread is gone.
  What a thread leaves in a pool at exit leaks. `spikes/metal-cpp-shim`'s `thread-exit` run is the
  repro.

- **Buffers come from pooled arenas**, `setBytes` data from a region each command buffer holds
  until it retires, and argument tables are destroyed after their last commit
  (`spikes/metal-cpp-shim/RESULTS.md` §How the shim manages memory and work).
  `newBufferWithBytesNoCopy:` returns nil, which Metal also does for memory it cannot wrap, and
  MLX then copies.

## What is still the spike's

These are untested by what ran here, and some will break a real model:

- **The VM's arena window is 16 GiB** (`ARENA_WINDOW` in `crates/limina-vmm/src/krun/metal_ext.rs`,
  guest-physical), shared by every context, the desktop's Vulkan driver included. Pooling does not
  move it, and a pooled arena takes its whole 64 MiB of it however little is in use. A model larger
  than about 14 GiB will not fit until the window grows.
- **Every MLX GPU stream shares one wire queue.** Within a stream, commit order follows MLX's
  evaluation order, so events are ordered correctly. Across two GPU streams, a wait committed
  before the signal it waits for would hold the queue forever. MLX uses one GPU stream unless the
  program makes more.
- **Not implemented:** zero-copy user buffers, linked functions, and
  `setThreadgroupMemoryLength:`. MLX has no callers of the last one yet.
- **Library probes print.** MLX probes three paths for a colocated `mlx.metallib` before it finds
  the prefix's, and limina-metal prints every failed load to stderr.
