# Radar draft — AGX driver aborts the client process on a render pass its own validation layer rejects

Draft for submission to Apple (Feedback Assistant, macOS → Metal). Written 2026-08-15; the user
asked for it once the trigger was identified, and `mtlrp-min.m` is that trigger.

---

**Title:** Metal aborts the client process from MTLCompilerService on an attachment-less render
pass with `defaultRasterSampleCount == 0` (AGXMetalG13X)

**Area:** macOS → Metal
**Reproducibility:** Always (Apple M1 Max / macOS 26.5 and Apple M4 Pro / macOS 26.5.2).

## Summary

Creating a render command encoder from an `MTLRenderPassDescriptor` that has **no attachments**
and leaves `defaultRasterSampleCount` at its default of **0** causes `MTLCompilerService` to
assert and the **calling process to die with SIGABRT**:

```
AGCLLVMObject::readBitcode(...) bitcode_url is NULL for bundle 'com.apple.AGXCompilerCore',
filename '<private>', extension 'ds'
```

The failing frame in `MTLCompilerService`'s crash report is
`AGCLLVMBackgroundObjectFragmentShader(AGCLLVMCtx&, LLVMContext&, _AGCDrawBufferState const*,
_AGCBackgroundObjectState const*)` → `buildStateless` → `readBitcode`. The request type is
`MTLBuildOpaqueRequest - opaque`, i.e. the driver's own background-object compile, not
application shader compilation. The client dies after Metal's XPC retries.

## The descriptor is invalid, and Metal already knows it

With `MTL_DEBUG_LAYER=1` the validation layer rejects it cleanly and precisely:

```
_MTLDebugValidateRenderPassDescriptorAndTrackAttachments:579: failed assertion
`RenderPass Descriptor Validation
no sampleCount for color and raster available, either set defaultColorSampleCount or set
defaultRasterSampleCount or set appropriate attachments'
```

So this is not a request for the driver to accept the descriptor. **The report is that the
release path aborts the host process where the validation path diagnoses the problem.** An
invalid descriptor should fail the encoder creation — the way a nil-returning
`renderCommandEncoderWithDescriptor:` already does for other invalid inputs — rather than assert
inside a helper process and take down a client that has no way to catch it.

This matters for any process that builds Metal work from untrusted or generated input. In our
case a Vulkan-on-Metal driver derived the descriptor from guest-supplied state, so a guest could
terminate the host VM process. There is no API surface to defend with: the abort happens inside
the framework, on a thread the caller does not own.

## Steps to reproduce

`mtlrp-min.m` (attached, ~40 lines, no third-party dependencies):

```
clang -O1 -g -fobjc-arc -o mtlrp-min mtlrp-min.m -framework Metal -framework Foundation
./mtlrp-min        # no attachments, defaultRasterSampleCount = 0  -> SIGABRT (exit 134)
./mtlrp-min 4      # same through MTL4                             -> SIGABRT (exit 134)
./mtlrp-min 1      # identical but defaultRasterSampleCount = 1    -> exits 0
```

The essential part is three statements:

```objc
MTLRenderPassDescriptor *rp = [MTLRenderPassDescriptor renderPassDescriptor];
rp.renderTargetWidth = 800;
rp.renderTargetHeight = 600;
// no attachments, and defaultRasterSampleCount deliberately left at 0
id<MTLRenderCommandEncoder> enc = [cb renderCommandEncoderWithDescriptor:rp];
```

## Expected vs actual

- **Expected:** encoder creation fails (nil encoder, or an `MTLCommandBufferError`), the way the
  validation layer indicates.
- **Actual:** `MTLCompilerService` asserts; the calling process is killed with SIGABRT.

## Notes

- Both the classic `MTLRenderPassDescriptor` path and the `MTL4RenderPassDescriptor` path abort.
- Sample counts 1 and 2 are accepted for an attachment-less pass; only 0 is fatal.
- **Not generation-specific.** Confirmed on both **Apple M1 Max (AGXMetalG13X), macOS 26.5** and
  **Apple M4 Pro, macOS 26.5.2** — identical behaviour on each (`0`/`4` → SIGABRT, `1`/`2` → exit
  0). The `ds` resource families in `AGXCompilerCore.framework` are GPU-generation-keyed (`_g13`,
  `_g14`, `_g15*`, `_g16p`, `_hal200/300`), but the missing background-object bitcode is common to
  the generations tested.

## Attachments

- `mtlrp-min.m` — minimal reproducer
- `MTLCompilerService-*.ips` — the compiler service crash report naming the failing frame
