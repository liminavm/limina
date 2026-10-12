# The Metal-level transport: delivery and interop

How the guest drivers of limina's third GPU transport (the Metal-level protocol, the replayer and
`guest/virtio-gpu-limina`; see `spikes/metal-heap-addressing/SKETCH.md`) reach guests, and how
their buffers meet those of the other two transports, vrend and venus. What the transport does
today, measured, is in `spikes/kk-on-wire/RESULTS.md`.

## 1. Vendor drivers

Both guest drivers ship as **vendor drivers**: the Vulkan driver (KosmicKrisp on the limina
bridge) and the GL driver (zink over it first, an own GL-to-Metal later). Neither replaces the
stock path; they are added beside it.

- **System-wide, enhanced tier.** An RPM next to the enhanced Mesa: the Vulkan ICD, and a Mesa
  build carrying zink-on-KK as an EGL/GLX vendor. The stock tier never installs them.
- **Selection is per feature.** A driver lights up only when its own prerequisites are there: the
  guest module with the extension negotiated (`VIRTIO_GPU_F_LIMINA_EXT`). Without them the loader
  falls back to the stock drivers, so a guest mid-upgrade keeps working.
- **Vulkan coexistence.** The loader enumerates every ICD, so a guest with both venus and the
  limina ICD shows two devices of one GPU. The enhanced tier must make ours the one applications
  pick (device ordering, or not installing the venus ICD where ours is present).
- **Versions.** A Flatpak extension (below) does not move with the guest's limina version, so the
  drivers must refuse cleanly, falling back to stock, when the module or host is older or newer
  than they speak. The wire protocol carries a version; the module's ioctls do not yet.

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

- **The transport stays out of virglrs.** The replayer remains its own process; interop is a
  resource-level matter, not a shared renderer.
- **The currency is the IOSurface, handed over by Mach port**, as worker → supervisor surfaces
  already are (`docs/graphics.md` §4). Only IOSurface-shareable formats in LINEAR layout cross
  transports (host KK implements `VK_EXT_image_drm_format_modifier` LINEAR-only over exactly
  those). Heap-placed images in KK's own layout stay within the Metal transport.
- **The worker keeps the registry**: virtio-gpu resource id → its IOSurface, whichever renderer
  made it. virglrs already registers its surfaces (`metal::republish`); extension blobs that are
  IOSurface-backed join the same registry.
- **Metal transport → vrend/venus:** an image exported for sharing outside the transport is an
  IOSurface-backed dedicated allocation; virglrs imports it as vrend (`EGL_IOSURFACE_LIMINA`) and
  venus already import IOSurfaces.
- **vrend/venus → Metal transport:** the worker hands the resource's IOSurface port to the
  replayer, which makes a texture over it (the replayer's IOSurface import is to be written).

To verify before building: that an IOSurface's pages can back a guest-visible mapping of an
extension blob the way venus's IOSurface blobs do (rutabaga's Apple `get_map_ptr` path).

## 4. Fences across transports

The module's fences (`DRM_IOCTL_VIRTGPU_LIMINA_FENCE`) carry Metal-transport work to every guest
`dma_fence` consumer. The other direction, a Metal-transport queue waiting on a guest fence
(another transport's, or an imported sync_file), belongs in the module too: a `dma_fence` callback
that, when the fence signals, has the host set the waiting context's shared event, so the GPU
queue's wait releases without a guest thread blocking. Until then a sync_file import into KK waits
on the CPU.
