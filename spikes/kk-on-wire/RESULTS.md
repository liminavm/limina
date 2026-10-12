# KosmicKrisp on limina's Metal-level protocol, in a Linux guest

Stage 1 of `spikes/metal-heap-addressing/SKETCH.md`: KosmicKrisp in the guest, with its Metal
bridge (`mtl_*`) replaced by an encoder of limina's GPU protocol (`crates/limina-gpu-wire`),
and the host's `limina-replayer` making the Metal 4 calls.

Measured 2026-10-12 on an M1 Max with macOS 26.6.2, in a stock F44 guest (`metal-smoke.raw`,
`guest/virtio-gpu-limina` module) booted with `LIMINA_GPU_METAL=1 cargo xtask run`. Mesa was
`/Volumes/mesa-cs/mesa-metal-guest`, branch `metal-native-context` at `026f0c12954`.

| check | result |
|---|---|
| `vulkaninfo --summary` / full `vulkaninfo` | `Apple M1 Max`, driver KosmicKrisp, API 1.4.363; the full dump completes with no errors |
| `kk-probe compute` | 256/256 words right |
| `kk-probe triangle` | 2016 green (64·63/2, the top-left rule) and 2080 blue of 64×64, nothing else |
| `vkcube --wsi wayland` on the seated GNOME desktop | renders correctly (textured, depth-tested; window capture checked by eye), runs 40 s without fault, 600 frames in 9.96 s FIFO (vsync-bound) |

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

## Open

- **Arena ids run out before zink does anything big.** Every `kk_alloc_bo` is one heap and one
  arena, and `VIRTGPU_LIMINA_MAX_ARENAS` is 4096. On macOS, KK's own BO census counts ~3.8k heaps
  on an idle desktop and 28k after one WebGL open/close. A GL desktop on zink will exhaust ids,
  and the failure will read as out of device memory. Decide between sub-allocating BOs from
  large arenas and a much larger cap before starting zink.
- Completion and reply waits poll; an idle guest process with a pending commit burns CPU.
- `[LIMINA-KK-GUARD]` prints its periodic encoder-guard line, which means nothing on this
  transport.
- Unimplemented (abort loudly): timestamps and counter heaps, sparse, IOSurface import, drawables.
- Delivery: the Mesa branch sits on the host `limina-kk` base; a guest package needs it carried
  onto the `limina-guest` base.
