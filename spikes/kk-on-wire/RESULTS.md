# KosmicKrisp on limina's Metal-level protocol, in a Linux guest

Stage 1 of `spikes/metal-heap-addressing/SKETCH.md`: KosmicKrisp in the guest, with its Metal
bridge (`mtl_*`) replaced by an encoder of limina's GPU protocol (`crates/limina-gpu-wire`),
and the host's `limina-gpu-host` making the Metal 4 calls.

Measured 2026-10-12 on an M1 Max with macOS 26.6.2, in a stock F44 guest (`metal-smoke.raw`,
`guest/virtio-gpu-limina` module) booted with `LIMINA_GPU_METAL=1 cargo xtask run`. Mesa was
`/Volumes/mesa-cs/mesa-metal-guest`, branch `metal-native-context` at `d71a3c39f85` (window
surfaces: `08e0f2b62a7`; video planes and imports: `3ce3a0262fe`; counter heaps: `bf03b0d9245`;
X11 presentation: `8a77bbed8ff`).

| check | result |
|---|---|
| `vulkaninfo --summary` / full `vulkaninfo` | `Apple M1 Max`, driver KosmicKrisp, API 1.4.363; the full dump completes with no errors |
| `kk-probe compute` | 256/256 words right |
| `kk-probe triangle` | 2016 green (64·63/2, the top-left rule) and 2080 blue of 64×64, nothing else |
| `kk-probe allocs` | 6000 separately allocated, mapped 4 KiB buffers keep their own contents (past the 4096 arenas a context may have); compute into the last one right |
| `vkcube --wsi wayland` on the seated GNOME desktop | renders correctly (textured, depth-tested; window capture checked by eye), runs 40 s without fault, 600 frames in 9.96 s FIFO (vsync-bound) |
| `vkcube` under stock gnome-shell, windows as surfaces | dma-buf swapchain (`zwp_linux_buffer_params_v1`, XR24, no `wl_shm`), imported by gnome-shell's vrend context with no refusal; renders correctly (capture checked by eye), ~1800 commits in 15 s |
| `vkgears` (mesa-demos) the same way | XR24 surfaces, 60 FPS, renders correctly (capture checked by eye) |
| `kk-probe export` | an image made for dma-buf export with the LINEAR modifier, filled by a buffer copy, sampled texel for texel by stock virgl GL (EGL on gbm) into a texture of its own: 1024/1024 pixels right |
| `kk-probe nv12` | a 100×16 4:2:0 picture from stock virgl VA (`vaPutImage` into DRM_PRIME_2 surfaces, padded to 112 wide), exported as two dma-bufs and imported as a disjoint 2-plane LINEAR image, each plane copied out: 1600/1600 luma and 800/800 chroma bytes right. Needs the host GL from `limina-kk-surfaces` |
| `kk-probe nv12-plain` | the same picture in surfaces made as players make theirs (no DRM_PRIME_2 memory type): its planes become surfaces when Mesa makes it a video buffer, and import: 1600/1600 and 800/800 |
| `kk-probe vadecode` | 60 frames of 854×480 VP9 decoded by ffmpeg's VA hwaccel (stock virgl), each mapped to DRM PRIME as a player maps it: three-plane I420 in three dma-bufs, imported plane by plane and copied out, identical to ffmpeg's own read of the frame: 15 runs, 900/900 frames; 3 runs first thing after a cold boot, 180/180; 1920×1080 (`$KK_CLIP`), 3 runs, 180/180 |
| `mpv --vo=gpu-next --gpu-api=vulkan --hwdec=vaapi` on the seated desktop | 1080p VP9 clip decoded by VA and imported through libplacebo's dma-buf interop: plays 20 s looping, 3 frames dropped, the frame on screen correct (capture checked by eye) |
| decode stays asynchronous with plane surfaces (`VIRGLRS_SUBMIT_STATS=2`, ffmpeg VA decode of 1080p with readback) | END_FRAME costs the render thread 0.10 ms per frame against a 2.03 ms mean decode session; the decode thread's write into the plane surfaces is 0.13 ms mean |
| `kk-probe timestamps` | 64 valid bits; two timestamps around a submit resolve to GPU times in order (counter heaps on the wire, protocol 4) |
| `kk-probe triangle`'s in-pass timestamps | after the vertex stage, then after the fragment stage of the draw: 1.1–1.3 µs apart, in order, 3 runs |
| stock VA decode, planes promoted vs not | GStreamer `vavp9dec` to DMABuf then `vapostproc`, plain `vavp9dec`, and `vp9dec`, 60 frames of 854×480 VP9 each: per-frame checksums identical with every VA surface's planes promoted to surfaces (`limina-kk-surfaces`, virglrs `f70dcb5`) and on the stock host GL, where they keep GL storage |
| `kk-probe import` | a linear XRGB8888 scanout buffer from stock gbm (virgl), written through `gbm_bo_map`, imported as a dedicated LINEAR-modifier image at gbm's pitch: 1024/1024 pixels right |
| stock F44 zink (Mesa 26.1.8) on it, `glxinfo -B` over Xwayland | `zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP))`, GL 3.3 core / compat, GLES 3.1; warns that `EXT_custom_border_color` is missing |
| `glxgears` on that zink over Xwayland, fresh boot | draws (capture checked by eye), ~58 fps vsync-bound, no context refused; `vkcube --wsi xcb` runs 200 frames |
| `kk-probe share` | a buffer exported as dma-buf, dispatched into through a self-import and through an import in a child process: the exporter's mapping shows 256/256 words right for both; the child reads the parent's mark |
| `kk-probe glbuf` | a buffer exported as an opaque fd, imported by zink on this KK (EGL on gbm, GL 3.3 core) through `GL_EXT_memory_object_fd` with buffer storage over it: device UUIDs match; GL reads 1024/1024 words Vulkan wrote, Vulkan reads 1024/1024 GL wrote; 10/10 runs, 0.13 s each |
| buffer sharing across guest drivers (`bufcaps`, `glxinfo`) | KK exports and imports buffers as dma-buf and opaque fd. Stock venus advertises no external memory for a buffer at all, and stock virgl GL has no `GL_EXT_memory_object`: zink on KK is the only GL a KK buffer can reach |
| `guest/limina-gpu-smoke` export latency | 20 shared-arena exports on a quiet desktop, 50 ms apart: worst 7.6–9.2 ms over 3 runs (the guest module kicks the control queue before it waits for the host's map) |
| `kk-probe syncfd` | a submit held by a wait on an unsignaled timeline exports its binary semaphore as a real sync_file: pending before the CPU opens the gate, signaled after, and the fill behind it landed |
| `kk-probe signal` | a submit that only signals a timeline semaphore reaches the host without any later push: the counter reads 1 after 1 ms |
| `glmark2-wayland` on that zink (`MESA_LOADER_DRIVER_OVERRIDE=zink`) | `zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP))` through Wayland EGL; phong scene correct (capture checked by eye); the full default run completes, score 456 at 800×600 (most scenes ~520 fps, terrain 76, refract 202) |

## Recipe

```
spikes/kk-on-wire/kk-build.sh               # container build -> out/libvulkan_kosmickrisp.so
spikes/kk-on-wire/build.sh                  # kk-probe -> out/kk-probe
spikes/kk-on-wire/kk-deploy.sh <port> ./kk-probe     # copy into /tmp/kk in the guest and run
```

`kk-deploy.sh` selects the driver with `VK_DRIVER_FILES`; nothing is installed in the guest.
`vadecode` and the mpv check need a VP9 clip in the guest, made there with
`ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 4 -c:v libvpx-vp9 -deadline realtime
-cpu-used 8 -b:v 4M ~/vab/clip1080.webm` (the 854×480 one is any VP9 WebM at
`~/vab/clip.webm`); the host must run the GL from `limina-kk-surfaces`
(`MESA_PREFIX=<its prefix> LIMINA_HOST_GALLIUM=zink`). For
a window, add `XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0` to the command.

The Mesa side is `-Dkosmickrisp-bridge=limina` (`src/kosmickrisp/bridge/limina/`). A Linux build
also needed:
- the macOS link arguments (`-undefined dynamic_lookup`, `-framework IOSurface`) made darwin-only;
- a placed BO map that is a second `mmap` of the heap's arena (`mtl_heap_map_at`);
- a 20-byte GNU build-id accepted where KK required the 16-byte `LC_UUID`;
- WSI given no KMS display fd (it was handed 0, stdin);
- no `VK_EXT_external_memory_host`, which needs Metal's bytes-no-copy buffers.

**Windows are surfaces the compositor imports.** When the module and host make surfaces (the
bridge tries a 1×1 one at init), WSI runs its native-image path: a window-system image is laid out
linear, and its exported dedicated memory is a texture over a host surface
(`DRM_IOCTL_VIRTGPU_LIMINA_SURFACE_CREATE`, then `CREATE_TEXTURE_FROM_SURFACE`) whose dma-buf
gnome-shell's virgl driver imports as a host3d blob; vrend adopts the same IOSurface. Surfaces
take 32-bit BGRA/RGBA only, so WSI withholds every other format through
`wsi_device::can_share_format` (Wayland only), and a window-system image in another format fails
its allocation: an import the compositor cannot make poisons the compositor's own context, as in
C virglrenderer. Without surfaces the software WSI path (a copy) stays. Implicit sync needs
sync_file import into binary semaphores (a CPU wait) and exports that always return a real fd,
signaled when the value is already reached: WSI passes it to `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`,
which refuses -1.

KosmicKrisp makes a Metal heap per allocation, and a guest context has at most 4096 arenas
(`VIRTGPU_LIMINA_MAX_ARENAS`), while KK on macOS holds ~3.8k heaps on an idle desktop and 28k
after one WebGL open/close. So the bridge carves heaps of up to 16 MiB out of shared 64 MiB
arenas (larger ones get their own), and the guest module maps an arena from any page inside it,
which a mapped pooled heap needs. The guest module must be at least that version
(`scripts/build-virtio-gpu-limina.sh`; on F44 it loads from `/lib/modules/<krel>/updates/`).

Memory allocated for export or imported from an fd is a heap of its own, shared through DRM:
the bridge makes it a shared arena (`VIRTGPU_LIMINA_ARENA_SHARED`), which the module exports as a
host-visible blob GEM object; PRIME carries it to other processes, and `ARENA_IMPORT` there gives
the same Metal heap at the same GPU address. Both sides map it through the GEM object. Importing
what the same file exported returns the same GEM handle, so the bridge counts its holders. The
transport alone is checked by `guest/limina-gpu-smoke` (its share step).

zink needed three more things from the bridge. KK releases depth-stencil states and meta pipelines
while still recording, as Metal encoders retain what they are set, so a command buffer holds every
object its stream names until it is recorded again: otherwise the DESTROY reaches the ring before
the commit that uses the object, and the host drops the context with `BadHandle`. And KK reports
the render node it opened through `VK_EXT_physical_device_drm`, which is how zink's EGL path picks
its device. And its EGL DRM screen requires `VK_KHR_external_memory_fd`, without which Wayland EGL
fell back to llvmpipe. Run zink with `MESA_LOADER_DRIVER_OVERRIDE=zink`.

Fences come from the guest module: `DRM_IOCTL_VIRTGPU_LIMINA_FENCE` makes a `dma_fence` for a
commit seqno of the context's ring (the host holds a fenced control-queue command until the ring
completes it), adds it to GEM reservations, and returns it as a sync_file. KosmicKrisp's binary
semaphores export through it: a queue signal of a shared event is noted with the commit it
follows, and the export is a fence for that commit. Where that commit is not known (a queue wait
was pushed since the last commit) the export still waits on the CPU, as the runtime's generic
one always did. `guest/limina-gpu-smoke` checks the kernel and host halves alone.

Anything a wait on another thread may be polling for must be published when it is pushed, not
left for a later flush: KosmicKrisp signals a submit's timeline semaphores after its last commit,
and zink waits on its batches from another thread, so an unpublished signal hung glmark2's
buffer scene with guest and host both idle. Queue events now publish like commits.

## Open

- **Implicit sync is up to userspace.** The bridge attaches no fence to a dma-buf by itself; the
  WSI and zink do, from exported sync_files (`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`). Importing a
  sync_file into a KK semaphore still waits on the CPU (the host cannot wait on a guest fence).
- The worker answers fences by polling ring control blocks every 100 µs while one is pending;
  a wake posted by the GPU host would remove the poll.
- **Only windows cross to the compositor.** A non-window exported KK heap is still a blob the
  extension owns, invisible to virglrs. Windows in RGBA16F or 10-bit formats are withheld until
  surfaces take them; X11 WSI's format list is not filtered.
- **X11 presents native images because KK says the X server's device is its own.** Mesa's
  `wsi_device_matches_drm_fd` matches PCI devices only; answered no, X11 WSI presents through an
  exported buffer, a heap blob that poisons Xwayland's and gnome-shell's contexts. KK compares
  the DRI3 fd's sysfs device with its render node's instead.
- **Which exports other drivers can read.** Any dedicated image whose layout is linear (LINEAR
  tiling or the LINEAR modifier, the portable ways to share with another driver) in a 32-bit
  BGRA/RGBA single-level 2D shape is a surface. An OPTIMAL-tiled export has no layout another
  driver could learn, so it stays a KK heap, shared between KK contexts only.
- **A surface cannot be CPU-mapped through virgl.** `gbm_bo_map` of an imported surface reads
  through a ctrl-queue transfer into guest pages it does not have (`transfer_read: ... no pages
  attached`), as for any non-mappable host3d blob, venus exports included. GPU sampling works.
  Mappable surfaces would need the IOSurface's pages in the guest-visible region.
- **Buffers are shared with KK and zink on it, nobody else.** Stock venus and virgl have no
  buffer sharing to meet KK with (`bufcaps`); KK's own dma-buf and opaque-fd buffers serve
  Vulkan↔GL interop through zink, which is the case applications have.
- **Mesa installs the external-objects entry points from GLES 3.2** while advertising
  `GL_EXT_memory_object` from GLES 2.0 (`src/mesa/glapi/glapi/gen/EXT_external_objects.xml`), so
  in zink-on-KK's GLES 3.1 every call is a no-op `invalid call`; desktop GL works. An upstream
  item.
- **Imports are images only.** A dma-buf from vrend or venus imports as a dedicated image over
  its surface, single-plane or a 4:2:0 picture shared one dma-buf per plane; as a buffer, or a
  picture in one dma-buf (enhanced guest Mesa's composite decode target), it is still refused.
- **Every VA surface's planes are IOSurfaces** on a host GL that takes them: vrend promotes an
  R8/RG8 plane at CREATE_VIDEO_BUFFER, whatever its binds, since players do not ask for
  exportable memory. That is one IOSurface per plane of every pool slot of every stock guest's
  decoder. The decode thread writes each picture into its planes' surfaces before the picture
  lands, so the guest's fence covers the write and END_FRAME does not wait for it.
- **An imported surface is not CPU-visible.** Its memory type is device-local only (mapping it
  would show the dma-buf's guest backing, not the host's pixels); an imported KK heap stays
  mappable.
- **A timestamp resolve is ordered by one device-wide fence.** No barrier reaches a resolve, so
  an encoder that wrote a timestamp updates the fence as it ends, the resolve waits on it and
  updates it, and the encoder after it waits on it. Metal's own bridge resolves with no fence.
- **One wrong frame, not reproduced.** An early `vadecode` run on a fresh worker read one frame
  (of 10) wrong in 839 of 960 rows. The picture then reached the surfaces by a GL upload on the
  render thread, ordered against KK's Metal queue only by the guest's fence; that path is gone,
  and no frame has read wrong since.
- **GStreamer's VA decoders hand DMABuf only to `vapostproc`**: a generic sink does not
  negotiate it, so the import is tested through ffmpeg, which is what players use.
- **Hardware VP9 decode is not bit-exact against software**, with or without plane surfaces:
  plain `vavp9dec` differs from `vp9dec` in 30 of 60 frames of the test clip, and a DMABuf
  decode through `vapostproc` in all 60.
- **A bad client import ends the compositor's context.** vrend poisons a context on a SET_TYPE of
  a resource it does not know; the format filter keeps KK from causing one on Wayland, and X11
  is held to the formats surfaces take only by what X visuals offer.
- **Every dma-buf memory-properties query asks the host whether it is a surface**, and for a
  heap the worker logs `LiminaSurfaceImport -> ErrInvalidResourceId` at WARN: an expected answer
  in an error's clothing.
- No DRM format modifiers: a dma-buf image means KosmicKrisp's own layout, so only another
  KosmicKrisp context can make sense of it.

- Completion and reply waits poll; an idle guest process with a pending commit burns CPU.
- `[LIMINA-KK-GUARD]` prints its periodic encoder-guard line, which means nothing on this
  transport.
- Unimplemented (abort loudly): timestamps and counter heaps, sparse, IOSurface import, drawables.
- Delivery: the Mesa branch sits on the host `limina-kk` base; a guest package needs it carried
  onto the `limina-guest` base.
