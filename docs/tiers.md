# limina GPU rendering tiers

**Single source of truth for limina's three GPU rendering tiers.** If anything here
disagrees with older notes (e.g. roadmap text that once called virgl "parked"), **this
document wins** — update the other note, not this one.

limina presents the guest with a **single coexist GPU device**. On top of an always-present
software-2D scanout, the host enables *both* venus (Vulkan) and vrend (GL). Which tier
actually renders a given guest depends on what the guest's userspace selects and what its
kernel page size allows. Tiers degrade into one another automatically and additively:
**venus → virgl → llvmpipe**.

## TL;DR — the tier ladder

| Tier | Renders via | Guest needs | Host needs | Page size | Status |
|---|---|---|---|---|---|
| **1 · software-2D** | llvmpipe (guest CPU) → CPU scanout | nothing (any stock guest) | libkrun (patch 0001) | any | ✅ shipped |
| **2 · virgl** | guest virgl Gallium → host **vrend** GL → zink-on-KK → Metal | stock mesa virgl driver (the default for virtio-gpu GL) | virglrenderer **vrend** + KosmicKrisp + EGL-no-GBM patch | 4k or 16k | ✅ validated: accelerated on stock 4k (reaches the GPU); perf is **workload-dependent** — beats llvmpipe on draw-heavy WebGL, loses on upload-heavy glmark2 (see Performance) |
| **3 · venus** | guest **zink** → **venus** Vulkan → host KosmicKrisp → Metal | 16k kernel + venus mesa RPM + zink env | virglrenderer **venus** + KosmicKrisp | **16k only** | ✅ shipped (RPM-delivered, pixel-verified) |

- **GL-only vs Vulkan:** tier 2 (virgl) is **GL-only**. Tier 3 (venus) does **both** GL (via
  zink→venus) *and* native Vulkan. Tier 1 is the CPU floor under everything.
- **Two-tier guarantee** (see `CLAUDE.md`): tiers 1–2 are the **stock baseline** — a fresh
  guest with *no* limina components gets software-2D always and virgl GL when the host can
  provide it. Tier 3 is the **enhanced tier**, unlocked by installing our kernel + mesa +
  mutter RPMs. Enhancements are additive; a guest that hasn't opted in still boots and runs.

## The foundation: the host coexist device

Everything starts at `crates/limina-vmm/src/krun/mod.rs`. `GPU_COEXIST_FLAGS`
(`mod.rs:102-108`) is the default virglrenderer flag set:

- **`VENUS` (0x40)** — Vulkan passthrough to the host venus backend (KosmicKrisp → Metal).
- **`USE_EGL | USE_GLES | USE_SURFACELESS`** — virglrenderer's own EGL winsys for the **vrend**
  GL path, backed by zink-on-KK. macOS has no GBM, so this needs
  `patches/virglrenderer-vrend-egl-no-gbm-macos`.
- **`NO_VIRGL` is OFF.** It was historically forced *on* because Apple Silicon has no host GL;
  zink-on-KK removed that constraint, so **vrend GL is now enabled** (`mod.rs:90-92`).
- **`RENDER_SERVER` (0x200) + `THREAD_SYNC` + `ASYNC_FENCE_CB`** — render-server-thread model.
  1.3.0 only initializes venus when `RENDER_SERVER` is set; venus fences retire *asynchronously*
  (fence eventfd → sync thread → guest IRQ), without which the guest hangs forever in
  `glFinish` / `vkQueueWaitIdle` (`mod.rs:93-97`).

The software-2D path serves **all** 2D/scanout commands unconditionally; venus + vrend add 3D
contexts on top. Mode precedence (`mod.rs:187-191`):

```
LIMINA_VIRGL_FLAGS=0x..   (power-user override)   >   --gpu-software-2d   >   default coexist
```

**Two-tier safety:** if `virgl_renderer_init` fails, libkrun degrades to software-2D — no panic
(`mod.rs:182-184`). That is what makes "coexist by default" safe: the 3D enhancement is
additive and self-degrading. The worker logs the resolved mode on every boot:

```
virtio-gpu virgl_flags = 0x35b, software_2d = false (coexist = true)
```

---

## Tier 1 — software-2D (llvmpipe)

The compatibility floor. Always available, on any guest, regardless of kernel or installed
components.

- **What renders it:** the guest's CPU. Mesa's llvmpipe rasterizes GL; the firmware/fbcon and
  the desktop's final framebuffer are read out by the host straight from guest CPU memory.
- **Build:** nothing custom. Stock Fedora mesa already ships llvmpipe.
- **Delivery:** in-image — it's just stock mesa.
- **Host side:** libkrun with `patches/libkrun/0001` (the software-2D scanout that serves
  framebuffer pixels from host CPU memory and skips virglrenderer/rutabaga entirely). No
  virglrenderer, no KosmicKrisp needed. The native Metal/Cocoa window
  (`crates/limina-display`) presents the CPU framebuffer with no GPU.
- **When it's used:** (a) forced with `--gpu-software-2d` (the 2D capture oracle; also the
  workaround for the local-Terminal GPU-init *hang*, which graceful degradation can't catch
  because it's a block, not an error); (b) as the automatic fallback when both venus and virgl
  fail to init; (c) any guest GL that doesn't route to virgl/venus.
- **Debug:** worker log shows `software_2d = true`. Pixels via the IOSurface scanout
  (`spikes/venus-draw-probe/iosdump.swift`, needs `LIMINA_GLOBAL_SCANOUT=1`) or window capture.

---

## Tier 2 — virgl (vrend GL → zink-on-KosmicKrisp)

Accelerated **OpenGL for a stock guest**, with no guest-side install. This is the baseline
tier's "fast GL" path and the reason `NO_VIRGL` is off.

- **What renders it:** the guest's stock **virgl Gallium driver** (mesa's `virtio_gpu`/`virgl`,
  the default Gallium driver for a virtio-gpu device) speaks the virgl wire protocol to the host;
  virglrenderer's **vrend** decodes it and runs the GL on **zink-on-KK** (zink translates the
  host-side GL to Vulkan, KosmicKrisp runs that Vulkan on Metal).
- **Why it works on a stock 4k guest:** virgl uses a **copy/transfer** memory model, *not*
  host-visible blob mapping — so it is immune to the 16k/4k page-alignment problem that gates
  venus. A bone-stock Fedora guest (4 KiB kernel, stock mesa) gets accelerated GL here.
- **Build:**
  - *Host:* virglrenderer built with vrend + the EGL-no-GBM macOS patch
    (`scripts/build-virglrenderer.sh`, output `third_party/virgl-prefix`), KosmicKrisp, and the
    zink-on-KK host GL stack. Custom epoxy with EGL (`third_party/epoxy-egl-prefix`).
  - *Guest:* nothing — the virgl Gallium driver is in stock mesa.
- **Delivery:** **host-side only.** No RPM, no guest agent. A stock guest "just uses it" for GL.
- **Trigger:** on by default (the coexist flags). A stock guest's GL apps select the virgl
  Gallium driver automatically for the virtio-gpu device. (On the **enhanced** tier this is
  *overridden*: `GALLIUM_DRIVER=zink` routes GL through zink→venus instead — see tier 3.)
- **Degradation:** if the host GL init fails (e.g. the worker linked Homebrew's virglrenderer,
  which has no render-server — the link trap below), vrend is unavailable and GL falls back to
  llvmpipe (tier 1).
- **Status — validated (2026-06-25).** A bone-stock 4k guest *does* get GL through
  vrend→zink-on-KK on the real M1 Max (`GL_RENDERER: virgl (zink ... Apple M1 Max
  (MESA_KOSMICKRISP))`), pixel-verified on-display. **But** it benchmarks *slower than llvmpipe*
  (56 vs 454) — the copy/transfer model is the bottleneck (see Performance). So virgl is a
  zero-install *compatibility* GL path, not a performance tier as-is.
- **Debug:** in the guest, `glxinfo -B` / `GL_RENDERER` must NOT say `llvmpipe`; worker log
  should show vrend GL context creation, not a venus-only init. Beware the `GL_RENDERER` env
  trap (below).

---

## Tier 3 — venus (Vulkan → KosmicKrisp)

The full enhanced tier: accelerated **GL *and* Vulkan**. Pixel-verified end-to-end on a
pristine F43 (2026-06-25).

- **What renders it:** the guest's **zink** routes GL to Vulkan; **venus** is the guest Vulkan
  driver (`libvulkan_virtio.so`) that forwards Vulkan to the host; virglrenderer's venus
  backend hands it to **KosmicKrisp**, which runs it on **Metal**. Native guest Vulkan apps go
  straight through venus.
- **Why 16k is mandatory:** venus uses `RESOURCE_MAP_BLOB` → `hv_vm_map`, which requires the
  host addr, guest addr, and size to all be 16 KiB-aligned. The host is 16 KiB pages (Apple
  Silicon); a stock 4 KiB guest packs blobs sub-page → can't map them independently → venus
  init fails. A **16 KiB guest kernel** makes blobs 16 KiB-aligned → mappable. (See
  `docs/research/03-graphics-virtio-gpu-3d.md`.)
- **Build (all three are RPMs):**
  - 16k kernel → `scripts/build-kernel-rpm.sh` (`CONFIG_ARM64_16K_PAGES`, modules + dracut
    initramfs + BLS entry, co-installs beside stock).
  - mesa 26.2 (zink + venus ICD) → `scripts/build-mesa-rpm.sh` (rebuilds the Fedora mesa SRPM
    with our snapshot pre-patched; ships `zink_dri.so` + `libvulkan_virtio.so`[venus] +
    `libvulkan_lvp.so`[lavapipe]).
  - patched mutter → `scripts/build-mutter-rpm.sh` (the target distro's mutter version + our
    rebased `patches/mutter/*`).
  - *Host:* virglrenderer with venus + KosmicKrisp (with `patches/kosmickrisp/0001-0003`),
    bundled into `limina.app` by `scripts/build-app.sh`.
- **Delivery:** RPMs replacing stock at `/usr`, applied by `scripts/provision/install-enhanced.sh`
  inside a stock guest (the validated path — see `docs/images.md`, memory `limina-enh-delivery`).
  Mesa + kernel are `dnf versionlock`ed; mutter tracks the distro's gnome-shell version.
- **Why RPM-replace, not a sysext overlay:** enhanced mesa 26.2 has a different `libgallium`
  soname than stock 25.x; an overlay can only *shadow* (not *remove*) the stock lib → a
  25.x⊕26.2 ABI blend breaks mutter's KMS EGL. An RPM removes/replaces the old soname. (The
  retired sysext builders live in `scripts/archive/`.)
- **Trigger / upgrade:** install the three RPMs → write the zink-selection env
  (`/etc/environment.d/90-limina-zink.conf`: `GALLIUM_DRIVER=zink`,
  `MESA_LOADER_DRIVER_OVERRIDE=zink`, `VK_DRIVER_FILES=<venus ICD>`, `VN_PERF=no_fence_feedback`)
  → GRUB default to the 16k kernel → reboot. venus then enumerates as
  `Virtio-GPU Venus (Apple M1 Max)`.
- **Degradation:** on a 4 KiB kernel or with venus mesa absent, venus init fails → GL falls back
  to virgl (tier 2) or, if the venus ICD is selected but can't init, to lavapipe/llvmpipe. The
  VM still boots and the desktop is usable. **The enhanced image is still safe to boot even if
  the 16k kernel isn't selected** — it just comes up degraded.
- **Debug:** `vulkaninfo --summary` shows `Virtio-GPU Venus`; the seated desktop renders (the
  `MESA: warning: ... VK_EXT_depth_clip_enable` line is venus's signature). Pixel-verify via
  `iosdump` (`LIMINA_GLOBAL_SCANOUT=1`). `LIMINA_KK_STATS=1` for per-second draw/encoder counts.

---

## Tier selection & graceful degradation

Selection is **granular and additive**, not a monolithic tier switch (`CLAUDE.md` §two-tier).
A guest may have *some*, *all*, or *none* of the enhanced pieces; the host tolerates any mix.

**Host (what the device offers):** `crates/limina-vmm/src/krun/mod.rs` — coexist flags by
default; `--gpu-software-2d` forces the floor; `LIMINA_VIRGL_FLAGS` overrides for experiments.
The host can't pick the guest's page size before boot, so it always offers the full coexist
device and lets the guest+kernel decide what initializes.

**Guest (what userspace selects):** `scripts/provision/install-enhanced.sh:75-90` writes the
zink/venus selection env on the enhanced tier. With *no* env (stock guest), GL takes the virgl
Gallium driver (tier 2) and Vulkan has only lavapipe.

**The degradation chain (best available wins):**

```
venus (16k + venus mesa + zink env)         ─┐  Vulkan + GL, full accel        ← tier 3
   │ venus init fails (4k kernel / no mesa)  │
   ▼                                          │
virgl/vrend GL (stock guest, host vrend OK)  ─┤  GL only, accelerated          ← tier 2
   │ host GL init fails (link trap, etc.)     │
   ▼                                          │
llvmpipe (guest CPU)                         ─┘  GL on CPU, always works        ← tier 1
```

**Capability signal:** the guest agent reports `pagesize` + `caps` in its `Hello`
(`crates/limina-proto`), and the supervisor logs them (`crates/limina/src/control.rs`). Host
policy can branch on these (today mostly informational).

---

## Debugging — traps that have cost real hours

- **The virgl link trap.** A plain `cargo build -p limina-vmm` can relink the worker against
  Homebrew's virglrenderer (no render-server) → venus init returns -1 → **silent** degrade to
  software-2D. `build.rs` prepends `third_party/virgl-prefix` to `PKG_CONFIG_PATH` and prints
  the resolved lib. **Verify:** `otool -L target/debug/limina-vmm | grep virgl` must show
  `third_party/virgl-prefix`. (`scripts/check-virgl-link.sh`.)
- **The `GL_RENDERER` env trap.** SSH shells do *not* inherit the session's `environment.d`, so
  a GL probe over ssh can silently run on llvmpipe even when the desktop runs on venus. Always
  check `GL_RENDERER` / `vulkaninfo` to confirm the backend before trusting a number.
- **Pixel-verify, proxies lie.** FPS counters and "no GL error" prove nothing rendered. Read the
  real scanout: `spikes/venus-draw-probe/iosdump.swift <ids>` (needs `LIMINA_GLOBAL_SCANOUT=1`)
  or the window capture. The human looking at the window is the unconfounded oracle.
- **Worker-log signatures:** `virgl_flags = 0x..., software_2d = false (coexist = true)` (3D
  enabled); `degrading to software-2D` / `ComponentError(-1)` after `virgl_flags` (renderer init
  failed — check the link first); `Virtio-GPU Venus` in `vulkaninfo` (venus live).
- **Enhanced-tier env gotchas** (memory `limina-enh-delivery`): the systemd *user* manager caches
  env at first login (only reboot / `loginctl terminate-user` reloads `environment.d`); a
  service-level `Environment=` overrides `environment.d`; **dev-image clones carry leftover
  `~/.config/systemd/user/org.gnome.Shell@wayland.service.d/*.conf` pinning `/opt/mesa-zink`** —
  these crash-loop gnome-shell ("GPU not supported by EGL") on a clone and are pure dev cruft.
  Validate the enhanced tier on a *pristine* base (`Fedora-Workstation-43.accessible.raw`), never
  a dev clone.

---

## Build & delivery reference (script → tier)

| Script | Produces | Tier |
|---|---|---|
| `scripts/build-virglrenderer.sh` | `third_party/virgl-prefix` (vrend + venus) | 2, 3 (host) |
| `scripts/build-krun-efi.sh` | GOP EDK2 firmware | all (firmware console) |
| `scripts/build-app.sh` | `limina.app` (bundles worker + virgl + KK + epoxy + firmware) | 2, 3 (host) |
| `scripts/build-kernel-rpm.sh` | 16k kernel RPM | 3 |
| `scripts/build-mesa-rpm.sh` | mesa 26.2 RPMs (zink + venus ICD) | 3 |
| `scripts/build-mutter-rpm.sh` | patched mutter RPM | 3 |
| `scripts/provision/install-enhanced.sh` | in-guest installer (RPMs + env + GRUB) | 3 |
| *(stock mesa, in-image)* | virgl Gallium driver | 2 (guest) |
| *(stock mesa, in-image)* | llvmpipe | 1 (guest) |
| `scripts/archive/build-{mesa-zink,mutter}-sysext.sh` | — *(retired; superseded by the RPMs)* | — |

## Performance

Three-tier head-to-head, measured 2026-06-25 (commit `284b758`). Workload: `glmark2-es2-wayland
-b build -b shading -b texture`, run **on-display** (seated wayland window through the full
compositor → scanout → host Metal present path, *not* offscreen/headless — verified by capturing
the scanout mid-run). Each tier on its as-deployed config. Trend rows in `perf/ledger.csv`
(`glmark2-display-*`).

| Tier | Guest | `GL_RENDERER` | glmark2 score |
|---|---|---|---|
| **venus** | enhanced 16k | `zink Vulkan 1.3(Virtio-GPU Venus (Apple M1 Max) (MESA_KOSMICKRISP))` | **2784** |
| **software-2D** | stock 4k | `llvmpipe (LLVM 21.1.2, 128 bits)` | **454** |
| **virgl** | stock 4k | `virgl (zink Vulkan 1.3(Apple M1 Max (MESA_KOSMICKRISP)))` | **56** |

**venus ≫ llvmpipe ≫ virgl** — two findings worth carrying:

1. **venus is ~6× the software floor and the clear winner** — zink→venus reaches the M1 Max with
   zero-copy blob scanout. This is the tier to be on.
2. **virgl reaches the GPU but underperforms the CPU floor (~8× slower than llvmpipe) on this
   workload.** The renderer string proves it runs on the M1 Max (not a fallback), so it is
   *functional*; but its per-frame **copy/transfer** model (vrend reads back the rendered buffer
   and transfers it through the virgl protocol each frame) dominates a geometry-uploading scene
   like glmark2-build, where venus's blob scanout does not. So virgl is a *compatibility* path
   (accelerated GL on a bone-stock guest, no install), **not** a performance middle-tier as-is.
   Whether a transfer/present optimization can lift it is an open question — see the open items.

### WebGL aquarium (a second workload — and it inverts the virgl story)

`webglsamples.org/aquarium` in fullscreen Firefox kiosk, **on-display**, fps read off the page's
own counter from the captured scanout. Static scene geometry, draw/fill-bound (the opposite shape
to glmark2-build's per-frame geometry upload). Trend rows in `perf/ledger.csv` (`aquarium-*`).

| numFish | software-2D (llvmpipe) | virgl | venus |
|---|---|---|---|
| 5 000 | 17 | 37 | **60** (vsync-capped) |
| 10 000 | — | 28 | 57 |
| 15 000 | — | 22 | 45 |

**The ranking flips for virgl: here virgl (37) *beats* the llvmpipe floor (17), ~2.2×.** Because the
scene's geometry is static, virgl isn't re-uploading/transferring vertex data every frame the way
glmark2-build forces it to — so its GPU acceleration shows through instead of being eaten by the
copy/transfer model. **Takeaway: tier ordering is workload-dependent** — virgl < software on
upload-heavy GL (glmark2-build) but virgl > software on draw-heavy WebGL (aquarium). venus wins
both decisively and is the only tier that holds ~vsync (60→57→45 as fish scale 5k→15k).

**Exact launch command (over ssh) — this is the proven, non-flailing recipe (see
[[limina-profiling-playbook]] for the why):**
```bash
ssh -p 2222 claude@127.0.0.1
export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
systemctl --user stop ff-bench 2>/dev/null; sleep 3      # clear any prior workload (NOT mid-capture)
busctl --user set-property org.gnome.Shell /org/gnome/Shell org.gnome.Shell OverviewActive b false
systemd-run --user --unit=ff-bench \
  --setenv=WAYLAND_DISPLAY=wayland-0 --setenv=MOZ_ENABLE_WAYLAND=1 \
  --setenv=MOZ_DISABLE_GPU_SANDBOX=1 --setenv=XDG_RUNTIME_DIR=/run/user/1000 \
  /usr/bin/firefox --kiosk "https://webglsamples.org/aquarium/aquarium.html?numFish=5000"
# wait ~25s, then: swift spikes/venus-draw-probe/iosdump.swift $(seq 1 200)  (with LIMINA_GLOBAL_SCANOUT=1)
# the aquarium-fullscreen scanout is the nonzero=1024000 one; Read the PNG — fps is top-left.
```
`MOZ_DISABLE_GPU_SANDBOX=1` is **mandatory** on the GPU tiers — without it Firefox's GPU process
can't reach the virtio-gpu device and **no window ever maps** (it cost a long debugging session;
on the software tier it isn't needed, which is exactly what masked the cause). `procs=1` is normal
with it. Change fish count = `systemctl --user stop ff-bench` then relaunch a new `numFish`.

**Caveats:** one workload (glmark2), one host (M1 Max, 32 GB), dev-machine variance applies —
these are directional, not gospel (`perf/README.md`: the ledger is a trend, not a gate). The
software floor here is a *GL client* forced to `LIBGL_ALWAYS_SOFTWARE`; in pure software-2D mode a
GL client otherwise fails to acquire a context (it tries the absent virtio-gpu native-context/vdrm
path rather than falling back to llvmpipe) — the desktop compositor itself runs on `swrast`.
