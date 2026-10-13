# The Metal-level transport: delivery and interop

How the guest drivers of limina's third GPU transport (the Metal-level protocol, the GPU host and
`guest/virtio-gpu-limina`; see `spikes/metal-heap-addressing/SKETCH.md`) reach guests, and how
their buffers meet those of the other two transports, vrend and venus. What the transport does
today, measured, is in `spikes/kk-on-wire/RESULTS.md`.

## 1. Vendor drivers

Both guest drivers ship as **vendor drivers**: the Vulkan driver (KosmicKrisp on the limina
bridge) and the GL driver (zink over it first, an own GL-to-Metal later). Neither replaces the
system's Mesa; they are installed beside it, and nothing selects them through the environment.

- **One build for every distro.** `scripts/build-gpu-drivers.sh` builds the `[kosmickrisp-guest]`
  Mesa with `-Dlimina-vendor=true` on the oldest glibc we support (Ubuntu 24.04's 2.39,
  `guest/gpu-drivers/Containerfile`), links the C++ runtime statically, and refuses an artifact
  that needs a newer glibc or a library outside the short list every desktop distro has (glibc,
  libdrm, expat, zlib, zstd, udev, wayland, xcb/X11, libgbm, the Vulkan loader, glvnd).
- **A private tree per protocol: `/opt/limina/<wire>/`**, `<wire>` being the protocol the
  drivers speak to the host. Drivers for several hosts sit side by side; each declines a host
  that speaks another. The tree carries its own `libgallium` beside the libraries that need it
  (RUNPATH `$ORIGIN`), never the distro's.
- **Found where the system's loaders look**, through links the installer makes:
  - EGL: a glvnd vendor, `/etc/glvnd/egl_vendor.d/10_limina-<wire>.json`, ahead of Mesa's `50_`.
  - Vulkan: an ICD, `/etc/vulkan/icd.d/limina-<wire>.json`.
  - GBM: `virtio_gpu_gbm.so` in the system libgbm's backend directory. libgbm tries a backend
    named after the device's DRM driver before its built-in one, so ours is asked first about
    every virtio-gpu. It loads one backend per name, so the newest protocol's takes it.
  - GLX is not provided: Xwayland names Mesa as the GLX vendor, so X11 GL clients stay on the
    stock drivers.
- **Selection is per device, by declining.** Every entry point asks the module what the host
  speaks (`DRM_VIRTGPU_LIMINA_INFO`, one ioctl, no host context) and declines a device that is
  not a limina transport speaking its protocol, so the stock drivers take it:
  - the DRI loader answers zink for such a device and nothing for any other, ahead of
    `MESA_LOADER_DRIVER_OVERRIDE`, which a system Mesa's configuration may set for itself;
  - EGL declines at vendor init when no such device exists, and declines a GBM display its own
    backend did not make;
  - the GBM backend declines other devices, and names its own `limina`, not `drm`: a system EGL
    takes a `drm` device for its own and would read our device structure as one of its own;
  - KosmicKrisp enumerates no device.
  A guest mid-upgrade, without the module or on a host without the transport, keeps working.
- **Vulkan coexistence.** The loader enumerates every ICD, so a guest with both venus and the
  limina ICD shows two devices of one GPU. zink in the vendor build takes only KosmicKrisp's.
  Applications still see both until a small implicit layer hides venus's device where ours is
  present.
- **The module ships through the distro's DKMS**, one package carrying the in-tree driver's
  sources for every kernel series we support, each patched with the extension
  (`guest/gpu-drivers/dkms-make.sh` picks by the kernel's major.minor). A series from our own
  kernel fork is vendored from the fork, which carries virtio-gpu commits of ours. A kernel with
  no series fails the build and keeps its in-tree driver. The same path serves the enhanced tier:
  its kernel builds virtio-gpu as a module (everything else boot needs stays built in) and ships
  a `-devel` package beside it, installed in the same transaction so DKMS can build for it as it
  lands. A kernel with virtio-gpu built in gets the drivers without the module, and so without
  the transport.
- **The installer is the distro-agnostic part of the guest tools** (`guest/gpu-drivers/install.sh`,
  packaged by `scripts/package-gpu-drivers.sh`). What differs per distro is confined to the
  prerequisites' package names and the initramfs tool; everything it installs is recorded in a
  manifest that `--uninstall` and upgrades read. A Vulkan driver list pinned in the environment
  (the RPM enhanced tier pins venus) gains our ICDs rather than being replaced.

### Flatpak

Flatpak takes GL and Vulkan drivers from runtime extensions of `org.freedesktop.Platform.GL`
(`GL32` for 32-bit), mounted under `/usr/lib/<arch>/GL/<name>/`, where libglvnd and the Vulkan
loader find their vendor files. A limina extension, `org.freedesktop.Platform.GL.limina`, is a
complete Mesa (stock drivers plus ours) chosen with `FLATPAK_GL_DRIVERS`, as `mesa-git` is, built
once per live runtime branch and published from a limina remote the guest tools add. The guest
tools set `FLATPAK_GL_DRIVERS` system-wide on the enhanced tier.

To verify before building: whether `FLATPAK_GL_DRIVERS` replaces or adds to `default`, the
runtime's `merge-dirs`, and whether `GL.default` stays mounted beside a named extension.

## 2. The compositor

On the enhanced tier gnome-shell (mutter) runs on the vendor GL driver, so the compositor and KK
clients are on one transport: client buffers are extension blobs it imports as dma-bufs, and
fences are the module's. That needs, beyond the driver:

- **GBM and scanout on the transport.** Mutter allocates its outputs through GBM and page-flips
  them; those become extension blobs, and the host must present them (SET_SCANOUT/flush of an
  extension blob, through the surface publisher like vrend's and venus's scanouts).
- **A layout both ends agree on.** A modifier for KosmicKrisp's own layout for same-transport
  sharing, and LINEAR for buffers that cross transports (§3).

The stock tier keeps gnome-shell on vrend, unchanged.

## 3. Interop across the three transports

Any transport's shareable buffers can be imported by any other, so stock clients (Flatpaks without
the extension, anything on stock Mesa) work under an enhanced compositor and the other way round.

- **The transport stays out of virglrs.** The GPU host remains its own process; interop is a
  resource-level matter, not a shared renderer.
- **The currency is the IOSurface, handed over by Mach port**, as worker → supervisor surfaces
  already are (`docs/graphics.md` §4). Only IOSurface-shareable formats in LINEAR layout cross
  transports (host KK implements `VK_EXT_image_drm_format_modifier` LINEAR-only over exactly
  those). Heap-placed images in KK's own layout stay within the Metal transport.
- **The worker keeps the registry**: virtio-gpu resource id → its IOSurface, whichever renderer
  made it. virglrs already registers the surfaces it mints (`metal::republish`).
- **Metal transport → vrend/venus — built for windows.** A window-system image is a dedicated
  texture over a *surface*: the guest module's `SURFACE_CREATE` makes a host3d resource the worker
  backs with an IOSurface minted by virglrs (`Renderer::resource_create_surface`), whose Mach port
  goes to the GPU host (`CREATE_TEXTURE_FROM_SURFACE`). vrend adopts the IOSurface when a context
  types the resource; venus imports it as it does its own scanouts. Surfaces take 32-bit
  BGRA/RGBA, and the guest driver offers the window system nothing else
  (`wsi_device::can_share_format`), since an import the compositor cannot make ends its context.
  Wider formats (RGBA16F, 10-bit) come with HDR and deep colour (`docs/roadmap.md` M15 wave 7),
  which adds them to surfaces, vrend's adoption and the guest driver's list together.
- **vrend/venus → Metal transport — built for images.** A guest driver importing a dma-buf as a
  dedicated image asks the module what surface its resource is (`SURFACE_IMPORT`); the worker
  resolves the resource to the IOSurface it is presented from (`Renderer::resource_surface_port`:
  vrend's SCANOUT/SHARED textures, venus exports, the transport's own surfaces), binds its port to
  the context, and the GPU host makes a texture over it. Each context importing a resource is a
  holder of its own, released when the resource goes or the context detaches it. A surface may be
  larger than the image over it (a decoder pads its planes); pitch and format must match.
- **Video pictures cross plane by plane.** A stock VA driver exports a 4:2:0 picture as two
  SHARED resources, R8 luma and R8G8 chroma, in a dma-buf each. vrend backs each with a plain
  one- or two-channel IOSurface (`'L008'`, `'2C08'`; DRM R8 and GR88), which needs a host GL that
  imports them and names their sized internal format (`limina-kk-surfaces`, not yet the
  `[kosmickrisp]` pin; a host GL without it keeps the planes in GL storage). The guest driver
  imports the picture as a disjoint `G8_B8R8_2PLANE_420` image at the LINEAR modifier, matching
  each object's surface to its plane (NV12, or three-plane I420 as ffmpeg's VA frames come).
  Players make no SHARED planes, so vrend also promotes every video buffer's R8/RG8 planes to
  surfaces at CREATE_VIDEO_BUFFER, when they are fresh, and the decode thread writes each
  picture into them before it lands. An imported surface's memory is device-local only. A
  single-object picture (enhanced guest Mesa's composite
  target) and 10-bit P010 wait on a plane index in `CREATE_TEXTURE_FROM_SURFACE`, with HDR.
- **Buffers stay within the Metal transport.** A KK buffer exports and imports as a dma-buf or an
  opaque fd between KK contexts, and so reaches GL through zink on KK (`GL_EXT_memory_object_fd`).
  Stock venus offers no external memory for buffers and stock virgl GL no memory objects, so no
  other transport has a buffer to meet it with.

To verify before building: that an IOSurface's pages can back a guest-visible mapping of an
extension blob the way venus's IOSurface blobs do (rutabaga's Apple `get_map_ptr` path).

## 4. Fences across transports

The module's fences (`DRM_IOCTL_VIRTGPU_LIMINA_FENCE`) carry Metal-transport work to every guest
`dma_fence` consumer. The other direction, a Metal-transport queue waiting on a guest fence
(another transport's, or an imported sync_file), belongs in the module too: a `dma_fence` callback
that, when the fence signals, has the host set the waiting context's shared event, so the GPU
queue's wait releases without a guest thread blocking. Until then a sync_file import into KK waits
on the CPU.
