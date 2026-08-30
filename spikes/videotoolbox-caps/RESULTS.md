# Hardware video decode for the guest: what each half of the stack can do

Scope: which codecs a Linux guest could get hardware-decoded on a macOS host, and by
what transport. `vt-caps.swift` is the host-side oracle; the guest-side facts below come
from the shipped Fedora RPMs and a live dogfood guest.

## The transport: VA-API over virgl, already wired on the guest side

virglrenderer carries a complete video-acceleration feature — protocol opcodes
(`VIRGL_CCMD_CREATE_VIDEO_CODEC`, `..._DECODE_BITSTREAM`, …), caps
(`struct virgl_video_caps`, `src/virgl_hw.h`), and an implementation in
`src/vrend/virgl_video.c` + `vrend_video.c` — gated behind meson `-Dvideo=true`.
Guest mesa builds `virtio_gpu_drv_video.so` from `src/gallium/targets/va` whenever
`virgl` is in `gallium-drivers`, and libva selects a driver by DRM driver name, which
for virtio-gpu is `virtio_gpu`. So the guest half needs **no limina components at all**.

Stock Fedora 44 aarch64 ships it: `mesa-dri-drivers-26.0.3-4.fc44.aarch64` contains
`/usr/lib64/dri/virtio_gpu_drv_video.so`. (`mesa-va-drivers` was *merged into*
`mesa-dri-drivers`, not dropped — the F44 spec carries
`Obsoletes: mesa-va-drivers < 26.0.0-5` / `Provides: mesa-va-drivers >= 26.0.0-5`.
A repo listing that shows no `mesa-va-drivers-*.rpm` for F44 is that merge, not a removal.)

**Absence degrades gracefully with no flag.** `virgl_get_video_param()` in the guest
driver walks `caps.v2.video_caps[]`; a host advertising `num_video_caps == 0` yields no
profiles. Measured on the dogfood guest (2026-08-30), with our virglrenderer
built *without* `-Dvideo=true`:

```
libva info: Trying to open /usr/lib64/dri/virtio_gpu_drv_video.so
libva info: va_openDriver() returns 0
vainfo: Driver version: Mesa Gallium driver 26.1.6 for virgl (zink Vulkan 1.4(Apple M4 Pro (MESA_KOSMICKRISP)))
vainfo: Supported profile and entrypoints
      VAProfileNone                   : VAEntrypointVideoProc
```

The driver loads and initializes cleanly; the profile list is simply empty. The two-tier
guarantee holds by construction here — there is nothing to gate.

## The host half

`vt-caps.swift`, measured 2026-08-30 on macOS 26.5:

| codec | M1 Max (dev Mac) | M4 Pro (dogfood Mac) |
|---|---|---|
| H.264 | YES | YES |
| HEVC | YES | YES |
| VP9 | YES | YES |
| AV1 | no | **YES** |
| MPEG-2 | no | no |
| MPEG-4 | no | no |
| MJPEG | YES | YES |
| ProRes 422 | YES | YES |

AV1 hardware decode arrives with M3; MPEG-2 and MPEG-4 part 2 have no VideoToolbox
hardware path on Apple silicon at all. The advertised caps must therefore be built from
a runtime query, not a compile-time table.

## The guest codec set is a separate, independent gate

`src/gallium/auxiliary/vl/vl_codec.c` gates codecs in the **VA frontend**, so it applies
to the virtio driver regardless of what the host advertises. Fedora passes no
`-Dvideo-codecs`, taking mesa's `all_free` default (unchanged at 26.0.3 and 26.1.x):

```
patent_codecs = ['vc1dec', 'h264dec', 'h264enc', 'h265dec', 'h265enc']
free_codecs   = ['av1dec', 'av1enc', 'vp9dec', 'mpeg12dec', 'jpegdec']
```

Our enhanced mesa RPM builds Fedora's spec, so it inherits `all_free` too.

Intersecting the two gates gives what a **stock** guest would actually get once a host
backend exists: **AV1 + VP9 + MJPEG** on an M3-or-later host, **VP9 + MJPEG** on M1/M2.
H.264 and HEVC need the guest codec set unlocked, two ways, neither of them new
machinery: `dnf swap mesa-dri-drivers mesa-va-drivers-freeworld` (RPM Fusion, confirmed
to build for F44 aarch64 and to contain `virtio_gpu_drv_video.so`), or building our own
mesa RPM with `-Dvideo-codecs=all`.

## What a host backend would cost

`src/vrend/virgl_video.h` is already a backend abstraction — 14 functions, and the
`virgl_video.c` header comment anticipates non-VA backends. A VideoToolbox
implementation of that header is the core of the job, plus:

- **Bitstream reserialization.** VA-API supplies *parsed* SPS/PPS structs plus slice
  data; `VTDecompressionSession` wants a `CMFormatDescription` (`avcC`/`hvcC`) plus whole
  frames. ffmpeg's `libavcodec/videotoolbox.c` performs exactly this transformation from
  exactly this input shape and is the reference. VP9/AV1 are easier — their slice data
  buffer already carries the frame.
- **Frame delivery.** Today: VA surface → dmabuf → `EGLImageKHR` → GL blit into the
  guest's video-buffer resource (`sync_dmabuf_to_video_buffer`, `vrend_video.c`). Ours:
  CVPixelBuffer → IOSurface → texture, the same interop the zero-copy scanout already does.
- **meson.** `-Dvideo=true` hard-requires `libva` + `libva-drm` + EGL; the VA dependency
  needs to become Linux-conditional.

Decode first. The protocol carries encode too (`virgl_video_encode_bitstream`, H.264/H.265
encode descriptors) and VideoToolbox can do it, but browser playback is the pain.

## No prior art: a VideoToolbox backend would be the first non-VA one

The video feature has only ever had the one backend. `virgl_video.c`'s header floats VDPAU
and "proprietary interfaces" as future options and the file marks itself an unstable API,
but no second backend — VDPAU, DXVA, D3D12 or otherwise — has landed in the four years
since. Nothing published implements one for macOS, and no VideoToolbox-backed libva driver
exists for any purpose. Nor is there public documentation of guest hardware video decode on
a macOS host from UTM, krunkit, Docker Desktop, OrbStack, or Parallels.

So the unstable-API marking is the real cost signal, not the missing backend: we would be
the second consumer of an interface that has had exactly one, and it can shift under us.
Upstream attention since 2022 has gone to venus/vrend, not here. Secondary reporting that
"VirGL is unmaintained" does not survive checking — the mesa 26.1.0 release notes and
`docs/drivers/virgl.rst` carry no such notice.

## Alternatives, and why they lose

- **Vulkan Video.** Nothing in KosmicKrisp (`grep -rn "VK_KHR_video" src/kosmickrisp/` →
  empty) and nothing in venus (`src/virtio/vulkan/`, `src/virtio/venus-protocol/` → empty).
  That is two implementations rather than one, the host half being Vulkan-Video-on-Metal.
  It also serves the wrong consumers: there is no VA-on-Vulkan shim on this path, so
  Firefox and GStreamer — which use VA-API — gain nothing.
- **virtio-video / virtio-media.** Neither is in mainline: `MAINTAINERS` carries 22
  `VIRTIO *` entries and none for media or video, and `drivers/media/virtio/` does not
  exist. Not in Fedora's kernel means enhanced-tier-only, for a device the ecosystem has
  not standardized (virtio-media's OASIS proposal was rejected; it ships as an informal
  spec).
