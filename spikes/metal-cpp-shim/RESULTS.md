# Apple's metal-cpp, unmodified, on Linux over limina's Metal transport

Milestone 18 (`docs/roadmap.md`) needs MLX's Metal backend to run in a Linux guest. MLX talks to
Metal through Apple's `metal-cpp` headers, which turn every call into
`objc_msgSend(object, selector, args…)` and look classes and selectors up by name. This spike
supplies those runtime entry points on Linux and puts Metal's compute classes behind them, over
`guest/limina-gpu-client`. A program built against metal-cpp as Apple ships it then runs on the
host's GPU, without anyone maintaining a copy of metal-cpp's headers.

Measured 2026-10-17 on an M1 Max, macOS 27, in a clone of `Fedora-Workstation-44.metal.test.raw`
(kernel 6.19.10-300.fc44, glibc 2.43, clang 22.1.8), booted with `LIMINA_GPU_METAL=1 cargo xtask
run`. The GPU host is from limina `5000e7d0` (function constants on the wire) and libkrun
`9dc32a05`. metal-cpp is `metal-cpp_26.zip` (sha256 `4df3c078…a3a4`).

| check (test/axpy.cpp, written as MLX writes its calls) | result, 4 runs |
|---|---|
| builds with `clang++ -std=c++17 -fblocks` against metal-cpp and the stand-in headers, linked to `liblimina_metal.a` | yes |
| `MTL::CreateSystemDefaultDevice`, `name()`, and `maxThreadsPerThreadgroup()`, a 24-byte struct return | `Apple M1 Max (limina)`, 1024×1024×1024 |
| `newLibrary(NS::URL)` from a `.metallib`, then `axpy` over 2²⁰ floats: two `setBuffer`s and a `setBytes` scalar | 1048576/1048576 right |
| `spec`, specialised through `FunctionConstantValues` and `FunctionDescriptor` as MLX's `get_function_` does, after a `memoryBarrier` on the same concurrent encoder | 1048576/1048576 right |
| `addCompletedHandler` with a C++ lambda, then `waitUntilCompleted` | the handler saw status 4 (completed) before the wait returned |
| `newLibrary(source, CompileOptions)`; then a source that does not compile | compiles; refused with an `NS::Error`, the program carries on |
| a method the shim lacks (`isHeadless`) | aborts: `limina-metal: -[MTLDevice isHeadless] is not implemented`, exit 134 |
| `thread-exit`: a C++ `thread_local` destructor that uses an autorelease pool at thread exit, after the thread's own pool use | runs |
| `many-buffers`: 5000 buffers of 4 KiB, freed and made again, then axpy across them; then 4500 buffers of 20 MiB made and freed one after another, and axpy in the last | 0 of 6144 and 0 of 5 242 880 wrong |

## Recipe

```
spikes/metal-cpp-shim/build.sh          # out/: metal-cpp, kernels.metallib, liblimina_metal.a, stand-in headers
LIMINA_GPU_METAL=1 cargo xtask run --disk <clone of metal.test>
port=$(scripts/wait-guest-ssh.sh /tmp/limina-worker-<disk>.log)
spikes/metal-cpp-shim/run.sh $port      # installs clang in the guest if missing, builds, runs twice
```

`build.sh` needs Xcode's Metal toolchain and `rustup target add aarch64-unknown-linux-gnu`. The
shim is a Rust static library, so building it needs no Linux linker. The guest links it with
`-lpthread -ldl -lm -lgcc_s`.

## What metal-cpp needs from Linux

- **Four runtime functions and nothing else of the runtime:** `objc_msgSend`, `sel_registerName`,
  `objc_lookUpClass` and `objc_getProtocol`. `objc_msgSend` is 31 instructions
  (`shim/src/runtime.rs`): it saves the argument registers, looks up the method for the receiver's
  class, restores them, and jumps. Arguments, stack arguments and `x8` struct returns stay where
  the caller put them, so each method is written with its real signature.
- **Static initializers name every class and protocol.** metal-cpp's headers look up all 164
  classes and every protocol they name when the program loads. An unknown class becomes a stub class whose
  every message aborts with its name, and protocols are nil, since MLX never checks conformance.
- **`__arm64__` must be defined.** Linux compilers define only `__aarch64__`, and metal-cpp's
  `doesRequireMsgSendStret` (`NSObject.hpp`) stops the build with `#error "Unsupported
  architecture!"` without it. The stand-in `objc/runtime.h` defines it, so no build flag is
  needed. With it, struct returns go through plain `objc_msgSend` in `x8`, as AAPCS64 has them,
  and no `_stret` variant is needed.
- **Stand-in headers** (`include/`):
  - `objc/runtime.h` and `objc/message.h`, where `struct objc_object` must be complete, because
    `NS::Object` derives from it;
  - `CoreFoundation/CoreFoundation.h`, which must bring in `<math.h>`, because metal-cpp uses
    `INFINITY` and gets it from there on Darwin;
  - type-only `dispatch`, `IOSurface`, `mach` and `TargetConditionals.h`.
- **A blocks runtime.** metal-cpp's `addCompletedHandler(std::function)` wraps the function in a
  block that captures a `__block` variable. Copying it needs clang's whole copy/dispose protocol,
  `__block` storage included. The shim carries one (`shim/src/blocks.rs`), after libclosure's
  layout. MLX has to be built with clang and `-fblocks`.
- **Objects** start with `{isa, refcount}`; classes are objects whose `isa` is a metaclass holding
  the class methods. `retain`, `release`, `autorelease` and per-thread `NSAutoreleasePool`s behave
  as Foundation's.

## How the shim manages memory and work

- **Buffers come from a pool** (`shim/src/pool.rs`). Regions of up to 16 MiB are carved out of
  shared 64 MiB arenas, and larger ones get an arena of their own, as KosmicKrisp's bridge does
  with its heaps. Regions are aligned to 256 bytes: the host's heap alignment for the arena
  options is no larger (a 1-byte buffer takes 256). A context has at most 4096 arenas, with ids the guest picks below that, so ids
  are reused, and an arena is destroyed (`ARENA_DESTROY`) once every commit made before its last
  free has completed. When every id is in use or waiting for that, an allocation waits for the
  commits, up to 10 s. A freed region is reused at once: that is Metal's contract with heaps and
  untracked resources, which MLX keeps.
- **`setBytes` data** of a command buffer goes in one region from the pool, which it holds until
  it retires.
- **Command buffers record into their own list**, and commit turns it into ring commands under the
  device's lock: this is the per-command-buffer stream MLX's multithreaded encoding needs. A
  completion thread polls the ring every 50 µs, and frees what retired command buffers held.
- **Each command buffer has an argument table**, destroyed straight after its last commit: the
  host holds a destroyed object until the commits made before the destroy have completed.
- **Each encoder starts with a queue barrier**, which gives Metal 3's ordering between encoders.
  Inside an encoder, dispatches run concurrently and order only at `memoryBarrier`, which is
  what MLX asks for with `DispatchTypeConcurrent`.
- **`newFunctionWithName:` does not check that the function exists.** The GPU host answers a
  pipeline naming a missing one with `PipelineCreate`, while Metal returns nil from
  `newFunction`, which MLX checks.
- **Bound buffers are not retained** by the command buffer.

## Next

MLX builds for Linux against this shim and Apple's metal-cpp: `spikes/mlx-linux/RESULTS.md`.
