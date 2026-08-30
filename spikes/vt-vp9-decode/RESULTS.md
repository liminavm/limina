# VideoToolbox can stand in for a VA-API VP9 decoder

The question this had to answer before any of it was wired into virglrenderer: the virgl
video protocol hands the host one VP9 frame at a time and expects a decoded picture back
for **every** frame — including the hidden alt-refs (`show_frame=0`) that a later
`show_existing_frame` displays. A backend that drops those decodes most streams into
garbage. VideoToolbox does not drop them.

`./fetch-sample.sh && cc -O2 -o vt-vp9-decode vt-vp9-decode.c -framework VideoToolbox
-framework CoreMedia -framework CoreVideo -framework CoreFoundation && ./vt-vp9-decode
hidden-frames.ivf`, measured 2026-08-30 on macOS 26.5 / M1 Max:

```
sub-frames after split : 107
  hidden (show_frame=0): 7
submitted to VT        : 107
output callbacks       : 107
  arrived after return : 0  (0 = fully synchronous)
  with real pixels     : 107
  errors               : 0
pixel format           : 420v  352x240
```

## What that buys the backend

- **1:1 in, 1:1 out.** Every submitted frame produces exactly one output frame carrying
  real pixels — the luma is read back and checked to vary, since "a buffer came back" is
  not "something decoded".
- **Synchronous in ORDER, but not on the caller's thread.** With decode flags `0` the
  output callback always fires *before* `VTDecompressionSessionDecodeFrame` returns
  (`arrived after return: 0`), so exactly one picture is outstanding per frame and the
  backend needs no async plumbing and no `WaitForAsynchronousFrames`. It nonetheless runs
  on one of VideoToolbox's own threads (`callback thread: DIFFERENT from caller`). **Do
  not do GL work there.** That thread has no EGL context current, so every call is
  silently dropped — Mesa logs `called without a rendering context`, `glGetError` returns
  0 because it too is a no-op, and the guest reads an untouched surface. Park the
  CVPixelBuffer in the callback and use it after `DecodeFrame` returns.

  Ordering is not thread identity: the first version of this spike measured the former and
  concluded the latter, and the backend built on that reported success while uploading
  nothing.
- **VideoToolbox owns the DPB.** Nothing here tells it about reference frames, and inter
  frames decode correctly regardless. The whole `desc->ref[16]` apparatus the VA-API
  backend has to maintain is dead weight on this path — feed frames in decode order and
  it tracks its own state.
- **Output is `420v`** — 8-bit biplanar NV12, video range — which is what the guest's
  video buffer wants, so the planes copy across with no conversion.

## Traps this cost time on

- **`ffmpeg -c:v libvpx-vp9 -auto-alt-ref` produced no hidden frames at any setting
  tried** (`-auto-alt-ref 1` and `6`, `-lag-in-frames 25`, long GOP, motion-rich source).
  A clip encoded that way contains zero superframes and silently tests only the easy path.
  libvpx's own `vp90-2-09-aq2` vector has 7 superframes carrying 7 hidden frames, so
  `fetch-sample.sh` uses it. **Verify a VP9 clip actually has superframes before drawing
  conclusions from it** — the marker is a trailing byte matching `0b110xxxxx` that repeats
  at the start of the index.
- **`VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_VP9)` is required
  first.** Without it `VTIsHardwareDecodeSupported(VP9)` answers no on hardware that has
  it, and the session create fails.
- VideoToolbox needs a `vpcC` box in the format description's
  `SampleDescriptionExtensionAtoms` — 4 bytes of FullBox header then the 8-byte VP9
  configuration record. `make_vpcc()` here is the whole of it; profile, bit depth and
  chroma subsampling all come straight out of the virgl VP9 picture descriptor.
