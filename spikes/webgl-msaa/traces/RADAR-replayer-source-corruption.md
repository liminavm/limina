# Xcode's Metal replayer drops a run of shader source, then the driver crashes on the failure

Filed as FB118532264.

## What happens

Replaying a `.gputrace` captured from a Mesa/KosmicKrisp Vulkan driver fails: 25 calls to
`-[MTL4Compiler newLibraryWithDescriptor:error:]` return errors, after which `GPUToolsReplayService`
takes `SIGSEGV`, `KERN_INVALID_ADDRESS` at `0xe0`, inside

```
AGXMetalG13X  AGX::UserCommonShaderFactory<...>::createVertexProgramVariant(...)
```

reached from `-[AGXG13XFamilyCompiler newRenderPipelineStateWithDescriptor:...]`. A library that
failed to build should surface as an `NSError`, not as a null dereference in the driver.

## The source the replayer compiles is not the source in the trace

The trace on disk is intact. All 14 MSL sources extractable from the bundle compile clean:

```
xcrun metal -std=metal3.2 -c <extracted>.metal      # 14 of 14 succeed
```

The source the replayer feeds the compiler is that same shader with **a contiguous run removed**.
Aligning the replayer's own diagnostics against the stored source, 399 of 400 reported lines match
at a constant **+133 line** shift; the one that does not is the line the cut lands inside.

For the shader in question (67649 bytes stored):

| | |
|---|---|
| dropped | 3242 bytes, from offset 7668 — stored lines 303 … 435 |
| the cut lands mid-line | stored line 436 `    int t682 = int(0);` arrives as `t(0);`, 17 leading bytes gone |
| the compiler's first error | `program_source:303:1: error: C++ requires a type specifier for all declarations` on that `t(0);` |
| the cascade | function bodies lose their signatures, so 5828 statements land at program scope: `error: program scope variable must reside in constant address space` |

Other shaders in the same replay lose different amounts, and always from the *front* of an
identifier — `at4` for `float4`, `ong` for `long`, `ype` for `type` — which is the same defect
landing at other offsets.

Deterministic: two separate replays of the same trace failed identically, same shader, same line,
same text.

## Producing it

The trace is a capture of a Vulkan workload rendering three consecutive multisampled render passes
(1280x720, 4 samples) into one attachment, taken with `MTLCaptureManager` writing to a directory.
The driver is Mesa's KosmicKrisp, which creates every library with
`MTL4LibraryDescriptor.source` set from a plain NUL-terminated string.

macOS 26.5, Xcode 26.4, Apple M1 Max.
