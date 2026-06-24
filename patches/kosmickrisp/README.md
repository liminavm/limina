# patches/kosmickrisp — limina host KosmicKrisp + host-zink patches

KosmicKrisp (KK) is the **host** Vulkan-on-Metal driver (`libvulkan_kosmickrisp.dylib`) that
backs the guest's venus over virglrenderer. The limina enhanced-tier desktop runs
**guest zink → guest venus → virglrenderer (vkr) → KK → Metal → Apple GPU**. This dir carries
our patch series over the KK mesa tree.

Unlike the guest mesa build (`patches/mesa/`, built in the Apple `container`), KK is built
**natively on macOS** from a **case-sensitive APFS volume** `/Volumes/mesa-cs/mesa` (the host
APFS is case-insensitive and can't even check out mesa). The shipped dylib lives at
`target/limina.app/Contents/Frameworks/libvulkan_kosmickrisp.dylib`. See
`docs/drivers/kosmickrisp.rst` for the build recipe.

## Base
- **`UPSTREAM_BASE`** = `178a3d73968` (`egl/gbm: Eliminate local variable "max_age" in
  get_back_bo`). Mesa main; the `/Volumes/mesa-cs/mesa` checkout is pinned here.

## Patches
- **`0001-limina-KosmicKrisp-zink-host-stack-patches-for-the-v.patch`** — the full
  host-tree delta (21 files, ~1340 lines) that builds the shipped KK dylib and the host
  zink-on-KK GL stack. **Recovered 2026-06-24**: it had only ever existed as an
  *uncommitted working tree* in `/Volumes/mesa-cs/mesa` — never committed or exported, the
  enhanced-tier discipline gap (same class of bug as the venus dma-buf patch,
  `patches/mesa/0008`). Now committed on the `limina/kosmickrisp` branch in that tree.
  Components (a follow-up **should split these into logical patches**):
  - **kosmickrisp/vulkan** — encoder + render-pass + bind-cache (`kk_encoder.c`,
    `kk_cmd_buffer.*`), draw path (`kk_cmd_draw.c`, +484), transform-feedback NIR lowering
    (`kk_nir_lower_xfb.c` [new], `kk_shader.*`, `meson.build`), queries (`kk_query_pool.c`),
    external-memory opaque-fd/dma_buf host side (`kk_device_memory.c`,
    `kk_physical_device.c`), pool/descriptor/device.
  - **kosmickrisp/bridge** — Metal encoder/buffer/texture/command-buffer (`mtl_*.m/.h`).
  - **zink** — `driCheckOption` driconf guard on the surfaceless→sw-vk→zink loader path
    (`zink_screen.c`); see `spikes/virgl-zink-kk`.
  - **venus** — `vn_wsi.c` present-region (mirror of `patches/mesa/0005`; incidental in this
    host tree, which builds KK not venus).

## Apply / rebuild
The `/Volumes/mesa-cs/mesa` tree is on the `limina/kosmickrisp` branch with these committed.
To re-create from a fresh checkout: `git checkout 178a3d73968 && git am
patches/kosmickrisp/0001-*.patch`, then build per `docs/drivers/kosmickrisp.rst`.

## TODO
- Split `0001` into logical patches (xfb lowering, encoder/render-pass, external memory,
  zink driconf) for review + upstreaming.
- The `kk_render_encoder` assert (`kk_encoder.c:299`) fires under mutter-50 (F44): a draw
  reaches `kk_render_encoder` with `last_used != KK_ENC_RENDER` and
  `need_to_start_render_pass` false — a render-pass-restart tracking gap. (mutter-49.5 /
  dev-enh never hits it.) Fix tracked separately.
