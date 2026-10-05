# virgl: do not offer three-plane 4:2:0 as a decode target

**Bug.** `virgl_is_video_format_supported()` returns `vl_video_buffer_is_format_supported()`
for every profile and entrypoint. That helper only checks that the screen can sample each plane,
so for a decode (VLD) config `vaQuerySurfaceAttributes()` offers NV12, YV12 and I420 alike.
ffmpeg picks a decode surface format by exact match against the stream's software format, so for
8-bit 4:2:0 content it picks I420/YV12 over NV12. Consumers that only handle NV12 surfaces (Firefox,
for one) then refuse the frames and decode in software, while hardware decode is still reported as
available and selected.

r600 (`rvid_is_format_supported`) and nouveau (`nouveau_vp3_screen_video_supported`,
`nv84_screen_video_supported`) return `format == PIPE_FORMAT_NV12` for any real profile. virgl
can't simply copy that, because it also exposes 10-bit decode profiles that need P010/P016, so the
patch withholds only the two three-plane 4:2:0 layouts for `PIPE_VIDEO_ENTRYPOINT_BITSTREAM`.

**Reproducer.** `va-decode-formats.c`: for every VLD config the driver exposes, it prints the
surface pixel formats from `vaQuerySurfaceAttributes()` and fails if YV12 or I420 is among them.

    cc -o va-decode-formats va-decode-formats.c -lva -lva-drm
    ./va-decode-formats

It needs a virgl host that exposes decode: virglrenderer built with `-Dvideo=true`, the VMM
initialising it with `VIRGL_RENDERER_USE_VIDEO`, and a working VA-API decoder on the host.

## Results

| Mesa | Setup | Result |
|---|---|---|
| Fedora 44 `mesa-26.2.3` | QEMU guest, virgl on Intel Iris Plus G7, virglrenderer 1.3.0 | `SKIP: no decode (VLD) profiles` (exit 77); `vainfo` lists only `VAProfileNone/VAEntrypointVideoProc` |
| `main` b39d173ca93 | same | `SKIP: no decode (VLD) profiles` (exit 77) |
| `main` + fix (series tip e09e44d2d0d) | same | `SKIP: no decode (VLD) profiles` (exit 77) |

Measured 2026-10-05.

**Not reproducible on this rig.** The host has libva, iHD and a virglrenderer linked against
libva, but the stock QEMU 10.2 `virtio-gpu-gl-pci` device has no property that enables virgl video
(`-device virtio-gpu-gl-pci,help`), and the guest shows no VLD profile on any Mesa build. The hook
never runs with a decode entrypoint here. Running the reproducer needs a VMM that enables
virglrenderer video.

## MR description (draft)

> **virgl: do not offer three-plane 4:2:0 as a decode target**
>
> `virgl_is_video_format_supported()` defers to `vl_video_buffer_is_format_supported()`, which
> only checks that the planes can be sampled, so decode configs advertise YV12 and I420 next to
> NV12. ffmpeg chooses the decode surface format by exact match with the stream's software format,
> so 8-bit 4:2:0 streams decode into I420/YV12 surfaces, and NV12-only consumers fall back to
> software decoding.
>
> r600 and nouveau return NV12 only for real profiles. virgl also exposes 10-bit decode profiles,
> so this drops just YV12/IYUV for the bitstream entrypoint and leaves everything else as it was.
>
> Reproducer attached: lists `vaQuerySurfaceAttributes()` pixel formats for every VLD config.
> Needs a host with virgl video enabled. I could not run it under QEMU, which does not enable
> virglrenderer video.

## As sent

The commit — message, `Fixes:`, trimmed comments — is on branch `upstream/guest-2026-10` of
`liminavm/mesa`.
