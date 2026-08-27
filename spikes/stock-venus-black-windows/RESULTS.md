# A stock guest's Vulkan windows composite black: vrend cannot type an mtl_shm blob

Measured 2026-08-27 on `Fedora-Workstation-44.stock.test.raw` (kernel `6.19.10-300.fc44`, 4 KiB
pages, stock `mesa-vulkan-drivers-26.0.3-4.fc44`, stock `mutter-50.0-1.fc44`, no limina guest
components), booted with no flags: `cargo xtask run --disk <clone>`.

venus is live on this guest — that is new, and it is what exposed this. Until the 4 KiB stage-2
granule shipped, a 4 KiB guest could not map host-visible blobs at all and had no Vulkan to
present. Nothing regressed; the present path simply ends one step short on the tier that could
never reach it before.

## What happens

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

## Where a fix would go

The stop point is in our own virglrenderer fork, so this is fixable host-side without any guest
component — which is what the two-tier guarantee asks for, since a black window is not "degraded
but usable". The placeholder branch already has a precedent directly below it: a guest-memory blob
with no importable fd is filled from its iovecs rather than declared untypeable. An mtl_shm carrier
is the same shape — host-visible bytes with no IOSurface — and could be sampled the same way.

The guest-side alternatives (upstreaming the WSI knob, or teaching virgl to enumerate modifiers)
are the durable fix for every VMM, but stock guests stay black until distros ship them.

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
