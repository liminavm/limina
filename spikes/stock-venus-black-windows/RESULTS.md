# A stock guest's Vulkan windows composited black: the blit buffer's bytes went nowhere

Measured 2026-08-27 on `Fedora-Workstation-44.stock.test.raw` (kernel `6.19.10-300.fc44`, 4 KiB
pages, stock `mesa-vulkan-drivers-26.0.3-4.fc44`, stock `mutter-50.0-1.fc44`, no limina guest
components), booted with no flags: `cargo xtask run --disk <clone>`.

venus is live on this guest — that is new, and it is what exposed this. Until the 4 KiB stage-2
granule shipped, a 4 KiB guest could not map host-visible blobs at all and had no Vulkan to
present. Nothing regressed; the present path simply ends one step short on the tier that could
never reach it before.

**Fixed** in our virglrenderer fork (`f68289d8`), guarded by
`crates/limina-test/tests/l2_stock_vulkan_window.rs`. This page records the diagnosis and the
measurements; the fix's own reasoning is in that commit.

## What happened

`vkcube` runs, reports `Virtio-GPU Venus (Apple M1 Max)`, and draws a window of the right size in
the right place. The window is **solid black** (`stock-vkcube-black.png`); the rest of the desktop
composites normally. On the enhanced tier the same command renders the cube
(`enhanced-vkcube-renders.png`).

## The chain, each link observed

**1. mutter advertises every format with `DRM_FORMAT_MOD_INVALID` and nothing else.**
`wayland-info -i zwp_linux_dmabuf_v1` in the guest lists 41 formats, every one of them
`0x00ffffffffffffff = INVALID`. That is not a limina choice: the guest's virgl gallium driver
implements `is_dmabuf_modifier_supported` (which answers "yes" to anything asked) but **not**
`query_dmabuf_modifiers`, so nothing can enumerate a real modifier and the compositor can only
offer the implicit one.

**2. The stock client therefore allocates on the implicit-modifier path.** From `WAYLAND_DEBUG=1`:

| | `zwp_linux_buffer_params_v1.add(fd, plane, offset, stride, mod_hi, mod_lo)` |
|---|---|
| stock | `add(fd 12, 0, 0, **4096**, 16777215, 4294967295)` → modifier **INVALID** |
| enhanced | `add(fd 7, 0, 0, **2000**, 0, 0)` → modifier **LINEAR** |

2000 = 500 × 4 is the image's own row pitch; 4096 is a padded staging row. The enhanced guest
takes the native single-memory LINEAR path because `patches/mesa-guest/0001` sets
`wsi_device::treat_invalid_modifier_as_linear`, which rewrites INVALID to LINEAR in the *client's*
view of the compositor's list. A stock guest by definition does not carry that patch.

The client's staging buffer is `R16G16B16X16_FLOAT` — 8 bytes per pixel, so 500 px is 4000 B and
the 4096 B stride is the padding, not a 4-byte-per-pixel row. Our enhanced guest never allocates
one because the same patch's `block_16f_swapchain_formats` also drops the 16F formats.

**3. Host-side, the export is stripped and given an mtl_shm carrier.** For each of vkcube's three
swapchain buffers the worker logs

    vkr: limina: scanout memory -> stripped OPAQUE/DMA_BUF export, attached mtl_shm carrier
    (size=2097152); present uses the bound image's IOSurface

The comment names the assumption: presentation goes through *the bound image's IOSurface*. That
holds for a LINEAR scanout image. It does not hold for a padded staging buffer, which is bound to
no such image.

**4. So mutter's vrend context cannot type the resource, and gets zeros.**

    vrend_renderer_pipe_resource_set_type: untypeable blob res 235 (fd_type 2, no IOSurface id)
      — placeholder texture, contents will be wrong        (also res 236, res 237)

Three warnings, one per wl_buffer. The enhanced arm logs the opposite for the same app:

    vrend_renderer_pipe_resource_set_type: res 278 adopted IOSurface id 205
      (500x500 PIPE_FORMAT_B8G8R8X8_UNORM)                 (also res 279, res 280)

`vrend_renderer.c`'s limina typing path needs `res->iosurface_id`. An mtl_shm carrier has none, so
it falls to the zeroed placeholder — deliberately, because returning `EINVAL` there poisons the
compositor's context permanently. Black is the designed-for failure, not a crash.

**5. And the buffer really is a buffer.** A probe build logging the allocation's memory type and
its bind target settled the two premises the fix rests on, rather than inferring them from the
2 MiB size:

    [LIMINA-PROBE-ALLOC] size=2097152 typeIndex=0 props=0xf DEVICE_LOCAL HOST_VISIBLE
                         HOST_COHERENT HOST_CACHED
    [LIMINA-PROBE-BIND]  carrier memory -> BUFFER2 (offset=0 iosurf=0x0)

Three of each, one per swapchain buffer. Host-visible is what makes the fix cheap; bound to a
`VkBuffer` is what makes the IOSurface path inapplicable.

## The fix

The carrier's bytes are made real: the memory is host-pointer-imported over the carrier's own
mapping, so the guest's blit lands in the bytes the exported fd carries, and vrend mmaps that fd
and feeds the existing `guest_pixels` upload path (which already re-reads per command batch and
already honours a padded stride). A CPU copy per frame — the enhanced tier keeps the zero-copy
IOSurface scanout, which is what the guest components are for.

`stock-vkcube-fixed.png` is the same stock guest after the fix: the cube renders with correct
colours and no shear, and the frame contains **zero** pure-black pixels, against 490,694 before.

## Where the fix went

The stop point was in our own virglrenderer fork, so this was fixable host-side without any guest
component — which is what the two-tier guarantee asks for, since a black window is not "degraded
but usable". The placeholder branch already had a precedent directly below it: a guest-memory blob
with no importable fd is filled from its iovecs rather than declared untypeable. The mtl_shm
carrier is the same shape — host-visible bytes with no IOSurface — and is now sampled the same
way.

The guest-side alternatives (upstreaming the WSI knob, or teaching virgl to enumerate modifiers)
remain the durable fix for every VMM and would restore zero-copy for a stock guest too — but no
stock guest gets them until distros ship them, which is why the host carries this.

## Scope note

The original sighting was on Debian testing, whose mesa is much newer than Fedora 44's 26.0.3.
This reproduction establishes the class on Fedora; whether Debian's client takes the same
implicit-modifier path has not been checked.

## Reproducing

    cp -c Fedora-Workstation-44.stock.test.raw poke.raw
    RUST_LOG=limina=info,limina_vmm=debug,krun_rutabaga_gfx=debug \
      LIMINA_GLOBAL_SCANOUT=1 LIMINA_WINDOW_CAPTURE=/tmp/scanout.png \
      cargo xtask run --disk poke.raw
    port=$(scripts/wait-guest-ssh.sh /tmp/limina-worker-poke.log)
    ssh -p $port claude@127.0.0.1 \
      'env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 vkcube'

`krun_rutabaga_gfx=debug` is load-bearing: the `untypeable blob` warning is emitted on that
target, so `limina_vmm=debug` alone shows a clean log and the symptom looks sourceless.
