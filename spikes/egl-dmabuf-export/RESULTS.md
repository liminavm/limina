# eglExportDMABUFImageMESA on virgl: what a consumer may do with the fd

Two oracles for guest-side dmabuf export of an ordinary (non-shared) GL texture, kept from
the dogfood investigation of the garbled GNOME Settings avatar (the reporting session's bundle
lives on the dogfood guest under `~/Projects/backgrounds/virgl-dmabuf-bug/`).

## The rule they measure

On virgl with copy-transfer in both directions every non-shared texture stages: its guest BO is
one page (`alloc_size = 1` in `virgl_resource_create_front`), whatever the resolution. Exporting
such a texture is normal and sound for a consumer that **re-imports** the fd (EGL, or PRIME back
into virtio-gpu): the fd names the host resource, and `virgl_resource_from_handle` marks the
import staged because the guest storage is smaller than the layout. A consumer that **mmaps**
the fd and trusts the geometry reads one page of zeros and then SIGBUSes. That is upstream virgl
behaviour; the guest mesa series does not refuse the export (a refusal that shipped in
`26.1.8-5` through `-9` fired for every staged texture and broke GTK4's readback — see
`spikes/va-dmabuf-size/RESULTS.md`). Video decode targets, the one mmap consumer, get real guest
memory instead.

## `cross.py` — the GTK4 path (the avatar bug)

Renders a source image through one GSK renderer and optionally re-reads it through another,
then saves the result; `gdk_texture_download` of a GL-rendered texture goes through
`eglExportDMABUFImageMESA` and a dmabuf re-import. Run in the seated session of an enhanced
guest (`XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0`):

```
python3 cross.py gl none out-gl.png /path/to/source.png
python3 cross.py gl gl out-gl-gl.png /path/to/source.png
python3 cross.py vulkan none out-vk.png /path/to/source.png
python3 cross.py cairo none out-cairo.png /path/to/source.png   # baseline, no GPU
```

RMSE of each output against the source (0 = pixel-identical), 512x288 source:

| guest mesa | gl none | gl gl | vulkan none | cairo none |
|---|---|---|---|---|
| `26.1.8-9.limina` (refusal in; measured on the dogfood guest by the reporting session) | 0.774 | | | 0 |
| `26.1.8-10.limina` (F44 enhanced.test, delivered r23, 2026-09-02) | 0 | 0 | 0 | 0 |

At `-9` GTK's export "succeeded" with the caller's fd array untouched and the download consumed
an uninitialized fd; at `-10` every GPU path produces a `DmabufTexture` and reads it back
correctly, and Mesa's EGL layer returns `EGL_FALSE` if a driver ever does refuse, so GTK falls
back instead of corrupting.

## `dmabuf_export_test.c` — an mmap consumer, no toolkit

Renders a known colour into an FBO texture, exports it, and compares the **mmapped** dmabuf
against `glReadPixels`. Build in the guest:

```
cc dmabuf_export_test.c -o dmabuf_export_test -lEGL -lGLESv2 -lgbm -ldrm
```

Measured on `26.1.8-10.limina`, 16 KiB guest kernel, 256x256 RGBA:

```
eglExportDMABUFImageMESA returned EGL_TRUE
  fd=7 stride=1024 offset=0
  lseek(SEEK_END) = 16384 (expected >= 262144)
  dmabuf first pixel  : 00 00 00 00
Bus error
```

This is the expected result, not a regression: the fd is a one-page stub and the test maps it.
It documents the boundary; a consumer that needs to map an exported GL texture needs it created
shared, which is what WSI does. At `-9` the same test stopped at `EXPORT FAILED`.
