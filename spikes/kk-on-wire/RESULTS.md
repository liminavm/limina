# KosmicKrisp on limina's Metal-level protocol, in a Linux guest

Stage 1 of `spikes/metal-heap-addressing/SKETCH.md`: KosmicKrisp in the guest, with its Metal
bridge (`mtl_*`) replaced by an encoder of limina's GPU protocol (`crates/limina-gpu-wire`),
and the host's `limina-replayer` making the Metal 4 calls.

Measured 2026-10-12 on an M1 Max with macOS 26.6.2, in a stock F44 guest (`metal-smoke.raw`,
`guest/virtio-gpu-limina` module) booted with `LIMINA_GPU_METAL=1 cargo xtask run`. Mesa was
`/Volumes/mesa-cs/mesa-metal-guest`, branch `metal-native-context` at `f85978f1f6f`.

| check | result |
|---|---|
| `vulkaninfo --summary` / full `vulkaninfo` | `Apple M1 Max`, driver KosmicKrisp, API 1.4.363; the full dump completes with no errors |
| `kk-probe compute` | 256/256 words right |
| `kk-probe triangle` | 2016 green (64·63/2, the top-left rule) and 2080 blue of 64×64, nothing else |
| `kk-probe allocs` | 6000 separately allocated, mapped 4 KiB buffers keep their own contents (past the 4096 arenas a context may have); compute into the last one right |
| `vkcube --wsi wayland` on the seated GNOME desktop | renders correctly (textured, depth-tested; window capture checked by eye), runs 40 s without fault, 600 frames in 9.96 s FIFO (vsync-bound) |
| stock F44 zink (Mesa 26.1.8) on it, `glxinfo -B` over Xwayland | `zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP))`, GL 3.3 core / compat, GLES 3.1; warns that `EXT_custom_border_color` is missing |
| `glxgears` on that zink | renders correctly (capture checked by eye), ~240 fps |
| `kk-probe share` | a buffer exported as dma-buf, dispatched into through a self-import and through an import in a child process: the exporter's mapping shows 256/256 words right for both; the child reads the parent's mark |
| `glmark2-wayland` on that zink (`MESA_LOADER_DRIVER_OVERRIDE=zink`) | `zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP))` through Wayland EGL; phong scene correct (capture checked by eye); build 410 fps, texture 502 fps |

## Recipe

```
spikes/kk-on-wire/kk-build.sh               # container build -> out/libvulkan_kosmickrisp.so
spikes/kk-on-wire/build.sh                  # kk-probe -> out/kk-probe
spikes/kk-on-wire/kk-deploy.sh <port> ./kk-probe     # copy into /tmp/kk in the guest and run
```

`kk-deploy.sh` selects the driver with `VK_DRIVER_FILES`; nothing is installed in the guest. For
a window, add `XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0` to the command.

The Mesa side is `-Dkosmickrisp-bridge=limina` (`src/kosmickrisp/bridge/limina/`). A Linux build
also needed:
- the macOS link arguments (`-undefined dynamic_lookup`, `-framework IOSurface`) made darwin-only;
- a placed BO map that is a second `mmap` of the heap's arena (`mtl_heap_map_at`);
- a 20-byte GNU build-id accepted where KK required the 16-byte `LC_UUID`;
- WSI given no KMS display fd (it was handed 0, stdin);
- the software WSI path (`sw_device`) and no `VK_EXT_external_memory_host`, which needs Metal's
  bytes-no-copy buffers.

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

## Open

- **No fences on shared memory.** A dma-buf of a KK heap carries no implicit or explicit fence;
  a consumer in another process can read before the producer's GPU work lands. The guest module
  is where `dma_fence`s for these objects belong.
- **The compositor cannot import a KK buffer.** An exported KK heap is a blob the extension owns,
  invisible to virglrs, so gnome-shell (vrend/venus) cannot take it as a client buffer; windows
  present through the software WSI (a copy). Zero-copy presentation needs the host to register
  extension blobs with the renderer, or a compositor on this transport.
- No DRM format modifiers: a dma-buf image means KosmicKrisp's own layout, so only another
  KosmicKrisp context can make sense of it.

- Completion and reply waits poll; an idle guest process with a pending commit burns CPU.
- `[LIMINA-KK-GUARD]` prints its periodic encoder-guard line, which means nothing on this
  transport.
- Unimplemented (abort loudly): timestamps and counter heaps, sparse, IOSurface import, drawables.
- Delivery: the Mesa branch sits on the host `limina-kk` base; a guest package needs it carried
  onto the `limina-guest` base.
