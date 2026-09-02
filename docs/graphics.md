# Graphics: render, present, and the tier ladder

**Scope.** Everything between a guest draw call and a pixel on the host window: the virtio-gpu
device, the three rendering tiers (software-2D / vrend GL / venus Vulkan), KosmicKrisp, blob
mapping, IOSurface scanout and present, the host GPU-memory budget, and the pitfalls that keep
catching people. Display *identity and geometry* — EDID, hotplug, display modes, runtime resize,
cutouts, fullscreen and input policy — are a different subsystem and live in
`docs/design/{stable-edid-hotplug,display-modes,runtime-display-resize,display-cutouts,fullscreen-pointer-grab}.md`.

**Provenance.** Every claim below was re-derived from booted VMs on 2026-08-16 rather than carried
forward from older notes; the measurements and the falsified claims they replaced are in
`spikes/graphics-doc-audit/RESULTS.md`. Four guests were involved, deliberately: enhanced and stock
F44 for the tiers, the **synoik** Vulkan-compositor image for the venus present path (a GNOME guest
composites on vrend and structurally cannot reach it), and an enhanced guest forced onto
software-2D for the floor differential. Where this document and an older one disagree, this one is
right — that is why the older ones were deleted.

---

## 1. The stack in one picture

```
guest app ──GL──► mesa virgl (gallium)  ─┐
guest app ──VK──► mesa venus (libvulkan_virtio) ─┐
                                                 │  virtio-gpu (one device, coexist)
                          ┌──────────────────────┴──────────────────────┐
                          ▼                                             ▼
                 virglrenderer / vrend                        virglrenderer / vkr (venus)
                 (host-side GL decode)                        (host-side Vulkan decode)
                          │                                             │
                          └──► zink ──────────► KosmicKrisp ◄───────────┘
                                                    │
                                                  Metal
                                                    │
                                      IOSurface ──► limina supervisor ──► CAMetalLayer
```

One host device serves both paths. `libvulkan_virtio.so` in the guest is venus; the guest's GL
goes out as a virgl command stream and is decoded host-side by vrend, which runs on zink over
KosmicKrisp. So **both** tiers ultimately land on KosmicKrisp → Metal; they differ in where the
API boundary is crossed.

**KosmicKrisp (KK)** is Mesa's Vulkan-on-Metal driver and the **sole** host Vulkan backend.
MoltenVK was retired as a venus backend on 2026-06-13 (it crashed the compositor); the archived
instrumentation lives in `spikes/archive/moltenvk/`. KK currently advertises **Vulkan 1.4** (since
the MTL4 rebase, 2026-08-05).

## 2. The host device: coexist by default

The worker builds one virtio-gpu device with `GPU_COEXIST_FLAGS`
(`crates/limina-vmm/src/krun/mod.rs`):

```
VIRGLRENDERER_VENUS | USE_EGL | USE_GLES | USE_SURFACELESS
                    | THREAD_SYNC | USE_ASYNC_FENCE_CB | RENDER_SERVER      = 0x35b
```

`NO_VIRGL` is **off** — that is what "coexist" means: venus and vrend are both live on the same
device, and the guest picks per-API. This is the default and there is no reason to leave it.
`--gpu-software-2d` clears the whole set (see §3.1); it is a probe mode, not an alternative.

The worker logs the resolved mode at `info` level and defaults to `warn`, so a normal boot prints
nothing. To see it:

```sh
RUST_LOG=limina_vmm=info …    # "virtio-gpu virgl_flags = 0x35b, software_2d = false (coexist = true)"
```

### The link trap (read this before diagnosing any GPU bug)

The worker **must** link `third_party/virgl-prefix/lib/libvirglrenderer.*`. Homebrew ships a
virglrenderer with no venus render-server support; if the worker picks that one up,
`virgl_renderer_init` returns −1 and the GPU **silently degrades to software-2D**. The VM still
boots, 2D and ssh work, and venus simply never enumerates — which reads exactly like a venus bug
and has burned hours. `build.rs` now prepends our prefix to `PKG_CONFIG_PATH` and prints a
`cargo:warning` naming the resolved library, so a plain `cargo build` is safe and a wrong link is
loud. Verify anyway:

```sh
otool -L target/debug/limina-vmm | grep virgl     # must show third_party/virgl-prefix/…
```

If a worker log shows `degrading to software-2D` or `ComponentError(-1)` right after `virgl_flags`,
check the link before suspecting anything else.

## 3. The three tiers

### 3.1 Software-2D — the compatibility floor, and the GL-less-host path

`--gpu-software-2d` gives the guest a virtio-gpu with no 3D capability at all: no venus, no vrend,
no host GL, no KosmicKrisp. Everything renders on llvmpipe in the guest. This is the two-tier
guarantee's actual floor, and it is guarded:
`crates/limina-test/tests/boot.rs::fedora_stock_image_software_2d_floor_renders_desktop` boots a
**stock** Fedora guest on this device and asserts a usable GNOME desktop paints. It is also what
the capture oracle (`--display-capture`), the EFI image-prep script, and ISO/GRUB boot work all run
on, since none of them can assume a working host GL.

**But an *enhanced* guest does not come up on it, and that is our doing, not software-2D's.** The
enhanced image's `/etc/environment.d/90-limina-zink.conf` pins
`MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` + `GALLIUM_DRIVER=virgl`, so mesa is forced onto a driver
that requires the 3D device. With no 3D device, gbm has no driver behind `/dev/dri/card0` (which
still exists, and `drm_info` still reports a full mode list), and mutter dies in a loop:

```
libEGL warning: egl: failed to create dri2 screen
Failed to open gpu '/dev/dri/card0': … Failed to create gbm device: No such file or directory
Failed to setup: No GPUs found
org.gnome.Shell@gdm.service: Failed with result 'protocol'
Gdm: GdmLocalDisplayFactory: maximum number of display failures reached. Giving up.
```

Measured 2026-08-16 with a clean differential: on the same image and the same device, moving that
one file aside and restarting gdm brings a greeter up on `seat0` immediately. So the cause is the
unconditional driver pin, not the device.

**That combination is an unsupported configuration — wontfix (user's call, 2026-08-16).** The
enhanced tier exists to use the 3D device; running it against a device that has none is not a
scenario limina serves. The floor that must work on a GL-less host is the **stock** tier, and it
does. Do not "fix" the override to make this case boot — the diagnosis is recorded here only so the
next person who sees `Failed to setup: No GPUs found` on an enhanced guest recognises it in seconds
instead of chasing gbm.

Software-2D is never the right answer to "venus is misbehaving", and it is not removable — the
compatibility floor is tested through it.

### 3.2 vrend — accelerated GL, on any guest, with nothing installed

A bone-stock Fedora 44 guest (stock 4 KiB kernel, stock mesa, no limina components, no limina
environment) gets hardware-accelerated GL out of the box:

```
OpenGL renderer string: virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))
```

This is the compatibility floor doing its job, and it is why **GL rides vrend on both tiers**. The
enhanced session runs the same path — guest-side zink-on-venus for GL was dropped as a supported
configuration on 2026-08-04. `gnome-shell` in an enhanced guest maps `libgallium-*.so` and no venus
ICD at all.

vrend also owns the scanout (§4), so zero-copy present is a host-side property available to *both*
tiers, not an enhanced-tier feature.

#### An imported dmabuf: how a software-decoded video frame reaches the GPU

A frame that a CPU decoder produced lives in guest memory, and GStreamer's `glupload` hands it to
GL by wrapping the memfd with `/dev/udmabuf` and PRIME-importing it into virtio-gpu. On a 16 KiB
guest that is the *default* uploader, so it is the path every software-decoded video takes;
a 4 KiB guest picks a different uploader, which is why the page size looks like the trigger.

Two things have to happen for the host to be able to sample it:

1. **The guest must say what the bytes are.** The import registers the pages as a
   `VIRTGPU_BLOB_MEM_GUEST` blob, but pages alone are not an image — format, size, stride and
   plane offsets travel separately, in `VIRGL_CCMD_PIPE_RESOURCE_SET_TYPE`. Mesa emits that only
   when `RESOURCE_INFO` reports a nonzero `blob_mem`, and only for plane 0.
2. **The host must be able to read them.** On Linux vrend imports the dmabuf and the texture
   aliases the guest pages. **macOS has no dmabuf**: `fd_type` can never be `DMABUF` here, so vrend
   copies instead — it keeps the iovecs and the plane layout and re-reads them before every command
   batch that samples the texture. Planar YUV is converted to RGBA on the way in (BT.601 limited
   range, the EGL default), and NV12/NV21/IYUV/YV12 are advertised **sampler-only** so the guest
   stops taking gallium's per-plane "lowered" path — which virgl cannot express, because a sampler
   view carries only a format and I420's two chroma planes are identical in format and size.

An untyped resource is not in the render context's resource hash, so `CREATE_SAMPLER_VIEW` on it is
rejected as an illegal resource — and `vrend_report_context_error` poisons the context
*permanently*. That is why the symptom is not a wrong-looking video but a player whose window never
paints at all: its GL context dies at the first frame. If you see `Illegal resource` followed by
`failed to dispatch CREATE_OBJECT`, look for a missing SET_TYPE, not for a rendering bug.


### 3.3 venus — Vulkan, and the one thing that needs the enhanced guest

venus is the Vulkan side only. In an enhanced guest:

```
$ vulkaninfo --summary
GPU0: Virtio-GPU Venus (Apple M1 Max),  driverName = venus
```

**venus needs the VM's stage-2 granule to be no coarser than the guest's page size.** Every
guest-physical address, size and offset handed to `hv_vm_map` must be a multiple of that granule,
and a guest packs host-visible blobs back to back in one arena — so a 4 KiB guest's second blob
starts 4 KiB in, and a 16 KiB granule cannot name that address:

```
hv_vm_map failed: ret=0xfae94003 … guest=0x280021000 size=0x100000
                  (granule=0x4000 host%g=0 guest%g=4096 size%g=0)
ResourceMapBlob -> ErrUnspec
```

macOS pins the granule to the host page size — 16 KiB on Apple silicon — **unless asked otherwise**.
`hv_vm_config_set_ipa_granule` (macOS 26+) asks otherwise, and limina creates every VM at 4 KiB by
default (`[hardware] ipa_granule`, *Memory pages* in the Configure sheet). That is the whole of the
fix: measured on a stock Fedora 44 guest — stock kernel, stock Mesa, no limina components — venus
enumerates and `vkcube` runs (`spikes/hv-ipa-granule/RESULTS.md`).

So **there is no page-size tier boundary in the graphics stack any more.** 16 KiB pages remain the
enhanced tier because they are faster — the finer granule costs 4-8% on guest CPU-bound work and
nothing where the work is GPU-bound (`perf/2026-08-27-ipa-granule.md`) — and a guest whose owner
sets `ipa_granule = "16k"` gets that back. It is a speed setting now, not an entry fee.

Everything that was built to manufacture an aligned lattice from the guest side — Mesa rounding
blob sizes, the `guest/virtio-gpu-dkms` node-alignment module, the negotiated
`VIRTGPU_PARAM_BLOB_ALIGNMENT` chain — is retired unbuilt or reverted; `docs/design/16k-page-requirement.md`
keeps the analysis. Two host-side pieces survive them, both in virglrenderer: host-visible
allocations are padded to 64 KiB (a guest `PAGE_ALIGN`s its blobs, so an allocation must be at
least one guest page — removing this segfaults `vkcube` on a stock guest), and a blob larger than
its allocation is refused rather than published.

#### Open: stock-tier Vulkan is dead, not degraded

A latent violation of the two-tier guarantee: whenever venus fails, its failure does not degrade —
it takes the whole Vulkan loader down with it, so a guest that should have fallen back to llvmpipe
is left with no Vulkan at all. The granule fix removed the failure that used to trigger this on
every stock guest, which demotes it from a live defect to a trap waiting for the next venus
failure — the amplifier itself is untouched:

```
$ vulkaninfo
vkCreateInstance failed with ERROR_OUT_OF_HOST_MEMORY
```

Isolating the ICDs proves the loader is the amplifier: `VK_DRIVER_FILES=…/lvp_icd…` alone
enumerates llvmpipe perfectly; `…/virtio_icd…` alone produces the OOM.

The fix exists — `patches/mesa-guest/0003-venus-degrade-to-the-stub-instance-when-ring-setup-f.patch`
does exactly this — but it is in **our guest series**, which by definition a stock guest does not
have. Closing it means getting that patch upstream so every future distro mesa carries it; it is
item 1 of the shopping list in `docs/design/16k-page-requirement.md` and it is tracked in
`docs/upstreaming/ledger/mesa.md`.

### 3.4 Detecting tiers: additively, never as one switch

A guest may have some, all, or none of the enhanced pieces, and partial states are normal (a guest
mid-upgrade, or one that installed only part). Light up each feature when *its own* prerequisite is
present — the limina mesa for the venus WSI fixes, the agent for its own features — rather than
gating everything on a monolithic "enhanced" flag.

## 4. Scanout and present: IOSurface is the macOS dmabuf

There is no dma-buf on macOS. The equivalent currency is an **IOSurface**, and the whole present
path is built on it.

**vrend renders directly into the display IOSurface.** The scanout surface is wrapped as an
`EGL_IOSURFACE_LIMINA` EGLImage, so vrend's framebuffer *is* the surface — no readback, no blit.
The worker says so on every boot, on both tiers:

```
iosurface scanout: 2560x1440 PIPE_FORMAT_B8G8R8X8_UNORM EGL-backed (IOSurface id N),
                   renders land in the surface directly
```

**venus presents via `SET_SCANOUT_BLOB` + `present_surface`**, importing the guest's image as an
`MTLTexture` over the IOSurface rather than copying it.

**Worker → supervisor** hands over the surface by Mach port, not as a global IOSurface id. Global
scanout still exists as an opt-in (`LIMINA_GLOBAL_SCANOUT=1`) purely so the cross-process pixel
oracle can find the surface; do not rely on it in normal operation.

Any older statement that present is "a full-frame CPU readback per flush", that `SET_SCANOUT_BLOB`
panics, or that "there is no zero-copy scanout of a GPU texture" describes the pre-KosmicKrisp
stack and is wrong.

**The guarantee a guest may build on: presenting a surface id keeps it resolvable, and a dropped id
self-heals.** The supervisor holds Mach-published surfaces in a bounded, evicting store
(`SurfaceStore`, `crates/limina/src/window/present.rs`), and a non-global surface it drops cannot be
recovered from that side — `IOSurfaceLookup` fails by design, and only the worker can mint a port
for one. Two things make that safe: an id the guest is currently presenting is **never evicted**,
and a failed resolve asks the worker to re-publish (`resurface <id>` → the registry every publish
populates), which costs one frame. A guest therefore does **not** need to re-create swapchain
buffers on a transition to keep its scanout alive; a compositor that does so is working around a
bug that no longer exists. The cap is a memory bound on client transients, not a budget guests
share. History: `spikes/scanout-blob-freeze/RESULTS.md`.

### More than one display

The guest may have several connectors, so every line of the worker→supervisor present protocol
names the **pool slot** it belongs to: `surface <id0> <id1> <w> <h> [<scanout>]`,
`frame <id> [<scanout>]`, and `scanoutgone <scanout>` when a connector goes away. The field is
optional on read and absent means slot 0, so an *old* trace still parses; the worker always
writes it, so a new single-display trace carries a trailing `0` on every line.

Two structural facts follow from virtio-gpu itself and shape everything above:

- **`num_scanouts` is device-config state read once at probe**, so a display cannot be added to a
  running device. Every display a VM may ever show exists from boot as a **disconnected** scanout
  (`--display-pool`, `LIMINA_DISPLAY_POOL`), and "adding a display" is connecting one of those.
- **Connector status is one bool** (`DisplayControl { connected }`), and the EDID *is* the mode
  list. So identity, mode and presence are all pushes on a slot that already exists.

The device and both compositors were never the constraint here: an idle pool is inert, and a spare
slot connects at runtime into a real second monitor — **on the stock tier too**, with Fedora's own
kernel and no limina components. Multi-display needs no agent, no custom kernel and no venus.
Measurements: `spikes/scanout-pool/RESULTS.md`.

#### A panel owns a slot, and the pool is arranged in two phases

**The slot index IS the connector name.** The guest driver creates its outputs in index order at
probe (`third_party/linux/drivers/gpu/drm/virtio/virtgpu_display.c:284`), so scanout *i* is
`Virtual-(i+1)` for the life of the boot. mutter identifies a monitor by connector **and**
vendor/product/serial together (`meta_monitor_spec_equals`,
`third_party/mutter/src/backends/meta-monitor.c:141`), so the same panel arriving on a different
slot is a *different* monitor with its own saved arrangement — exactly as moving a real display
from DP-1 to DP-2 would be. Slots are therefore assigned per host panel, first-come lowest-free,
and persisted per VM (`display_slots` in the VM state), so the guest sees the same monitors on the
same connectors every run. The primary window follows its panel's slot; every other connected slot
gets its own window, slot 0 included — no index is permanently special.

That collides with the firmware, which can only paint head 0: edk2's `VirtioGpuDxe` hardcodes it
(`OvmfPkg/VirtioGpuDxe/Gop.c:66,447,480`). So the pool is arranged in two phases, and the
boundary is **observed, not declared**: firmware never issues `VIRTIO_GPU_CMD_GET_EDID`, and every
Linux connector probe does, so the first one is an unambiguous "the OS driver has the GPU". The
device reports it to the display backend (`krun_display_guest_driver_ready_fn`) and the supervisor
sees a `guestdriver` line. Before it, slot 0 is the only connector touched; after it, the whole
pool is arrangeable. This needs nothing from the guest — no agent, no custom kernel — so it holds
on the stock tier by construction. A guest reboot returns to the firmware phase (libkrun is
single-shot: the worker is torn down and the fresh one starts in firmware); an in-process resume
does not, because the restored guest's driver is already up.

**Who picks a connector's resolution.** The display mode (`host` / `dynamic` / a fixed size) is
the *window's* policy and reaches only the connector the window is on: `host` drives the guest to
that panel, `dynamic` and fixed push the window's own content size instead. Every other connector
is driven to its panel's full size regardless of mode — its window fills that panel and cannot be
resized, so nothing else would ever set it, and a sizeless connect leaves the guest advertising a
stale preferred mode with a physical size derived from it. The rule lives in
`hostdisplay::drives_size`.

**Per-monitor configuration survives the cycle**, which is what the per-panel slot assignment
exists to buy. Measured on the two-panel rig, dynamic mode: an arrangement set while fullscreen
across both panels comes back on the next fullscreen *and* across a full VM restart, mutter
matching its saved `monitors.xml` entry on connector plus the panel's real vendor/product/serial.

**Switching a display off.** A slot carries the user's standing intent (`enabled`) as well as the
guest's current state (`connected`), and the planner ANDs them — so the Displays menu's rows are
the same connector cycle as an unplug, planned in the same place. Intent is keyed on the **panel**
and persisted as `display_disabled`, because it has to outlive the assignment: an unplugged panel
has no slot to carry the decision. The menu lists every attached panel rather than every assigned
slot (a panel earns a connector only when something wants to show it), and the row for the panel
the window is on is checked and dead.

**A window's lifetime is the TABLE's decision, never `scanoutgone`'s.** A guest disables a scanout
and reconfigures it for every ordinary modeset — simpledrm → plymouth → gdm on the way up, and
again at every session handover — and the host sees the identical `scanoutgone` either way, so the
message cannot tell "between modes" from "gone". A slot with no geometry therefore keeps its
window: it holds its panel, its style and its fullscreen, and shows its last frame until the new
mode's first present replaces it, exactly as a monitor does across a mode change. Only the slot
table takes a window down (`windows::slot_fate`, unit-tested). Closing on the dark slot instead
cost a fullscreen secondary its Space at every logout, and the re-entry meant to give it back is
a `toggleFullScreen` that does not always land.

Not done yet:

- **The cursor.** The guest's cursor commands name a scanout
  (`virtio_gpu_cursor_pos.scanout_id`) but libkrun's display C ABI drops it, so the sprite rides
  the primary window; plumbing it is a fork ABI change.
- **Keyboard reaches the guest through the primary window only.** A secondary is never made key
  — one that took key focus would swallow every keystroke while still showing pixels — so the
  keyboard has one owner. The *pointer* is per-display: each window resolves to the slot it shows
  and maps into that display's share of the absolute range through the guest's own layout
  report (`window/arrangement.rs`).
- **A covered panel outranks everything on it.** The cover window is borderless above
  `NSMainMenuWindowLevel`, so while the VM is fullscreen that panel's menu bar and Dock are
  unreachable and there is no reveal gesture for them — the primary's chrome ask serves the
  primary's panel only. macOS's own fullscreen at least reveals the menu bar on a top-edge push;
  this does not.
- **The guest's arrangement is known only when the guest reports it.** `limina-agent-session`
  reports the compositor's own logical rects (enhanced tier) and the pointer maps through them:
  measured 2026-08-18 with a BenQ at guest scale 1.25 beside a built-in at 2.0, a full sweep of
  each window covered its own display edge to edge (0..2047 of 2048, and 4..1512 of 1512) with
  no overshoot. A stock guest reports nothing and the host does not guess: each window maps onto
  the whole range, exact for one display and the documented stock floor for two
  (`docs/input-and-windows.md` §3).
- **User-defined virtual displays.** `SlotSource::Virtual` is a placeholder: the slot model
  accepts one, but nothing constructs it and it carries no `EdidSpec` yet, so there is no
  "Add virtual display…" row.
- ~~A VM resumed into a fresh supervisor process is held to one connector~~ — fixed: the
  restore path queues an empty display update after the GIC restore, whose config-change makes
  the restored guest re-read every EDID and re-fire the firmware→OS handover
  (`limina-vmm/src/krun/mod.rs`; regression-covered in `vrend_session_restore`).

### Format modifiers

KK implements `VK_EXT_image_drm_format_modifier` for real, LINEAR-only, over the
IOSurface-shareable colour formats, with truthful subresource layouts; vkr passes modifier creates
through verbatim, and upstream venus's own passthrough gate does the rest. That is what let the
guest-side modifier fiction (mesa 0010, both halves) be **retired whole** on 2026-08-04 — the
renderer now negotiates honestly instead of the guest driver being patched to compensate. The
`vk_image.h` `drm_format_mod` guard carries `DETECT_OS_APPLE` on the `limina-kk` branch.

Guest-side, the enhanced kernel advertises **no** format modifiers on the KMS side
(`DRM_CAP_ADDFB2_MODIFIERS = 0`, primary plane `XR24` only, cursor `AR24`) — that is deliberate,
post-r9, and unrelated to the Vulkan extension above.

## 4.5 Hardware video decode: VA-API over virgl, VideoToolbox on the host

A **stock** Fedora guest hardware-decodes VP9 with nothing of ours installed. The guest half
already ships: `mesa-dri-drivers` contains `/usr/lib64/dri/virtio_gpu_drv_video.so` (mesa
builds it from `src/gallium/targets/va` whenever `virgl` is in `gallium-drivers`), and libva
selects a driver by DRM driver name — `virtio_gpu`. It talks VA-API over the **virgl command
stream** (`VIRGL_CCMD_*_VIDEO`), so video rides vrend and is independent of which tier the
guest's 3D is on.

The host half is ours: upstream virglrenderer implements its codec backend only against libva,
so `src/vrend/virgl_video_vt.c` in our fork implements the same `virgl_video.h` interface
against VideoToolbox. `src/meson.build` picks one backend by host OS; they are never built
together. Enabled by `-Dvideo=true` (`scripts/build-virglrenderer.sh`) plus
`VIRGLRENDERER_USE_VIDEO` in the worker's virgl flags.

**Nothing gates it.** Caps are negotiated: a Mac with no silicon for a codec advertises none,
the guest's `virgl_get_video_param()` finds no matching entry, and the application falls back
to software. The driver still loads and initialises either way.

### What a stock guest can actually get

Two independent gates, and only their intersection is reachable:

- **Guest.** Fedora builds mesa with the default `-Dvideo-codecs=all_free`, enforced in the VA
  *frontend* (`src/gallium/auxiliary/vl/vl_codec.c`) and therefore driver-independent: AV1,
  VP9, MPEG-2, JPEG. H.264 and HEVC are absent whatever the host offers. RPM Fusion's
  `mesa-va-drivers-freeworld`, or our own mesa RPM built `-Dvideo-codecs=all`, restores them.
- **Host.** VideoToolbox on Apple silicon has no MPEG-2 path at all, and AV1 *hardware*
  decode needs an M3 or later. Measured matrix: `spikes/videotoolbox-caps/RESULTS.md`.
- **Host, software.** The backend carries a dav1d decoder, but only as a repair for frames
  AV1-capable silicon decodes correctly and then hands back wrong — super-resolution
  (`docs/design/av1-decode.md`, `docs/radar/videotoolbox-av1-superres.md`). It is deliberately
  *not* a decoder in its own right: a host with no AV1 silicon advertises no AV1 profile at
  all, so the guest keeps decoding with its own dav1d, which is better tested than ours and
  costs nothing to route through the host (`virgl_video_vt.c`, `fill_caps`).

So VP9 (and MJPEG) everywhere, and **AV1 from M3 on** — below that the guest decodes it
itself, unaccelerated, which is what it would have done anyway. **Implemented today: VP9 profile 0, AV1 main, H.264 (Baseline/Main/High) and
HEVC Main** — the last two enhanced-tier or freeworld only, since the guest's `all_free`
gate keeps them out of a stock driver whatever the host offers
(`docs/design/h264-hevc-decode.md`).

### Traps this path is shaped around

- **The profile enum travels raw.** `virgl_video_caps.profile` and every picture descriptor
  carry the numeric value of mesa's `pipe_video_profile`, and virglrenderer's vendored copy of
  that header must match the guest's. It had drifted, putting VP9_PROFILE0 on mesa's
  JPEG_BASELINE — a host advertising VP9 made the guest publish a `vajpegdec`. Nothing fails
  to build when this goes stale.
- **VideoToolbox's decode callback is ordered-synchronous but on its own thread**, which holds
  no EGL context. GL issued from it is dropped silently and the guest reads a cleared surface.
  Park the picture; deliver it after `DecodeFrame` returns.
- **Decode into the layout the guest allocated.** ffmpeg's VA-API path allocates I420 (three
  planes) decode targets while asking for NV12 elsewhere; VideoToolbox produces either, so ask
  it for the target's own layout instead of converting.
- **Post-processing is a second, separate draw, and it is not covered by testing the decode.**
  Asking libva for the surface's own format is a plain readback; asking for any *other* format,
  or any `scale_vaapi`, routes the frame through mesa's `vl_compositor` on the host. That draw
  failed for reasons entirely unrelated to video, so a perfect decode still reached the guest
  black — which reads as a broken decoder and is why hardware VP9 looked like it was falling
  back to software. Two faults, each on its own sufficient to blacken it, **neither visible
  while the other stands** (fix one alone and the draw is still black, so each looked exonerated
  when tested singly):
  - `vl_compositor` declares a one-dimensional `DCL CONST[0..n]`, which vrend translates to a
    plain `uniform uvec4` array, then supplies the data via `SET_UNIFORM_BUFFER`, which vrend
    only ever binds as a GL uniform block. The two constant-delivery paths do not meet, nothing
    rejects the combination, and the shader's colour-space matrix read as all zeroes.
  - It samples `2D_ARRAY` while `vl_video_buffer` allocates plain 2D planes. Native drivers
    build the descriptor from the view and take only the coordinate count from the shader;
    GLSL cannot, and an incomplete sampler returns `(0,0,0,1)`.

  Both are fixed host-side, so this needs nothing installed in the guest.

  **Bridge the constants from the resource's iov, never by mapping the GL buffer.** The first
  version mapped it on the draw path. It produced correct pixels and was still wrong: a map is
  a synchronisation point, and a render thread parked in one does not answer the quiesce that
  suspend waits for, so the whole suspend bracket hangs — long after the frame it belonged to
  rendered perfectly. Right pixels are not evidence that a draw-path change is safe; the cost
  showed up in a different subsystem entirely, with every frame around it looking flawless. Exonerated on the way,
  each with the lever confirmed live: KosmicKrisp index promotion, zink triangle-fan lowering,
  GL errors and shader compile failures.

- **An RGB → YUV post-processing conversion still comes out with permuted colours**, and that
  one is guest-side: the traced matrix is a textbook BT.709 **YUV→RGB** matrix, and applying it
  by hand to RGB inputs reproduces every measured output exactly (black → `Y=0 U=77 V=0`,
  white → `255/184/255`, red → `48/255/7`). mesa binds the wrong-direction matrix for the
  encode-side pass. Geometry is exact; only the colours are wrong. Nothing we decode uses this
  direction.

- **A separate, real gap, not to be confused with either:** mesa's guest `virgl_video.c` never
  calls `virgl_resource_dirty()` on a decode target, unlike every other host-writes-here path in
  that driver. Still present in mesa main (checked 2026-08-30) and worth upstreaming, but
  patching it changes nothing observable — measured both ways against an unpatched control.
- **The host must never write into guest backing at `ATTACH_BACKING` unless it is restoring content
  the guest cannot have.** The guest kernel queues `RESOURCE_CREATE` + `ATTACH_BACKING` and returns
  the handle to userspace without waiting, so the guest may already be writing through its mapping
  while the host processes the attach. vrend's `PIPE_BIND_CUSTOM` resources (the bitstream buffers)
  carry a zero-filled host shadow, and upstream's attach copies that shadow out unconditionally —
  which erased about one keyframe in a hundred under the copy that had just filled it (whole buffer or
  from an arbitrary offset, then `-12909`). Our fork writes the shadow back only once it holds content
  the backing lacks (`ptr_valid`: set by a detach or by a transfer into an unattached resource). The
  hypervisor-side dead end, and a probe-only hazard found on the way, are in
  `spikes/hv-stage2-write-loss/RESULTS.md`.

Verification: `cargo nextest run -p limina-test -E 'test(stock_guest_hardware_decodes_vp9)'`.
VP9 is a normatively exact codec, so the test asserts the hardware decode is **byte-identical**
to a software one, plus that the frame is not uniform. It also drives the post-processing draw
twice over — a non-native download, and a scaled BGRA image with no codec in it at all. That
second one has to *scale*: a same-format, same-size `scale_vaapi` is serviced as a blit, never
reaches the compositor, and passes on a host where every real VPP draw renders black.

### AV1: what it cost

Implemented; `docs/design/av1-decode.md` is the plan of record. AV1 is the only *other* codec a
stock guest can ask for — the `all_free` gate above keeps H.264 and HEVC out of the guest driver
regardless of the host — and the host half was measured first
(`spikes/av1-vt-probe/`, M3-or-later only: no on M1 Max, yes on M4 Pro). VideoToolbox imposes no obstacle: it owns its DPB as it does for VP9, accepts a
repeated sequence header per temporal unit, and returns a picture for every frame including
the no-show ones — **but only when each frame is wrapped in its own temporal delimiter**. In
a stream's natural framing one picture comes back for a no-show/display pair, so a run that
silently drops a third of the frames reads as a clean 1:1 pass; count in the right unit.

The whole cost was the frame header. virgl hands over tile entries plus a fully
parsed `virgl_av1_picture_desc`, while VideoToolbox wants real OBUs, so the backend must
re-serialize a conformant `OBU_FRAME_HEADER` — plus a sequence header for the `av1C` box,
unlike VP9's six-scalar `vpcC`. There is no prior art: ffmpeg's own VideoToolbox AV1 hwaccel
(`libavcodec/videotoolbox_av1.c`) still holds the original packet and forwards untouched
OBUs. ffmpeg's `cbs_av1` is the one bidirectional writer, and being bidirectional buys the
oracle as much as the bit-packing — a synthesized header can be diffed field-by-field against
one parsed from a real stream, as a unit test with no VM and no GPU. The irreducible part is
a shadow DPB: `primary_ref_frame` inheritance and `global_motion_params()`'s subexp deltas
mean header synthesis needs per-slot state that VP9 let us skip entirely.

One decision to make deliberately rather than discover: **VideoToolbox applies film grain and
returns no grain-free picture** (measured bit-identical to dav1d with grain on). The protocol
carries both `target` and `film_grain_target` and expects a grain-free reference alongside.
Harmless — VT owns the DPB, so decode correctness never needs one — but the backend must fill
the grain-applied picture into whichever surface the guest *displays*, which is not
necessarily `target`. Real AV1 web content leans on grain, so getting this backwards would
present as a corruption bug.

## 5. Host GPU-memory budget

Host memory allocated on the guest's behalf is invisible to the guest, so a guest-side leak ends
with macOS jetsamming the worker and killing the whole VM with no guest-side evidence. The budget
ledger in `third_party/virglrenderer/src/venus/vkr_budget.{c,h}` fixes the attribution: exact-size
per-context accounting (always on) plus an enforced cap that kills the offending context rather
than the VM. The guest is told the cap through `VK_EXT_memory_budget` — the one backpressure
channel the venus transport does not discard.

Default cap: `max(8 GiB, 2 × guest RAM)`; at 8 GiB of guest RAM the worker logs
`limina GPU budget: cap 16384 MiB`. Design, policy knobs, and how to read a refusal:
`docs/design/gpu-memory-budget.md`.

## 5.1 Delivery: how the enhanced graphics stack reaches a guest

The enhanced tier is delivered as **rebuilt Fedora RPMs replacing stock at `/usr`**, versionlocked
— not as a sysext overlay. The reason is specific to mesa: our mesa has a different `libgallium`
soname than the stock one, and an overlay can only *shadow* a file, never remove it, so the guest
ends up with a blended ABI that breaks mutter's KMS EGL. An RPM replaces the old soname outright.
(The retired sysext builders are in `scripts/archive/`.)

Alongside the RPMs, `install-enhanced.sh` writes the driver selection into
`/etc/environment.d/90-limina-zink.conf` (the filename predates the GL flip and was kept):

```sh
GALLIUM_DRIVER=virgl
MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu
VK_DRIVER_FILES=<venus ICD>
VN_DEBUG=mem_budget          # venus only advertises VK_EXT_memory_budget under this gate
```

then sets GRUB's default to the 16 KiB kernel and reboots. `MESA_LOADER_DRIVER_OVERRIDE=zink` /
`GALLIUM_DRIVER=zink` — zink as the *guest's* GL driver — is no longer a supported configuration.

**Two naming hazards in that file, flagged by the synoik guest session (2026-08-16) after one of
them cost them a false claim in their own code.** `VN_DEBUG=mem_budget` reads like a debug flag any
tidy-up would drop, but it is the gate on the *only* backpressure channel the venus transport does
not throw away (§5) — without it `VK_EXT_memory_budget` simply is not advertised, and a guest that
tests for the extension concludes venus lacks it. They measured the difference directly:
`vulkaninfo` lists 1102 lines with `VN_DEBUG` unset and 1105 with it set, the extension appearing
only in the second. And the whole file is named `90-limina-zink.conf` while zink-as-guest-GL has
not been a supported configuration since 2026-08-04. Neither is load-bearing to rename, but both
invite exactly the mistake that was made.

An enhanced image is safe to boot even if the 16 KiB kernel is not the selected one: under the
default 4 KiB granule venus comes up on the 4 KiB kernel too, and under `--ipa-granule 16k` it
fails while GL keeps working on vrend — degraded, not broken, either way. Component versions and
the host-first ordering prerequisite are in `docs/images.md`.

## 6. Performance

**`perf/ledger.csv` is the live record.** Do not maintain a ranking table in prose — the last one
outlived its stack by months and ended up recommending a tier the session no longer ran on.

Two things a reader needs to know before touching a number:

- **The 2026-08-04 GL flip is a discontinuity in the ledger.** The workload named
  `glmark2-wayland-venus` has measured **vrend**, not zink-on-venus, since that date. Rows before
  and after the flip are not comparable; the name was not changed because renaming it would
  falsify the earlier half instead.
- **Both GL tiers hit the frame clock on the local rig.** As of the 2026-08-08 re-measure, the
  WebGL aquarium held a steady 60 fps to 20 000 fish on vrend. A benchmark that returns "60" is
  measuring vsync, not the renderer — push the load until it stops being 60, or you are recording
  the display's refresh rate.

Live analysis worth keeping: `docs/perf/overhead-inventory.md` (wakeups and exits under load — note
the venus-ring poll budget is absent from the shipped GL path, so that lever is worth only what
Vulkan clients cost) and `docs/perf/venus-cmdstream-overhead.md` (where a Vulkan command stream
spends its time crossing the venus boundary).

## 7. Pitfalls

These are the ones that have actually cost days. Most are epistemic, not technical.

**"Out of memory" in this stack is almost never about memory.** `VK_ERROR_OUT_OF_HOST_MEMORY`,
`ENOMEM`, `ResourceCreateBlob -> ComponentError(-1)` — the venus transport has exactly one error
code for "could not get a buffer": `vn_call_*` returns OOM whenever `vn_ring_get_command_reply`
comes back NULL, for any reason. Distinct causes so far: a poisoned context from a slow ring wait, a
host address-space leak (vm regions 3.5k→23.6k, RSS flat), launchd's 256-fd limit on Dock-launched
apps, the budget cap refusing deliberately, and — reported by the synoik guest session on
2026-08-16 — **a guest-side spec violation**: a destroyed `VkImage` still referenced by a recording
command buffer made *every* subsequent allocation return OOM, and the Khronos validation layer
named it in one run after hours of falsified memory-pressure theories. It is a symptom class, never
a diagnosis — and note it cuts both ways: a guest reporting OOM is not evidence of host pressure
until it has run with validation on, and a host seeing OOM should not assume the guest is at fault.
Ask **what
refused the allocation**: read the worker log at the timestamp of the guest symptom — the cause is
usually a different, earlier line there — and check the exhaustible resource that is not RAM
(vm regions, fds, mappings). Consider real memory pressure last.

**The frame can be dropped one process after the one you are instrumenting.** A guest reported
`SET_SCANOUT_BLOB` consumed and ACKed but never applied, on a display frozen while it rendered
correctly at 60 Hz. Everything either side could see was healthy, because the worker *did* apply and
ACK every flip — the **supervisor** discarded the frame afterwards, having evicted the surface from
its bounded store. Six mechanisms were proposed and died before anyone looked in the right process.
Two habits fall out. **A ruling-out is only as good as the population it counted**: "eviction is
ruled out, only 20 distinct ids exist" counted a log line that covers venus scanout imports only,
while the store receives every published surface (41 that run). And **an unlogged exit path is where
the wrong answer lives** — both ways an id could leave the store were silent, which is precisely
why the fault was invisible from the host and unreachable from the guest. The instrument that ended
it makes a failed resolve *name its own cause*.

**A guest can take the whole VM down through the renderer.** Invalid Vulkan usage from the guest
reaches vkr on the ring thread, and four incidents so far turned that into a host-side abort that
killed the VM rather than the offending context: degenerate `vkCmdClearAttachments` rects (×3),
`vkCreateBuffer` size==0, a render-pass format mismatch, and an attachment-less AGX pass with
`defaultRasterSampleCount == 0`. **Fix at the vkr trust boundary every time** — a boundary check
produces a loggable, attributable refusal. Regression tests live in
`crates/limina-test/tests/venus_bad_usage.rs`, one arm per incident.

The mesa/KK asserts that used to *catch* these are **compiled out now**: KK builds with
`b_ndebug=true` (verified 2026-08-16 — the devenv library has zero `assert` symbol references and
zero assertion-failure strings, and the dogfood bundle builds release). Older notes saying "~820
asserts are live, shipping `-Db_ndebug=true` is still TODO" are stale. Note this is not purely good
news: removing the tripwire means an unchecked bad command now runs on into undefined behaviour
instead of aborting loudly, so the trust-boundary checks are no longer defence in depth — they are
the only defence.

**A scary warning is a lead, not a cause.** We nearly "fixed" two non-bugs by reasoning from a log
line. Before acting on a warning, read its emission site in the source we own and see whether it is
even fatal, then confirm by observation. The `[KK-MODIFIER]` warning is the standing example: a
zero-warning width once sheared 8 px per row, so the warning was not the oracle for the bug it
looked like it described.

**Permanent benign boot noise.** On both tiers, every boot: `context N failed to dispatch
CREATE_VIDEO_BUFFER: 22` followed by `vrend_decode_ctx_submit_cmd: context error reported N
"gst-plugin-scan" Illegal command buffer`. The victim is a throwaway probe context. It is not a
regression and does not need chasing.

**Verify premises before deep-diving.** List the assumptions a bug "obviously" rests on and prove
each one; do not inherit them. Two false premises ("the present pipe is broken", "glmark2 18/18
means GL renders") cost hours apiece.

**Pixel-verify; proxies lie.** FPS counters, "no GL error", "18/18 scenes", exit-0 — none of them
prove anything rendered. Read the real pixels: the IOSurface scanout via
`spikes/venus-draw-probe/iosdump.swift`, or the window capture (`LIMINA_WINDOW_CAPTURE`). Not
`glReadPixels`. When only a human can see the window, ask the user to look.

**Identical A/B results across many configurations mean the differential is not reaching the system
under test.** Five consecutive "exonerations" once all returned pixel-identical damage because
every arm ran the same unmitigated stack. Invariance is a smell, not a verdict — stop toggling and
re-verify the baseline.

**Verify the fix is loaded, at the path the process actually maps.** Half a day went to bisecting a
"regression" that was a half-installed fix: the library sat in `/usr/lib64/mutter-17/` while
gnome-shell loaded it from `/usr/lib64/`. A sub-oracle proving one piece is live proves nothing
about the load-bearing piece — check the artifact itself (mtime/size at the path in
`/proc/PID/maps`).

**Instrument the stack we own.** A few `fprintf`s in KK, virglrenderer, or libkrun beat any amount
of outside-in guessing; an instrumented host Vulkan driver loaded via `VK_ICD_FILENAMES` is what
turned "venus renders black" into the single fact that the vertex buffer the GPU fetched was
all-zero. Keep those oracles in the repo.

**Pin the guest display mode and scale before any graphics benchmark.** An unpinned display makes
runs incomparable. (Note the guest-side helper `~/bin/set-guest-display.py` that older docs cite is
**not** on the current enhanced image — pin by hand or ship the script.)

## 8. Verification recipes

### Boot the thing (the default, and almost always the right one)

```sh
cargo xtask run --disk <enhanced.raw>
```

EFI → GRUB → the guest's own kernel, SELinux enforcing, coexist venus, `--window --net`. This is
the configuration that ships, so it is the configuration to test. The disk boots **in place** —
clone it first (`cp -c`) to keep the original pristine. It sets all the host KK/zink environment
for you; a bare `target/debug/limina --window` with none of it aborts on GPU init
(`Couldn't open libEGL.dylib`), which is missing environment, not a coexist problem.

Fringe modes, for when they are the explicit subject and not otherwise: `--kernel` injection
(bypasses GRUB and SELinux; for deterministic test kernels and early-boot debugging) and
`--gpu-software-2d` (§3.1).

### Confirm which tier is actually live

Inside the seated session — the renderer string is what settles it:

```sh
systemd-run --user --wait --pipe --collect --setenv=WAYLAND_DISPLAY=wayland-0 \
    glxinfo -B                     # → "virgl (zink Vulkan 1.4(… MESA_KOSMICKRISP))"
vulkaninfo --summary               # → "Virtio-GPU Venus (Apple M1 Max)", driverName = venus
```

On the current F44 enhanced image a plain non-login `ssh … vulkaninfo` **does** enumerate venus,
and `/etc/environment.d/90-limina-zink.conf`'s variables **are** present in that shell (the user
manager holds them). Older notes said the opposite as an unconditional rule; it is not one. It was
only verified with a seated session up, though, so the durable rule is:

> Print the variable in the shell you are actually using. Treat an *empty* result as "check the
> environment", not as "venus is broken".

Host-side, active `Mesa:` GL errors in the worker log mean venus is rendering.

### Read the pixels

`spikes/venus-draw-probe/iosdump.swift` dumps any IOSurface by id cross-process (needs
`LIMINA_GLOBAL_SCANOUT=1`); `LIMINA_WINDOW_CAPTURE` captures the window. See
`spikes/graphics-doc-audit/RESULTS.md` for the full re-run recipe used to write this document.

### Full validation

`cargo xtask test` (~28 min, needs `dangerouslyDisableSandbox`). Detaching hands you a **fake exit
code** — the launch shell's, not the suite's. Wait on the real pid, then read the log. Never build
or commit while it runs.

## 9. Open items

| item | where |
|---|---|
| **A venus failure kills the whole Vulkan loader** — upstream the stub-instance patch so a stock guest keeps llvmpipe when venus goes down | §3.3, `docs/design/16k-page-requirement.md`, `docs/upstreaming/ledger/mesa.md` |
| **Fence-accurate present is not wired for vrend** — vrend's flush path never reaches `try_park_present`, so `FENCEPRESENT` never fires and the #24 tear/pacing work does not apply to the tier the desktop actually runs on. **No observable symptom, though:** the overview-toggle stress (historically the most tear-prone workload) was human-verified smooth on both present paths on 2026-08-16, so this is a missing mechanism rather than a live defect. Re-open it if tearing is ever reported. | `docs/hardening-backlog.md`, `spikes/graphics-doc-audit/RESULTS.md` row 20 |
| zink reads `heap.size − heapUsage` instead of `heapBudget`, so GL clients do not see our cap | `docs/design/gpu-memory-budget.md` §Known limits |
| Pure-GL guests are unbounded — the cap is only enforced at `vkAllocateMemory` | same |
| Explicit sync: only binary `SYNC_FD` external semaphores exist; timeline/`OPAQUE_FD` do not | `docs/research/venus-explicit-sync-gap.md` (and read §5–6 before chasing `OPAQUE_FD`) |
| Ship or stop citing `~/bin/set-guest-display.py` | §7 |

## 10. Related documents

- `docs/design/16k-page-requirement.md` — the page-size wall, both halves, and the ways out
- `docs/design/gpu-memory-budget.md` — the host GPU-memory ledger and cap
- `docs/design/{venus,vrend}-snapshot-replay.md` — GPU state across suspend/resume
- `docs/design/venus-ring-idle-wakeups.md` — the idle-wakeup work
- `docs/research/venus-explicit-sync-gap.md` — external semaphores and explicit sync
- `docs/images.md` — image inventory and shipped guest component versions
- `docs/perf/`, `perf/` — analysis and the measurement ledger
- `spikes/graphics-doc-audit/RESULTS.md` — the measurements this document was written from
